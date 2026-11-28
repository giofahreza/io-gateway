//! Pixel-accurate 3D display (opt-in): sixel images drawn over the scene's cells.
//!
//! ratatui is told to leave those cells alone (`skip`), so only the regions that
//! changed are sent. Text drawn over the scene, such as labels and modals, owns its
//! cells: the image is never painted over it and returns when the text goes away.
//!
//! Inside tmux the images go through DCS passthrough. tmux's own sixel storage cannot
//! hold an image that keeps changing under text (text on a row erases the whole row,
//! pieces of split images go missing), whereas passthrough hands the bytes straight
//! to the terminal. It needs `set -g allow-passthrough on`.
use crossterm::{cursor::MoveTo, queue};
use ratatui::{buffer::Buffer, layout::Rect, style::Color};
use std::{
    collections::HashMap,
    io::{self, Write},
    process::Command,
};

/// Colour registers per image; every sixel terminal offers at least this many.
const MAX_COLORS: usize = 256;

/// How images reach the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Direct,
    /// Wrapped for tmux, with the pane's top-left corner in the terminal window.
    Tmux {
        left: u16,
        top: u16,
    },
}

impl Route {
    fn write(self, out: &mut impl Write, column: u16, row: u16, image: &[u8]) -> io::Result<()> {
        match self {
            Route::Direct => {
                queue!(out, MoveTo(column, row))?;
                out.write_all(image)
            }
            Route::Tmux { left, top } => {
                // tmux does not know where in the window the image belongs, so the
                // cursor move travels inside the passthrough. ESC is doubled in it.
                let mut inner =
                    format!("\x1b[{};{}H", top + row + 1, left + column + 1).into_bytes();
                inner.extend_from_slice(image);
                let mut wrapped = Vec::with_capacity(inner.len() + 64);
                wrapped.extend_from_slice(b"\x1bPtmux;");
                for byte in inner {
                    if byte == 0x1b {
                        wrapped.push(0x1b);
                    }
                    wrapped.push(byte);
                }
                wrapped.extend_from_slice(b"\x1b\\");
                out.write_all(&wrapped)
            }
        }
    }
}

fn tmux(args: &[&str]) -> Option<String> {
    let output = Command::new("tmux").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Where to send images right now. `Ok(None)`: inside tmux but this window is not the
/// one on screen, so nothing may be drawn. `Err`: sixel cannot be used here.
pub(super) fn route() -> Result<Option<Route>, &'static str> {
    if std::env::var_os("TMUX").is_none() {
        return Ok(Some(Route::Direct));
    }
    let pane = std::env::var("TMUX_PANE").map_err(|_| "no tmux pane")?;
    let setting = tmux(&["show-options", "-Apv", "-t", &pane, "allow-passthrough"]);
    if !passthrough_allowed(setting.as_deref().unwrap_or("")) {
        return Err("tmux needs `set -g allow-passthrough on` for sixel");
    }
    let geometry = tmux(&[
        "display-message",
        "-p",
        "-t",
        &pane,
        "#{pane_left} #{pane_top} #{window_active} #{client_height} #{window_height}",
    ])
    .ok_or("tmux did not answer")?;
    let top_bar = tmux(&["show-options", "-gv", "status-position"]).is_some_and(|v| v == "top");
    pane_route(&geometry, top_bar).ok_or("unreadable tmux geometry")
}

fn passthrough_allowed(setting: &str) -> bool {
    matches!(setting, "on" | "all")
}

/// Parses `pane_left pane_top window_active client_height window_height`. A status
/// line above the panes pushes them down by the rows the client has beyond the window.
fn pane_route(geometry: &str, top_bar: bool) -> Option<Option<Route>> {
    let field: Vec<&str> = geometry.split_whitespace().collect();
    let number = |i: usize| field.get(i)?.parse::<u16>().ok();
    let (left, top, active) = (number(0)?, number(1)?, number(2)?);
    let bar = match (number(3), number(4)) {
        (Some(client), Some(window)) if top_bar => client.saturating_sub(window),
        _ => 0,
    };
    Some((active == 1).then_some(Route::Tmux {
        left,
        top: top + bar,
    }))
}

/// One frame of tightly packed RGB pixels.
pub(super) struct Pixels<'a> {
    pub rgb: &'a [u8],
    pub width: u32,
    pub height: u32,
}

/// Encodes the `w` x `h` pixel rectangle at (`x`, `y`) as one sixel image. Rows past
/// the bottom of the frame repeat its last row, so a rectangle may end on a cell edge.
///
/// The flat-shaded scene has far fewer colours than the register budget, so colours
/// stay exact. Should a rectangle exceed it, low bits are dropped for that rectangle.
pub(super) fn encode(frame: &Pixels, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let at = |px: u32, py: u32| {
        let o = ((py.min(frame.height - 1) * frame.width + px) * 3) as usize;
        [frame.rgb[o], frame.rgb[o + 1], frame.rgb[o + 2]]
    };
    let (palette, indices) = (1..=8u32)
        .rev()
        .find_map(|keep| {
            let mask = 0xffu8 << (8 - keep);
            let mut known: HashMap<[u8; 3], u16> = HashMap::new();
            let mut palette: Vec<[u8; 3]> = Vec::new();
            let mut indices = Vec::with_capacity((w * h) as usize);
            for py in y..y + h {
                for px in x..x + w {
                    let c = at(px, py).map(|v| v & mask);
                    let index = *known.entry(c).or_insert_with(|| {
                        palette.push(c);
                        palette.len() as u16 - 1
                    });
                    indices.push(index);
                }
            }
            (palette.len() <= MAX_COLORS || keep == 1).then_some((palette, indices))
        })
        .expect("one bit per channel always fits");
    let (w, h) = (w as usize, h as usize);
    let mut out = format!("\x1bP9;1q\"1;1;{w};{h}").into_bytes();
    for (n, c) in palette.iter().enumerate() {
        let percent = |v: u8| (u32::from(v) * 100 + 127) / 255;
        write!(
            out,
            "#{n};2;{};{};{}",
            percent(c[0]),
            percent(c[1]),
            percent(c[2])
        )
        .unwrap();
    }
    let mut bits = vec![0u8; palette.len() * w];
    for band in 0..h.div_ceil(6) {
        let mut used: Vec<usize> = Vec::new();
        for dy in 0..6.min(h - band * 6) {
            for px in 0..w {
                let color = indices[(band * 6 + dy) * w + px] as usize;
                if bits[color * w..(color + 1) * w].iter().all(|b| *b == 0) {
                    used.push(color);
                }
                bits[color * w + px] |= 1 << dy;
            }
        }
        used.sort_unstable();
        used.dedup();
        for (n, &color) in used.iter().enumerate() {
            if n > 0 {
                out.push(b'$');
            }
            write!(out, "#{color}").unwrap();
            let row = &mut bits[color * w..(color + 1) * w];
            let mut px = 0;
            while px < w {
                let value = row[px];
                let run = row[px..].iter().take_while(|b| **b == value).count();
                if run > 3 {
                    write!(out, "!{run}").unwrap();
                    out.push(63 + value);
                } else {
                    out.extend(std::iter::repeat(63 + value).take(run));
                }
                px += run;
            }
            row.fill(0);
        }
        // Bands are separated, not terminated: a trailing `-` declares an empty band
        // and some terminals (tmux) then pad the image by six pixel rows.
        if band + 1 < h.div_ceil(6) {
            out.push(b'-');
        }
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Splits the `true` cells of a `columns` x `rows` mask into disjoint rectangles that
/// cover exactly those cells: (left, top, right, bottom), all inclusive.
pub(super) fn rectangles(mask: &[bool], columns: usize, rows: usize) -> Vec<(u16, u16, u16, u16)> {
    let (mut found, mut open): (Vec<_>, Vec<(usize, usize, usize)>) = (Vec::new(), Vec::new());
    for row in 0..=rows {
        let mut runs = Vec::new();
        let mut column = 0;
        while row < rows && column < columns {
            if mask[row * columns + column] {
                let start = column;
                while column < columns && mask[row * columns + column] {
                    column += 1;
                }
                runs.push((start, column - 1));
            } else {
                column += 1;
            }
        }
        // A run directly below an open rectangle of the same width extends it.
        let mut next = Vec::new();
        for (left, right) in runs {
            match open.iter().position(|o| (o.0, o.1) == (left, right)) {
                Some(at) => next.push(open.swap_remove(at)),
                None => next.push((left, right, row)),
            }
        }
        found.extend(
            open.drain(..)
                .map(|(l, r, top)| (l as u16, top as u16, r as u16, row as u16 - 1)),
        );
        open = next;
    }
    found
}

/// The rectangles to send for the `dirty` cells. Nearby cells are grouped, so a
/// moving cat is one image instead of a scatter of slivers, but a rectangle never
/// includes a `covered` cell: those hold text.
pub(super) fn patches(
    dirty: &[bool],
    covered: &[bool],
    columns: usize,
    rows: usize,
) -> Vec<(u16, u16, u16, u16)> {
    let neighbours = |n: usize| {
        let (x, y) = ((n % columns) as isize, (n / columns) as isize);
        (-1..=1)
            .flat_map(move |dy| (-1..=1).map(move |dx| (x + dx, y + dy)))
            .filter(move |(nx, ny)| {
                (0..columns as isize).contains(nx) && (0..rows as isize).contains(ny)
            })
            .map(move |(nx, ny)| ny as usize * columns + nx as usize)
    };
    // Grow by one cell, never into text, so near neighbours join.
    let grown: Vec<bool> = (0..columns * rows)
        .map(|n| !covered[n] && neighbours(n).any(|m| dirty[m]))
        .collect();
    let mut seen = vec![false; columns * rows];
    let mut found = Vec::new();
    for start in (0..columns * rows).filter(|n| grown[*n]) {
        if seen[start] {
            continue;
        }
        let (mut stack, mut cells) = (vec![start], Vec::new());
        seen[start] = true;
        while let Some(n) = stack.pop() {
            cells.push(n);
            for m in neighbours(n) {
                if grown[m] && !seen[m] {
                    seen[m] = true;
                    stack.push(m);
                }
            }
        }
        let (left, right) = cells.iter().fold((usize::MAX, 0), |(l, r), n| {
            (l.min(n % columns), r.max(n % columns))
        });
        let (top, bottom) = cells.iter().fold((usize::MAX, 0), |(t, b), n| {
            (t.min(n / columns), b.max(n / columns))
        });
        if (top..=bottom).all(|y| (left..=right).all(|x| !covered[y * columns + x])) {
            found.push((left as u16, top as u16, right as u16, bottom as u16));
        } else {
            // Text sits inside the group: send exactly the group's own cells.
            let w = right - left + 1;
            let mut local = vec![false; w * (bottom - top + 1)];
            for n in cells {
                local[(n / columns - top) * w + n % columns - left] = true;
            }
            found.extend(rectangles(&local, w, bottom - top + 1).into_iter().map(
                |(l, t, r, b)| {
                    (
                        l + left as u16,
                        t + top as u16,
                        r + left as u16,
                        b + top as u16,
                    )
                },
            ));
        }
    }
    found
}

/// What the terminal currently shows in the scene's cells.
pub(super) struct Canvas {
    area: Rect,
    cell: (u32, u32),
    size: (u32, u32),
    /// False while the area is being blanked; the image only follows a clean slate.
    live: bool,
    /// The pixels last sent for each cell.
    shown: Vec<u8>,
    /// Cells where the terminal shows `shown`; text over a cell clears this.
    drawn: Vec<bool>,
    /// Cells ratatui drew text into this frame.
    covered: Vec<bool>,
    /// Where the images were sent last time; a different route starts over.
    route: Option<Route>,
}

impl Canvas {
    pub fn new(area: Rect, cell: (u32, u32), size: (u32, u32)) -> Self {
        let cells = usize::from(area.width) * usize::from(area.height);
        Self {
            area,
            cell,
            size,
            live: false,
            shown: vec![0; (size.0 * size.1 * 3) as usize],
            drawn: vec![false; cells],
            covered: vec![false; cells],
            route: None,
        }
    }

    pub fn fits(&self, area: Rect, cell: (u32, u32), size: (u32, u32)) -> bool {
        (self.area, self.cell, self.size) == (area, cell, size)
    }

    /// The terminal's contents are unknown (cleared, resized, another screen was shown).
    pub fn invalidate(&mut self) {
        self.live = false;
        self.drawn.fill(false);
    }

    /// Send every cell again over what is already there, without blanking first.
    pub fn refresh(&mut self) {
        self.drawn.fill(false);
    }

    /// Blank the scene's cells once so no old text lingers under the image, then
    /// have ratatui leave them alone.
    pub fn mark(&self, buffer: &mut Buffer, background: Color) {
        for y in self.area.top()..self.area.bottom() {
            for x in self.area.left()..self.area.right() {
                let cell = &mut buffer[(x, y)];
                if self.live {
                    cell.set_skip(true);
                } else {
                    cell.set_symbol(" ").set_bg(background).set_skip(false);
                }
            }
        }
    }

    /// Cells that ratatui will emit in this frame hold text rather than image.
    pub fn note_coverage(&mut self, buffer: &Buffer) {
        if !self.live {
            return;
        }
        let columns = usize::from(self.area.width);
        for (n, covered) in self.covered.iter_mut().enumerate() {
            let (x, y) = (n % columns, n / columns);
            *covered = !buffer[(self.area.x + x as u16, self.area.y + y as u16)].skip;
        }
    }

    fn changed(&self, frame: &Pixels, column: usize, row: usize) -> bool {
        let (cw, ch) = (self.cell.0 as usize, self.cell.1 as usize);
        let last = (row * ch + ch).min(frame.height as usize);
        (row * ch..last).any(|y| {
            let at = (y * frame.width as usize + column * cw) * 3;
            frame.rgb[at..at + cw * 3] != self.shown[at..at + cw * 3]
        })
    }

    /// Sends the cells that changed since they were last shown, or that were freed
    /// from text. The first call after a blanking frame only goes live.
    pub fn present(
        &mut self,
        frame: &Pixels,
        route: Route,
        out: &mut impl Write,
    ) -> io::Result<()> {
        if self.route != Some(route) {
            // A new route (first use, or the tmux pane moved and tmux repainted).
            self.route = Some(route);
            self.drawn.fill(false);
        }
        if !self.live {
            self.live = true;
            return Ok(());
        }
        let (columns, rows) = (usize::from(self.area.width), usize::from(self.area.height));
        let mut dirty = vec![false; columns * rows];
        for n in 0..columns * rows {
            if self.covered[n] {
                self.drawn[n] = false;
            } else {
                dirty[n] = !self.drawn[n] || self.changed(frame, n % columns, n / columns);
            }
        }
        let rects = patches(&dirty, &self.covered, columns, rows);
        if rects.is_empty() {
            return Ok(());
        }
        // A terminal can show the patches together; through tmux they just stream.
        let synchronized = route == Route::Direct;
        if synchronized {
            out.write_all(b"\x1b[?2026h")?;
        }
        let (cw, ch) = self.cell;
        for (left, top, right, bottom) in rects {
            let (x, y) = (u32::from(left) * cw, u32::from(top) * ch);
            let (w, h) = (
                (u32::from(right - left) + 1) * cw,
                (u32::from(bottom - top) + 1) * ch,
            );
            route.write(
                out,
                self.area.x + left,
                self.area.y + top,
                &encode(frame, x, y, w, h),
            )?;
            for py in y..(y + h).min(frame.height) {
                let at = ((py * frame.width + x) * 3) as usize;
                self.shown[at..at + (w * 3) as usize]
                    .copy_from_slice(&frame.rgb[at..at + (w * 3) as usize]);
            }
            for cy in top..=bottom {
                for cx in left..=right {
                    self.drawn[usize::from(cy) * columns + usize::from(cx)] = true;
                }
            }
        }
        if synchronized {
            out.write_all(b"\x1b[?2026l")?;
        }
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes sixel back to RGB, as a terminal would; transparent pixels stay `None`.
    fn decode(data: &[u8]) -> (usize, usize, Vec<Option<[u8; 3]>>) {
        let text = std::str::from_utf8(data).unwrap();
        let body = text
            .strip_prefix("\x1bP9;1q")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let mut parts = body.strip_prefix('"').unwrap().splitn(5, ';');
        let (_, _, w, rest) = (parts.next(), parts.next(), parts.next(), parts.next());
        let w: usize = w.unwrap().parse().unwrap();
        let mut rest = format!(
            "{}{}",
            rest.unwrap(),
            parts.next().map(|s| format!(";{s}")).unwrap_or_default()
        );
        let height_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap();
        let h: usize = rest[..height_end].parse().unwrap();
        rest = rest[height_end..].to_string();
        let (mut palette, mut current) = (HashMap::new(), [0u8; 3]);
        let mut image = vec![None; w * h];
        let (mut x, mut band) = (0usize, 0usize);
        let bytes = rest.as_bytes();
        let mut i = 0;
        let number = |i: &mut usize| {
            let start = *i;
            while *i < bytes.len() && bytes[*i].is_ascii_digit() {
                *i += 1;
            }
            rest[start..*i].parse::<usize>().unwrap()
        };
        while i < bytes.len() {
            match bytes[i] {
                b'#' => {
                    i += 1;
                    let n = number(&mut i);
                    if bytes[i] == b';' {
                        let mut v = [0usize; 4];
                        for slot in &mut v {
                            i += 1;
                            *slot = number(&mut i);
                        }
                        palette.insert(n, [0, 1, 2].map(|k| ((v[k + 1] * 255 + 50) / 100) as u8));
                    }
                    current = palette[&n];
                }
                b'!' => {
                    i += 1;
                    let run = number(&mut i);
                    let value = bytes[i] - 63;
                    for k in 0..run {
                        paint(&mut image, w, h, x + k, band, value, current);
                    }
                    x += run;
                    i += 1;
                }
                b'$' => {
                    x = 0;
                    i += 1;
                }
                b'-' => {
                    x = 0;
                    band += 1;
                    i += 1;
                }
                value => {
                    paint(&mut image, w, h, x, band, value - 63, current);
                    x += 1;
                    i += 1;
                }
            }
        }
        (w, h, image)
    }

    fn paint(
        image: &mut [Option<[u8; 3]>],
        w: usize,
        h: usize,
        x: usize,
        band: usize,
        bits: u8,
        color: [u8; 3],
    ) {
        for dy in 0..6 {
            if bits >> dy & 1 == 1 && band * 6 + dy < h && x < w {
                image[(band * 6 + dy) * w + x] = Some(color);
            }
        }
    }

    fn frame(width: u32, height: u32, color: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| (0..1).map(move |_| (x, y))))
            .flat_map(|(x, y)| color(x, y))
            .collect()
    }

    #[test]
    fn encoding_round_trips_exact_colors_and_repeats_the_last_row_past_the_frame() {
        let rgb = frame(24, 20, |x, y| {
            [(x * 10) as u8, (y * 12) as u8, if x < 12 { 200 } else { 7 }]
        });
        let pixels = Pixels {
            rgb: &rgb,
            width: 24,
            height: 20,
        };
        // A 12 x 30 rectangle starting at (12, 0) runs 10 rows past the frame's bottom.
        let (w, h, image) = decode(&encode(&pixels, 12, 0, 12, 30));
        assert_eq!((w, h), (12, 30));
        for y in 0..30usize {
            for x in 0..12usize {
                let source = [((x + 12) * 10) as u8, (y.min(19) * 12) as u8, 7];
                let got = image[y * w + x].expect("every pixel is painted");
                // Sixel carries percentages: at most one percentage step of error.
                assert!(
                    got.iter().zip(source).all(|(a, b)| a.abs_diff(b) <= 3),
                    "({x},{y}) {got:?} vs {source:?}"
                );
            }
        }
    }

    #[test]
    fn more_colors_than_registers_still_encode_within_the_budget() {
        let rgb = frame(40, 40, |x, y| {
            [(x * 6) as u8, (y * 6) as u8, ((x + y) * 3) as u8]
        });
        let pixels = Pixels {
            rgb: &rgb,
            width: 40,
            height: 40,
        };
        let data = encode(&pixels, 0, 0, 40, 40);
        let registers = String::from_utf8_lossy(&data).matches(";2;").count();
        assert!(
            (1..=MAX_COLORS).contains(&registers),
            "{registers} registers"
        );
        assert!(decode(&data).2.iter().all(Option::is_some));
    }

    #[test]
    fn rectangles_cover_exactly_the_marked_cells_without_overlap() {
        let mut seed = 7u32;
        for (columns, rows) in [(1, 1), (7, 5), (30, 12), (12, 30)] {
            for density in [0, 30, 70, 100] {
                let mask: Vec<bool> = (0..columns * rows)
                    .map(|_| {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        (seed >> 16) % 100 < density
                    })
                    .collect();
                let mut covered = vec![0u8; columns * rows];
                for (l, t, r, b) in rectangles(&mask, columns, rows) {
                    for y in t..=b {
                        for x in l..=r {
                            covered[usize::from(y) * columns + usize::from(x)] += 1;
                        }
                    }
                }
                let expected: Vec<u8> = mask.iter().map(|m| u8::from(*m)).collect();
                assert_eq!(covered, expected, "{columns}x{rows} at {density}%");
            }
        }
    }

    /// Where the images in `output` landed, as (column, row, width, height).
    fn sent(output: &[u8]) -> Vec<(u16, u16, usize, usize)> {
        let text = String::from_utf8_lossy(output).to_string();
        let mut found = Vec::new();
        for piece in text.split("\x1b[").skip(1) {
            if let Some((position, rest)) = piece.split_once('H') {
                if let Some(start) = rest.find("\x1bP") {
                    let (row, column) = position.split_once(';').unwrap();
                    let raster = &rest[start..];
                    let fields: Vec<&str> = raster[raster.find('"').unwrap() + 1..]
                        .splitn(5, ';')
                        .collect();
                    let height: String =
                        fields[3].chars().take_while(char::is_ascii_digit).collect();
                    found.push((
                        column.parse::<u16>().unwrap() - 1,
                        row.parse::<u16>().unwrap() - 1,
                        fields[2].parse().unwrap(),
                        height.parse().unwrap(),
                    ));
                }
            }
        }
        found
    }

    /// The (column, row) cells, relative to the scene, that sent images cover.
    fn cells(
        images: &[(u16, u16, usize, usize)],
        area: Rect,
        (cw, ch): (u32, u32),
    ) -> std::collections::HashSet<(u16, u16)> {
        let mut found = std::collections::HashSet::new();
        for &(column, row, w, h) in images {
            for y in 0..(h as u32 / ch) as u16 {
                for x in 0..(w as u32 / cw) as u16 {
                    found.insert((column - area.x + x, row - area.y + y));
                }
            }
        }
        found
    }

    #[test]
    fn nearby_cells_share_one_image_but_text_cells_are_never_inside_one() {
        let (columns, rows) = (12usize, 6usize);
        let mark = |cells: &[(usize, usize)]| {
            let mut mask = vec![false; columns * rows];
            for (x, y) in cells {
                mask[y * columns + x] = true;
            }
            mask
        };
        let holds = |found: &[(u16, u16, u16, u16)], x: usize, y: usize| {
            found.iter().any(|(l, t, r, b)| {
                (*l..=*r).contains(&(x as u16)) && (*t..=*b).contains(&(y as u16))
            })
        };
        let changed = [(2, 1), (4, 1), (3, 3), (10, 5)];
        let dirty = mark(&changed);
        // A scatter of changed cells around one object, and one far away.
        let found = patches(&dirty, &mark(&[]), columns, rows);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(changed.iter().all(|(x, y)| holds(&found, *x, *y)));
        // Text in the middle of the group: more images, none touching it.
        let found = patches(&dirty, &mark(&[(3, 2)]), columns, rows);
        assert!(found.len() > 2);
        assert!(!holds(&found, 3, 2));
        assert!(changed.iter().all(|(x, y)| holds(&found, *x, *y)));
    }

    #[test]
    fn only_changed_or_freed_cells_are_resent_and_text_is_never_painted_over() {
        let (cols, rows, cw, ch) = (10u16, 6u16, 4u32, 8u32);
        let area = Rect::new(2, 3, cols, rows);
        let size = (u32::from(cols) * cw, u32::from(rows) * ch - 1);
        let mut canvas = Canvas::new(area, (cw, ch), size);
        let mut base = frame(size.0, size.1, |x, y| [(x * 5) as u8, (y * 5) as u8, 9]);
        let mut send = |canvas: &mut Canvas, rgb: &[u8], covered: &[(u16, u16)]| {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 12));
            canvas.mark(&mut buffer, Color::Rgb(1, 2, 3));
            for &(x, y) in covered {
                buffer[(area.x + x, area.y + y)].reset();
            }
            canvas.note_coverage(&buffer);
            let mut out = Vec::new();
            let frame = Pixels {
                rgb,
                width: size.0,
                height: size.1,
            };
            canvas.present(&frame, Route::Direct, &mut out).unwrap();
            out
        };
        // A blank frame first: nothing is sent until the area has been cleared.
        assert!(send(&mut canvas, &base, &[]).is_empty());
        assert_eq!(
            sent(&send(&mut canvas, &base, &[])),
            vec![(
                2,
                3,
                (cols as u32 * cw) as usize,
                (rows as u32 * ch) as usize
            )]
        );
        assert!(
            send(&mut canvas, &base, &[]).is_empty(),
            "an identical frame sends nothing"
        );
        // One pixel changes inside cell (4, 2): that cell and its neighbours are resent
        // as one image, so an object that moves stays a single patch.
        let at = ((2 * ch + 3) * size.0 + 4 * cw + 1) as usize * 3;
        base[at] ^= 0xff;
        assert_eq!(
            sent(&send(&mut canvas, &base, &[])),
            vec![(5, 4, 3 * cw as usize, 3 * ch as usize)]
        );
        // A label covers cells (3..=5, 2): they are never painted, even though the frame changes.
        let label: Vec<(u16, u16)> = (3..=5).map(|x| (x, 2)).collect();
        base[at] ^= 0xff;
        assert!(send(&mut canvas, &base, &label).is_empty());
        // The label moves away: its cells come back, as one image.
        let resent = cells(&sent(&send(&mut canvas, &base, &[])), area, (cw, ch));
        assert!(label.iter().all(|c| resent.contains(c)));
        // Anything that wipes the terminal starts over from a blank frame.
        canvas.invalidate();
        assert!(send(&mut canvas, &base, &[]).is_empty());
        assert_eq!(sent(&send(&mut canvas, &base, &[])).len(), 1);
        // `refresh` repaints everything in place, without blanking first.
        canvas.refresh();
        assert_eq!(sent(&send(&mut canvas, &base, &[])).len(), 1);
    }

    #[test]
    fn marking_leaves_scene_cells_alone_but_text_drawn_over_them_is_detected() {
        use ratatui::widgets::{Clear, Paragraph, Widget};
        let area = Rect::new(1, 1, 8, 4);
        let mut canvas = Canvas::new(area, (6, 15), (48, 60));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 6));
        canvas.mark(&mut buffer, Color::Rgb(9, 9, 9));
        assert!(
            buffer[(2, 2)].symbol() == " " && !buffer[(2, 2)].skip,
            "blank until the image goes live"
        );
        let pixels = Pixels {
            rgb: &[0; 48 * 60 * 3],
            width: 48,
            height: 60,
        };
        canvas
            .present(&pixels, Route::Direct, &mut Vec::new())
            .unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 6));
        canvas.mark(&mut buffer, Color::Rgb(9, 9, 9));
        assert!(
            buffer[(2, 2)].skip && !buffer[(0, 0)].skip,
            "only the scene is skipped"
        );
        let label = Rect::new(2, 2, 3, 1);
        Clear.render(label, &mut buffer);
        Paragraph::new("abc").render(label, &mut buffer);
        canvas.note_coverage(&buffer);
        let covered: Vec<usize> = canvas
            .covered
            .iter()
            .enumerate()
            .filter(|(_, c)| **c)
            .map(|(n, _)| n)
            .collect();
        assert_eq!(covered, vec![9, 10, 11], "exactly the label's cells");
    }

    #[test]
    fn tmux_passthrough_doubles_escapes_and_carries_an_absolute_cursor_move() {
        let mut out = Vec::new();
        // The pane starts at column 80, row 1 of the window; the image is at its (3, 5).
        Route::Tmux { left: 80, top: 1 }
            .write(&mut out, 3, 5, b"\x1bPq#0\x1b\\")
            .unwrap();
        assert_eq!(
            out,
            b"\x1bPtmux;\x1b\x1b[7;84H\x1b\x1bPq#0\x1b\x1b\\\x1b\\".to_vec()
        );
        let mut direct = Vec::new();
        Route::Direct.write(&mut direct, 3, 5, b"IMG").unwrap();
        assert_eq!(direct, b"\x1b[6;4HIMG".to_vec());
    }

    #[test]
    fn tmux_geometry_and_the_passthrough_setting_are_read_strictly() {
        // pane_left pane_top window_active client_height window_height
        assert_eq!(
            pane_route("80 0 1 69 69", false),
            Some(Some(Route::Tmux { left: 80, top: 0 }))
        );
        // A status line above the panes pushes them down by the client's extra row.
        assert_eq!(
            pane_route("0 0 1 70 69", true),
            Some(Some(Route::Tmux { left: 0, top: 1 }))
        );
        assert_eq!(
            pane_route("0 0 1 70 69", false),
            Some(Some(Route::Tmux { left: 0, top: 0 }))
        );
        // A window that is not on screen must not be drawn on.
        assert_eq!(pane_route("0 0 0 70 69", false), Some(None));
        assert_eq!(pane_route("garbage", false), None);
        assert!(passthrough_allowed("on") && passthrough_allowed("all"));
        assert!(!passthrough_allowed("off") && !passthrough_allowed(""));
    }

    #[test]
    fn a_different_route_sends_everything_again() {
        let (area, cell, size) = (Rect::new(0, 0, 4, 2), (4u32, 8u32), (16u32, 16u32));
        let mut canvas = Canvas::new(area, cell, size);
        let rgb = vec![7u8; (size.0 * size.1 * 3) as usize];
        let pixels = Pixels {
            rgb: &rgb,
            width: size.0,
            height: size.1,
        };
        let mut present = |canvas: &mut Canvas, route| {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 4));
            canvas.mark(&mut buffer, Color::Black);
            canvas.note_coverage(&buffer);
            let mut out = Vec::new();
            canvas.present(&pixels, route, &mut out).unwrap();
            out
        };
        assert!(present(&mut canvas, Route::Direct).is_empty());
        assert!(!present(&mut canvas, Route::Direct).is_empty());
        assert!(
            present(&mut canvas, Route::Direct).is_empty(),
            "nothing changed"
        );
        // The tmux pane moved: whatever was drawn at the old place is not trusted.
        let out = present(&mut canvas, Route::Tmux { left: 5, top: 0 });
        assert!(String::from_utf8_lossy(&out).starts_with("\x1bPtmux;"));
        assert!(present(&mut canvas, Route::Tmux { left: 5, top: 0 }).is_empty());
    }
}
