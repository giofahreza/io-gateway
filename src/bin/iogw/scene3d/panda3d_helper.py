"""Trusted, embedded GPU helper. Receives geometry and visual parameters only."""
import json
import math
import random
import struct
import sys

# Reserve stdout for the binary protocol, including during engine imports.
wire = sys.stdout.buffer
sys.stdout = sys.stderr


def exact(count):
    result = bytearray()
    while len(result) < count:
        chunk = sys.stdin.buffer.read(count - len(result))
        if not chunk:
            raise EOFError("renderer input closed")
        result.extend(chunk)
    return bytes(result)


def calm(color):
    """Less saturated, softer contrast and a little warmer, so the room feels relaxed."""
    lum = .299 * color[0] + .587 * color[1] + .114 * color[2]
    return [
        min(1., (.5 + (lum + (value - lum) * .78 - .5) * .92) * warmth)
        for value, warmth in zip(color, (1.04, 1.01, .94))
    ]


def geometry(app, vertices, name, parent=None, soft=True):
    from panda3d.core import (
        Geom, GeomNode, GeomTriangles, GeomVertexData,
        GeomVertexFormat, GeomVertexWriter,
    )
    data = GeomVertexData(name, GeomVertexFormat.get_v3n3c4(), Geom.UHStatic)
    data.set_num_rows(len(vertices))
    position, normal, color = (
        GeomVertexWriter(data, field) for field in ("vertex", "normal", "color")
    )
    for vertex in vertices:
        position.add_data3(vertex[0], -vertex[2], vertex[1])
        normal.add_data3(vertex[3], -vertex[5], vertex[4])
        color.add_data4(*(calm(vertex[6:9]) if soft else vertex[6:9]), 1)
    indices = GeomTriangles(Geom.UHStatic)
    for index in range(0, len(vertices), 3):
        indices.add_vertices(index, index + 1, index + 2)
    geom = Geom(data)
    geom.add_primitive(indices)
    node = GeomNode(name)
    node.add_geom(geom)
    path = (parent or app.render).attach_new_node(node)
    path.set_light_off()
    return path


def box(vertices, center, size, color, flat=False):
    """Adds a box; `flat` skips the baked face shading (eyes, highlights)."""
    x, y, z = (value / 2 for value in size)
    corners = [(-x, -y, -z), (x, -y, -z), (x, y, -z), (-x, y, -z),
               (-x, -y, z), (x, -y, z), (x, y, z), (-x, y, z)]
    for face, normal in (
        ((4, 5, 6, 7), (0, 0, 1)), ((1, 0, 3, 2), (0, 0, -1)),
        ((5, 1, 2, 6), (1, 0, 0)), ((0, 4, 7, 3), (-1, 0, 0)),
        ((7, 6, 2, 3), (0, 1, 0)), ((0, 1, 5, 4), (0, -1, 0)),
    ):
        shade = 1. if flat else .72 + .22 * max(0, normal[1]) + .06 * max(0, normal[0])
        for index in (0, 1, 2, 0, 2, 3):
            point = corners[face[index]]
            vertices.append((*[center[i] + point[i] for i in range(3)],
                             *normal, *[value * shade for value in color]))


def place(node, x, y, z):
    node.set_pos(x, -z, y)


def quad(vertices, y, x0, x1, z0, z1, color):
    for x, z in ((x0, z1), (x1, z1), (x1, z0), (x0, z1), (x1, z0), (x0, z0)):
        vertices.append((x, y, z, 0, 1, 0, *color))


FLOOR = 10.  # half-width: the café floor is 20 x 20
BACKGROUND = (38 / 255, 43 / 255, 58 / 255)  # keep in step with scene3d.rs
WOOD, WOOD_D = (.85, .62, .35), (.5, .3, .19)
TEAL, TEAL_D = (.24, .54, .52), (.16, .37, .36)
CREAM, RED, POT = (.96, .88, .66), (.7, .36, .32), (.55, .28, .25)
SAGE = (.6, .72, .6)
PLANKS = [(.54, .40, .24), (.49, .34, .20), (.58, .42, .26),
          (.62, .45, .27), (.73, .54, .32), (.67, .46, .27)]
WHITE, PINK, DARK = (.98, .96, .93), (1., .66, .72), (.13, .11, .15)


def furniture(v, x, z, face=0):
    """Returns a box adder for a piece at (x, z); face 0..3 turns it in 90 degree steps."""
    def part(center, size, color):
        dx, dy, dz = center
        dx, dz = ((dx, dz), (dz, -dx), (-dx, -dz), (-dz, dx))[face]
        sx, sy, sz = size
        box(v, (x + dx, dy, z + dz), (sz, sy, sx) if face % 2 else size, color)
    return part


def table(v, x, z):
    part = furniture(v, x, z)
    part((0, 1.13, 0), (1.65, .16, 1.15), WOOD)
    for dx in (-.7, .7):
        for dz in (-.45, .45):
            part((dx, .53, dz), (.1, 1.05, .1), WOOD_D)
    part((.35, 1.27, -.05), (.2, .12, .2), CREAM)
    for side in (-1, 1):
        chair(v, x, z + side * 1.0, 0 if side < 0 else 2)


def chair(v, x, z, face):
    part = furniture(v, x, z, face)
    part((0, .64, 0), (.65, .13, .65), TEAL)
    part((0, 1.05, -.3), (.65, .7, .1), TEAL_D)
    for dx in (-.26, .26):
        for dz in (-.26, .26):
            part((dx, .29, dz), (.08, .58, .08), WOOD_D)


def sofa(v, x, z, face, width):
    part = furniture(v, x, z, face)
    part((0, .45, 0), (width, .5, 1.0), TEAL_D)
    part((0, .8, .05), (width - .6, .2, .85), TEAL)
    part((0, 1.1, -.36), (width, .9, .28), TEAL_D)
    for side in (-1, 1):
        part((side * (width / 2 - .14), .75, 0), (.28, .7, 1.0), TEAL_D)


def plant(v, x, z):
    box(v, (x, .25, z), (.7, .5, .7), POT)
    for y, size, shade in ((.8, .95, .0), (1.25, .72, .06), (1.6, .45, .12)):
        box(v, (x, y, z), (size, .5, size), (.27 + shade, .45 + shade, .27))


def shelf(v, x, z0, z1, rng):
    box(v, (x, 1.6, (z0 + z1) / 2), (.4, 2.9, z1 - z0), (.55, .32, .2))
    for row, y in enumerate((.43, 1.22, 2.01, 2.8)):
        box(v, (x + .08, y, (z0 + z1) / 2), (.55, .1, z1 - z0), WOOD)
        if row < 3:
            z = z0 + .1
            while z < z1 - .25:
                height = rng.uniform(.4, .66)
                box(v, (x + .2, y + .05 + height / 2, z + .08), (.2, height, .16),
                    rng.choice([(.7, .3, .28), (.3, .5, .55), (.85, .7, .4), (.35, .55, .35)]))
                z += .2


def window(v, x, z, face, width):
    part = furniture(v, x, z, face)
    part((0, 2.68, 0), (width, 2.0, .12), (.14, .22, .24))
    part((0, 2.68, .07), (width - .2, 1.8, .04), (.95, .75, .45))
    part((0, 2.68, .1), (.1, 1.8, .06), CREAM)
    part((0, 2.68, .1), (width - .2, .1, .06), CREAM)


def door(v, x, z, face):
    part = furniture(v, x, z, face)
    part((0, 1.55, 0), (1.7, 3.1, .12), WOOD_D)
    part((0, 1.85, .07), (1.3, 2.1, .04), (.95, .75, .45))
    part((.58, 1.4, .1), (.1, .1, .06), CREAM)


def lamp(v, x, z):
    box(v, (x, 3.9, z), (.04, .9, .04), (.1, .17, .18))
    box(v, (x, 3.25, z), (.5, .25, .5), WOOD)
    box(v, (x, 3.0, z), (.9, .15, .9), WOOD)


def cat_tree(v, x, z):
    box(v, (x, .12, z), (1.3, .24, 1.3), TEAL_D)
    box(v, (x, 1.1, z), (.24, 1.9, .24), WOOD_D)
    box(v, (x, 1.25, z), (1.1, .14, 1.1), TEAL)
    box(v, (x - .2, 2.2, z + .2), (.8, .14, .8), TEAL)
    box(v, (x - .2, 1.7, z + .2), (.2, 1.0, .2), WOOD_D)
    box(v, (x - .2, 2.55, z + .2), (.5, .5, .5), CREAM)


def cushion(v, x, z, color):
    box(v, (x, .2, z), (.9, .2, .9), color)
    box(v, (x, .32, z), (.7, .06, .7), CREAM)


def cafe_extension(rng):
    """A 20 x 20 café around the baked bar room (back-left corner).

    Returns vertex lists by part: `floor` and `low` (wainscot) always draw; the
    upper walls drop away while the camera is outside them so the room stays visible.
    """
    parts = {name: [] for name in ("floor", "low", "back", "left", "front", "right")}
    v, edge = parts["floor"], FLOOR
    box(v, ((.3 + edge) / 2, -.16, 0), (edge - .3, .3, 2 * edge), (.15, .245, .27))
    box(v, ((-edge + .3) / 2, -.16, (edge - 2.5) / 2), (edge + .3, .3, edge + 2.5), (.15, .245, .27))
    for column in range(40):
        x0 = -9.84 + .5 * column
        x1 = min(x0 + .48, edge)
        for row in range(11):
            z0 = -9.92 + 1.84 * row
            z1 = min(z0 + 1.81, edge)
            if x1 > .15 or z1 > -2.5:
                quad(v, .04, x0, x1, z0, z1, rng.choice(PLANKS))
    # Four walls: the baked room already has the back-left corner. Each wall is a
    # low wainscot that always stays, plus an upper part that drops when needed.
    upper, low = (.94, .85, .67), parts["low"]
    box(parts["back"], (5.1, 2.15, -9.9), (9.8, 4.3, .14), upper)
    box(parts["left"], (-9.895, 2.15, 3.7), (.15, 4.3, 12.6), (.94, .83, .65))
    box(parts["front"], (0, 2.15, 9.9), (20, 4.3, .14), upper)
    box(parts["right"], (9.9, 2.15, 0), (.14, 4.3, 20), (.94, .83, .65))
    for center, size, trim_center, trim_size in (
        ((5.08, .68, -9.79), (9.85, 1.36, .12), (5.08, 1.4, -9.67), (9.85, .1, .18)),
        ((-9.79, .68, 3.7), (.12, 1.36, 12.6), (-9.67, 1.4, 3.7), (.18, .1, 12.6)),
        ((0, .68, 9.79), (19.9, 1.36, .12), (0, 1.4, 9.67), (19.9, .1, .18)),
        ((9.79, .68, 0), (.12, 1.36, 19.9), (9.67, 1.4, 0), (.18, .1, 19.9)),
    ):
        box(low, center, size, TEAL)
        box(low, trim_center, trim_size, WOOD)
    for x in (3.6, 8.4):
        window(parts["back"], x, -9.77, 0, 2.6)
    for z in (-.2, 8.0):
        window(parts["left"], -9.77, z, 1, 2.4)
    for x in (-6.5, 6.5):
        window(parts["front"], x, 9.77, 2, 2.6)
    door(parts["front"], 0, 9.77, 2)
    for z in (-1.5, 3.4, 7.4):
        window(parts["right"], 9.77, z, 3, 2.4)
    back = parts["back"]
    box(back, (6, 2.9, -9.76), (2.6, 1.3, .08), (.5, .3, .19))
    box(back, (6, 2.9, -9.7), (2.4, 1.1, .04), (.13, .2, .2))
    for y in (2.6, 2.9, 3.2):
        box(back, (6, y, -9.66), (1.6, .05, .03), CREAM)
    shelf(v, -9.6, 2.2, 5.6, rng)
    # Lounge, back right.
    box(v, (6, .08, -6.7), (7.6, .03, 4.0), CREAM)
    box(v, (6, .095, -6.7), (7.3, .04, 3.7), RED)
    sofa(v, 6, -9.2, 0, 3.4)
    sofa(v, 3.1, -6.9, 1, 1.5)
    sofa(v, 8.9, -6.9, 3, 1.5)
    box(v, (6, .62, -6.8), (1.6, .1, .9), WOOD)
    for dx in (-.65, .65):
        for dz in (-.3, .3):
            box(v, (6 + dx, .3, -6.8 + dz), (.08, .6, .08), WOOD_D)
    box(v, (6.3, .72, -6.8), (.2, .12, .2), CREAM)
    # Four reading tables at the sides leave the middle open for the cats.
    for x in (-7.0, 7.0):
        for z in (.4, 5.4):
            table(v, x, z)
            lamp(v, x, z)
    box(v, (0, .07, 3.4), (4.6, .03, 3.4), CREAM)
    box(v, (0, .085, 3.4), (4.3, .04, 3.1), SAGE)
    for x, z, color in ((-.8, 3.6, RED), (.9, 3.1, TEAL), (.1, 2.8, WOOD)):
        cushion(v, x, z, color)
    for x, z in ((-9.1, 9.1), (.9, -9.0), (9.2, -9.2), (6.2, 9.0)):
        plant(v, x, z)
    # Cat tree and beds in the front corners.
    cat_tree(v, 8.9, 9.0)
    for x in (-8.6, -6.9):
        cushion(v, x, 9.0, RED)
    return parts


def split_bar(vertices):
    """Splits the baked room into the always-visible part and its upper walls.

    The upper back/left walls (and what hangs on them) must drop when the camera
    is behind them. Pieces are the mesh's connected boxes, told apart by position.
    """
    triangles = [vertices[i:i + 3] for i in range(0, len(vertices), 3)]
    parent = list(range(len(triangles)))

    def find(index):
        while parent[index] != index:
            parent[index] = parent[parent[index]]
            index = parent[index]
        return index

    seen = {}
    for index, triangle in enumerate(triangles):
        for point in triangle:
            key = tuple(round(value, 3) for value in point[:3])
            if seen.setdefault(key, index) != index:
                parent[find(index)] = find(seen[key])
    pieces = {}
    for index, triangle in enumerate(triangles):
        pieces.setdefault(find(index), []).extend(triangle)
    main, back, left = [], [], []
    for piece in pieces.values():
        xs, ys, zs = ([point[axis] for point in piece] for axis in range(3))
        if max(ys) > 4.2 and -5.13 < min(xs) and max(xs) < -4.96:
            left += piece
        elif (max(ys) > 4.2 and -3.73 < min(zs) and max(zs) < -3.57) or (
                max(zs) <= -2.75 and min(ys) >= 1.5):
            back += piece
        else:
            main += piece
    return main, back, left


LEG = .41
STANCE = .62  # fraction of a step cycle a paw spends on the ground
# Lateral-sequence walk: back-left, front-left, back-right, front-right.
LEG_PHASE = (0., .25, .5, .75)


def cat_actor(app):
    root = app.render.attach_new_node("account")
    body = root.attach_new_node("body")
    tint = []  # parts coloured by provider; white, pink and dark parts stay as drawn

    def add(parent, name, mesh, tinted=False):
        path = geometry(app, mesh, name, parent, soft=False)
        if tinted:
            tint.append(path)
        return path

    def node(parent, name, x, y, z):
        path = parent.attach_new_node(name)
        place(path, x, y, z)
        return path

    torso = []
    box(torso, (0, .66, 0), (.52, .44, .92), (1, 1, 1))
    for z in (-.25, 0., .25):  # tabby stripes
        box(torso, (0, .885, z), (.36, .02, .09), (.72, .72, .72), flat=True)
    add(body, "torso", torso, True)
    chest = []
    box(chest, (0, .6, .475), (.3, .34, .04), WHITE)
    box(chest, (0, .5, 0), (.34, .06, .8), WHITE)
    add(body, "chest", chest)

    head = node(body, "head", 0, 1.0, .55)
    skull = []
    box(skull, (0, .02, .1), (.64, .52, .5), (1, 1, 1))
    add(head, "skull", skull, True)
    face = []
    box(face, (0, -.08, .36), (.34, .2, .14), WHITE)  # muzzle
    box(face, (0, -.01, .44), (.1, .07, .03), PINK, flat=True)  # nose
    for side in (-1, 1):
        box(face, (side * .045, -.09, .44), (.07, .02, .02), DARK, flat=True)  # smile
        box(face, (side * .25, -.07, .355), (.1, .06, .02), PINK, flat=True)  # blush
        for dy in (-.03, -.09):  # whiskers
            box(face, (side * .46, dy, .33), (.28, .015, .015), WHITE, flat=True)
    add(head, "face", face)
    eyes = node(head, "eyes", 0, .1, .357)
    eye_mesh = []
    for side in (-1, 1):
        box(eye_mesh, (side * .17, 0, 0), (.13, .18, .03), DARK, flat=True)
        box(eye_mesh, (side * .14, .045, .02), (.05, .06, .02), (1, 1, 1), flat=True)
        box(eye_mesh, (side * .205, -.045, .02), (.03, .03, .02), (1, 1, 1), flat=True)
    add(eyes, "eyes", eye_mesh)
    ears = []
    for side in (-1, 1):
        ear = node(head, "ear", side * .21, .24, 0)
        outer, inner = [], []
        box(outer, (0, .07, 0), (.2, .16, .12), (1, 1, 1))
        box(outer, (0, .2, 0), (.12, .12, .1), (1, 1, 1))
        box(inner, (0, .09, .065), (.1, .15, .02), PINK, flat=True)
        add(ear, "ear", outer, True)
        add(ear, "inner", inner)
        ears.append(ear)

    # The tail rises from the middle of the back and curls upward.
    tail = []
    base = node(body, "tail", 0, .78, -.44)
    base.set_p(35)
    parent, previous = base, 0.
    for length, thickness in ((.34, .13), (.3, .12), (.26, .11)):
        part = node(parent, "tail-part", 0, 0, -previous)
        mesh = []
        box(mesh, (0, 0, -length / 2), (thickness, thickness, length), (1, 1, 1))
        add(part, "tail", mesh, True)
        tail.append(part)
        parent, previous = part, length
    tip = []
    box(tip, (0, 0, -previous), (.115, .115, .12), WHITE)
    add(parent, "tail-tip", tip)

    legs = []
    for x in (-.17, .17):
        for z in (-.32, .32):
            leg = node(body, "leg", x, LEG, z)
            shaft, paw = [], []
            box(shaft, (0, -.17, 0), (.15, .34, .15), (1, 1, 1))
            box(paw, (0, -.37, .03), (.17, .09, .21), WHITE)
            add(leg, "leg", shaft, True)
            add(leg, "paw", paw)
            legs.append(leg)

    collar_mesh, bell, marker_mesh = [], [], []
    box(collar_mesh, (0, .74, .45), (.5, .09, .14), (1, 1, 1))
    collar = add(body, "status", collar_mesh)
    box(bell, (0, .66, .53), (.07, .07, .05), (1., .82, .3), flat=True)
    add(body, "bell", bell)
    box(marker_mesh, (0, .015, 0), (.95, .035, 1.4), (1., .85, .18))
    marker = add(root, "selection", marker_mesh)
    root.hide()
    return {"robot": False, "root": root, "body": body, "tint": tint, "head": head,
            "eyes": eyes, "ears": ears, "tail": tail, "legs": legs,
            "collar": collar, "marker": marker}


def robot_actor(app):
    root = app.render.attach_new_node("models")
    tint = []

    def add(name, mesh, tinted=False):
        path = geometry(app, mesh, name, root, soft=False)
        if tinted:
            tint.append(path)
        return path

    parts, face, collar_mesh, marker_mesh = [], [], [], []
    box(parts, (0, .62, 0), (.62, .84, .48), (1, 1, 1))
    box(parts, (0, 1.18, 0), (.78, .48, .58), (1, 1, 1))
    for x in (-.2, .2):
        box(parts, (x, .13, .08), (.27, .22, .42), (1, 1, 1))
    box(parts, (0, 1.55, 0), (.06, .24, .06), (1, 1, 1))
    add("robot", parts, True)
    for x in (-.17, .17):
        box(face, (x, 1.15, .325), (.09, .10, .035), (.08, .12, .16))
    box(face, (0, 1., .33), (.10, .065, .04), (1., .65, .55))
    box(face, (0, 1.7, 0), (.12, .12, .12), (1., .82, .3))
    add("face", face)
    box(collar_mesh, (0, .85, .03), (.65, .10, .55), (1, 1, 1))
    collar = add("status", collar_mesh)
    box(marker_mesh, (0, .015, 0), (.9, .035, .9), (1., .85, .18))
    marker = add("selection", marker_mesh)
    root.hide()
    return {"robot": True, "root": root, "tint": tint, "collar": collar, "marker": marker}


def smooth(value):
    return value * value * (3 - 2 * value)


def pose(item, obj, time, index):
    x, y, z = obj["position"]
    phase, stride = obj["gait"], obj["stride"]
    place(item["root"], x, y, z)
    item["root"].set_h(math.degrees(obj["heading"]))
    item["root"].show()
    # Softer pastel versions of the provider colours.
    color = [value * .78 + .22 for value in obj["color"]]
    for path in item["tint"]:
        path.set_color_scale(*color, 1)
    item["collar"].set_color_scale(*obj["status"], 1)
    if obj["selected"]:
        item["marker"].show()
    else:
        item["marker"].hide()
    if item["robot"]:
        return
    # Each paw steps back along the ground, then lifts and swings forward.
    for leg, offset in zip(item["legs"], LEG_PHASE):
        u = (phase / math.tau + offset) % 1
        if u < STANCE:
            reach, lift = 1 - 2 * u / STANCE, 0.
        else:
            step = (u - STANCE) / (1 - STANCE)
            reach, lift = -1 + 2 * smooth(step), math.sin(math.pi * step)
        leg.set_p(-math.degrees(math.asin(.49 * reach * stride)))
        leg.set_sz(1 - .3 * lift * stride)
    item["body"].set_z(.02 * stride * math.cos(2 * phase))
    item["body"].set_r(2.5 * stride * math.sin(phase))
    # Idle charm: look around while standing, blink, twitch an ear, wave the tail.
    item["head"].set_h(18 * math.sin(time * .5 + index * 1.3) * (1 - stride))
    item["head"].set_p(3 * stride * math.sin(2 * phase))
    item["eyes"].set_sz(.12 if (time + index * 1.7) % 4.6 > 4.4 else 1)
    item["ears"][1].set_r(14 * max(0, math.sin(time * .8 + index * 2.3)) ** 14)
    item["tail"][0].set_h(8 * math.sin(time * 1.4 + index))
    for number, part in enumerate(item["tail"][1:], 1):
        part.set_p(25)
        part.set_h(14 * math.sin(time * 1.4 + index - number * .8))


def main():
    from panda3d.core import (
        GraphicsOutput, Texture, Vec3, load_prc_file_data,
    )
    if exact(4) != b"SCN1":
        raise ValueError("invalid scene protocol")
    length = struct.unpack("<I", exact(4))[0]
    if length > 1024 * 1024 or length < 60:
        raise ValueError("invalid scene size")
    mesh = exact(length)
    if mesh[:8] != b"TART0001" or (len(mesh) - 60) % 108:
        raise ValueError("invalid scene mesh")
    driver = "p3headlessgl" if sys.platform.startswith("linux") else "pandagl"
    load_prc_file_data("", (
        f"window-type offscreen\nload-display {driver}\n"
        "audio-library-name null\nwin-size 160 100\nsync-video false\n"
        "framebuffer-multisample false\nframebuffer-srgb false\n"
        "notify-level warning\n"
    ))
    from direct.showbase.ShowBase import ShowBase
    app = ShowBase(windowType="offscreen")
    try:
        if app.win is None:
            raise RuntimeError("could not create an offscreen OpenGL buffer")
        app.disable_mouse()
        app.set_background_color(*BACKGROUND, 1)
        renderer = app.win.get_gsg().get_driver_renderer()
        if not renderer or any(name in renderer.lower() for name in (
            "llvmpipe", "softpipe", "swiftshader", "software rasterizer", "gdi generic",
        )):
            raise RuntimeError(f"hardware GPU required; renderer is {renderer!r}")
        # Upper walls, by side, hidden while the camera is outside that wall.
        walls = {side: [] for side in ("back", "left", "front", "right")}
        shift = (-FLOOR + 5.15, 0, -FLOOR + 3.75)  # baked bar room: back-left corner
        bar = split_bar(list(struct.iter_unpack("<9f", mesh[60:])))
        del mesh
        for name, vertices in zip(("cafe", "cafe-back", "cafe-left"), bar):
            path = geometry(app, vertices, name)
            place(path, *shift)
            if name != "cafe":
                walls[name[5:]].append(path)
        for name, vertices in cafe_extension(random.Random(7)).items():
            path = geometry(app, vertices, "cafe-" + name)
            if name in walls:
                walls[name].append(path)
        # At most seven visible actors, reused across account pages.
        cats = [cat_actor(app) for _ in range(7)]
        robot = robot_actor(app)
        texture = Texture()
        app.win.add_render_texture(texture, GraphicsOutput.RTMCopyRam, GraphicsOutput.RTPColor)
        app.camLens.set_near_far(.05, 150)
        ready = json.dumps({"renderer": renderer, "gpu": True}).encode()
        wire.write(b"P3D1" + struct.pack("<I", len(ready)) + ready)
        wire.flush()
        size = None
        while True:
            try:
                length = struct.unpack("<I", exact(4))[0]
            except EOFError:
                break
            if length > 65536:
                raise ValueError("renderer request exceeds limit")
            request = json.loads(exact(length))
            width, height = request["width"], request["height"]
            if not (1 <= width <= 1920 and 2 <= height <= 1200 and height % 2 == 0):
                raise ValueError("invalid image dimensions")
            if len(request["objects"]) > 7:
                raise ValueError("too many visible actors")
            if size != (width, height):
                app.win.set_size(width, height)
                app.camLens.set_fov(
                    math.degrees(2 * math.atan(math.tan(math.radians(20)) * width / height)),
                    40,
                )
                size = width, height
            x, y, z = request["camera"]
            app.camera.set_pos(x, -z, y)
            app.camera.look_at(Vec3(0, -2, 1.3))  # Rust target (0, 1.3, 2)
            # A wall between the camera and the room drops to its wainscot.
            inside = {"back": z > -FLOOR, "left": x > -FLOOR, "front": z < FLOOR, "right": x < FLOOR}
            for side, paths in walls.items():
                for path in paths:
                    path.show() if inside[side] else path.hide()
            for item in cats + [robot]:
                item["root"].hide()
            for index, obj in enumerate(request["objects"]):
                item = robot if obj["models"] else cats[index]
                pose(item, obj, request["time"], index)
            app.graphicsEngine.render_frame()
            image = memoryview(texture.get_ram_image_as("RGB"))
            if len(image) != width * height * 3:
                raise RuntimeError("GPU returned an unexpected image size")
            wire.write(b"RGB1" + struct.pack("<III", width, height, len(image)))
            for row in reversed(range(height)):
                wire.write(image[row * width * 3:(row + 1) * width * 3])
            wire.flush()
    finally:
        app.destroy()


if __name__ == "__main__":
    try:
        main()
    except (BrokenPipeError, EOFError):
        pass
    except Exception as error:
        print(f"Panda3D GPU: {error}", file=sys.stderr, flush=True)
        raise SystemExit(1)
