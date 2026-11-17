//! Original half-block station scenery. Only the shared panels represent data;
//! the train, sunlight and leaves are deterministic, decorative animation.
use super::*;

#[derive(Clone, Copy)]
struct Panels {
    machine: Rect,
    board: Rect,
}

fn canopy(height: i32) -> i32 {
    (height / 9).clamp(5, 10)
}

fn panels(area: Rect) -> Option<Panels> {
    if area.width < 160 || area.height < 40 {
        return None;
    }
    let machine = Rect::new(
        area.x + area.width * 46 / 100,
        area.y + canopy(i32::from(area.height) * 2) as u16 / 2 + 1,
        (area.width * 31 / 100).clamp(50, 84),
        11,
    );
    let board_x = machine.right() + 4;
    let bubble_top = area.y
        + ((cafe_counter(i32::from(area.height) * 2) - 14).max(0) as u16 / 2).saturating_sub(9);
    if machine.bottom() > bubble_top || board_x + 22 > area.right() {
        return None;
    }
    Some(Panels {
        machine,
        board: Rect::new(
            board_x,
            machine.y,
            area.right() - board_x - 2,
            (bubble_top - machine.y).min(11),
        ),
    })
}

pub(super) fn tv_area(area: Rect) -> Option<Rect> {
    panels(area).map(|panels| panels.board).or_else(|| {
        (area.width >= 70 && area.height >= 20)
            .then(|| Rect::new(area.x + 1, area.y + 4, area.width - 2, 2))
    })
}

pub(super) fn message_y(area: Rect) -> u16 {
    panels(area).map_or(area.y + 6.min(area.height), |p| p.machine.bottom() + 1)
}

/// Reuse real telemetry and its labels, including Intel's partial-coverage note.
/// The station's red service cabinet occupies the reference's vending-machine bay.
pub(super) fn render_system(
    frame: &mut Frame<'_>,
    area: Rect,
    pack: &Pack,
    sample: &super::super::system_metrics::Snapshot,
    ms: u64,
    names: &[String; 5],
    details: &[String; 5],
) -> bool {
    let Some(panels) = panels(area) else {
        if area.width >= 70 && area.height >= 20 {
            render_cafe_rack(
                frame,
                CafeRack {
                    area: Rect::new(area.x + 1, area.y + 1, area.width - 2, 3),
                    stacked: false,
                },
                pack,
                sample,
                ms,
                names,
                details,
            );
            return true;
        }
        return false;
    };
    let area = panels.machine;
    let mut c = Canvas::new(area.width as usize, area.height as usize * 2, pack);
    c.rect(0, 0, i32::from(area.width), i32::from(area.height) * 2, 'x');
    c.rect(
        1,
        1,
        i32::from(area.width) - 2,
        i32::from(area.height) * 2 - 2,
        'n',
    );
    for unit in 0..5 {
        let y = 2 + unit * 4;
        c.rect(0, y, 1, 3, 'o');
        c.rect(i32::from(area.width) - 1, y, 1, 3, 'r');
        if unit >= 3 {
            rack_device(
                &mut c,
                Rect::new(3, y as u16 + 2, 16, 2),
                unit as usize,
                sample,
                ms,
            );
        }
    }
    c.paint(frame, area);
    label(
        frame,
        Rect::new(area.x + 2, area.y, area.width - 4, 1),
        "Machine · LOCAL",
        pack.color('j'),
        pack.color('n'),
        true,
    );
    for unit in 0..5 {
        let y = area.y + 1 + unit as u16 * 2;
        label(
            frame,
            Rect::new(area.x + 3, y, area.width - 6, 1),
            &names[unit],
            pack.color(if rack_load(sample, unit).is_some() {
                'e'
            } else {
                'd'
            }),
            pack.color('n'),
            true,
        );
        if unit <= 2 {
            let values = sample.history.iter().map(|v| v[unit]).collect::<Vec<_>>();
            bar_chart(
                frame,
                Rect::new(area.x + 3, y + 1, 16, 1),
                &values,
                100.0,
                pack.color(['h', 'j', 'z'][unit]),
                pack.color('n'),
            );
        }
        label(
            frame,
            Rect::new(area.x + 20, y + 1, area.width - 23, 1),
            &details[unit],
            pack.color('k'),
            pack.color('n'),
            false,
        );
    }
    true
}

/// One train dwells, leaves to the right, then a new train arrives from the
/// left. Position depends only on scene time: pause freezes every layer.
fn train_offset(ms: u64, width: i32) -> Option<i32> {
    let t = ms % 36000;
    let ease = |x: f64| x * x * (3.0 - 2.0 * x);
    let distance = f64::from(width + 8);
    match t {
        0..12000 => Some(0),
        12000..18000 => Some((distance * ease((t - 12000) as f64 / 6000.0)) as i32),
        18000..25000 => None,
        _ => Some((-distance * (1.0 - ease((t - 25000) as f64 / 11000.0))) as i32),
    }
}

fn train_window(c: &mut Canvas<'_>, x: i32, y: i32, width: i32, height: i32, ms: u64) {
    if width < 8 || height < 10 {
        return;
    }
    // Draw into a separate clipped canvas: no train pixel can cross the pillars.
    let mut glass = Canvas::new(width as usize, height as usize, c.pack);
    glass.rect(0, 0, width, height, 's');
    glass.rect(0, height / 5, width, height, 'l');
    for hill in 0..width / 12 + 1 {
        let top = height / 4 + (rain_hash(hill as u64 * 27) % 5) as i32;
        glass.rect(hill * 12, top, 16, height - top, 'u');
    }
    glass.rect(0, height - 4, width, 4, 'n');
    glass.rect(0, height - 3, width, 1, 'k');
    if let Some(offset) = train_offset(ms, width) {
        let top = (height / 6).max(2);
        let bottom = height - 3;
        glass.rect(offset - 3, top, width + 6, bottom - top, 'l');
        glass.rect(offset - 3, top, width + 6, 2, 'e');
        glass.rect(offset - 3, top + 3, width + 6, 2, 'j');
        glass.rect(offset - 3, bottom - 5, width + 6, 2, 'j');
        glass.rect(offset - 3, bottom - 2, width + 6, 2, 'u');
        let door = (width / 3).clamp(12, 26);
        for bay in -1..width / door + 2 {
            let px = offset + bay * door;
            let window_y = top + 7;
            let window_h = (bottom - window_y - 6).max(4);
            glass.rect(px, window_y - 1, door - 3, window_h + 2, 'k');
            glass.rect(px + 1, window_y, door - 5, window_h, 'v');
            glass.rect(px + 2, window_y + 1, 1, window_h - 2, 'u');
            glass.rect(px + 1, window_y + window_h * 2 / 3, door - 5, 1, 's');
            // A narrow door seam and softly reflected sky distinguish each bay.
            glass.rect(px + door - 2, top + 5, 1, bottom - top - 6, 'u');
        }
    }
    for py in 0..height {
        for px in 0..width {
            if let Some(target) = c.pixel_mut(x + px, y + py) {
                *target = glass.pixels[(py * width + px) as usize];
            }
        }
    }
    c.rect(x - 2, y - 2, width + 4, 2, 'r');
    c.rect(x - 2, y, 2, height, 't');
    c.rect(x + width, y, 2, height, 't');
    c.rect(x - 2, y + height, width + 4, 2, 'm');
    c.rect(x + width / 2, y, 1, height, 'r');
}

fn tree(c: &mut Canvas<'_>, w: i32, platform: i32, ms: u64) {
    let trunk = (w / 25).max(2);
    c.rect(trunk, 3, 2, platform - 3, 'r');
    for segment in 0..12 {
        c.rect(trunk + segment, 12 - segment / 2, 3, 2, 'r');
    }
    // Clustered leaves, not independently blinking random pixels.
    let sway = ((ms as f64 / 2300.0).sin() * 1.2).round() as i32;
    for cluster in 0..22u64 {
        let hash = rain_hash(cluster * 97 + 43);
        let x = (hash % (w / 3).max(1) as u64) as i32 - 5 + sway;
        let y = (hash / 101 % 10) as i32;
        let width = 5 + (hash / 31 % 9) as i32;
        let role = if cluster % 3 == 0 { 's' } else { 'l' };
        c.rect(x + 2, y, width - 2, 1, role);
        c.rect(x, y + 1, width, 2, role);
        c.rect(x + 1, y + 3, width - 3, 1, role);
    }
}

fn leaves(c: &mut Canvas<'_>, w: i32, platform: i32, ms: u64) {
    for leaf in 0..7u64 {
        let phase = (ms + leaf * 2701) % 19000;
        let t = phase as f64 / 19000.0;
        let x = (w as f64 * (0.03 + t * 0.90) + (t * 13.0 + leaf as f64).sin() * 3.0) as i32;
        let y = 8 + ((platform - 5) as f64 * t) as i32;
        c.rect(x, y, 2, 1, if leaf % 2 == 0 { 'l' } else { 's' });
        if phase % 1600 < 800 {
            c.rect(x + 1, y - 1, 1, 1, 'l');
        }
    }
}

pub(super) fn render(
    frame: &mut Frame<'_>,
    layout: &SceneLayout,
    pack: &Pack,
    accounts: &[&SceneAccount],
    connection: &str,
    ms: u64,
    all_limits: bool,
) {
    let area = layout.stage;
    let mut c = Canvas::new(area.width as usize, area.height as usize * 2, pack);
    let (w, h) = (area.width as i32, area.height as i32 * 2);
    if w == 0 || h == 0 {
        return;
    }
    let platform = cafe_counter(h);
    let roof = canopy(h);
    c.rect(0, 0, w, h, 'w');
    // Timber siding and the broad diagonal afternoon sunbeam on the right.
    for x in (w / 2..w).step_by(4) {
        c.rect(x, roof + 3, 1, platform - roof, 'g');
    }
    for y in roof + 2..platform {
        let edge = w - (y - roof) * w / (platform - roof).max(1) / 2;
        c.rect(edge, y, w - edge, 1, 'k');
        for x in (edge..w).filter(|x| x % 4 == 0) {
            c.rect(x, y, 1, 1, 'o');
        }
    }
    if w >= 50 && h >= 36 {
        train_window(
            &mut c,
            w * 9 / 100,
            roof + 4,
            w * 35 / 100,
            platform - roof - 9,
            ms,
        );
        // The red vending bay is also the host's machine-telemetry cabinet.
        let (vx, vw) = (w * 46 / 100, (w * 31 / 100).clamp(18, 84));
        c.rect(vx - 1, roof + 2, vw + 2, platform - roof - 3, 'r');
        c.rect(vx, roof + 3, vw, platform - roof - 4, 'x');
        let shelf_y = platform - 16;
        for shelf in 0..3 {
            let y = shelf_y + shelf * 4;
            c.rect(vx + 3, y, vw - 6, 3, 'n');
            for bottle in 0..(vw - 8) / 5 {
                c.rect(
                    vx + 5 + bottle * 5,
                    y,
                    2,
                    2,
                    ['k', 'l', 'x'][bottle as usize % 3],
                );
            }
            c.rect(vx + 2, y + 3, vw - 4, 1, 'm');
        }
        c.rect(vx + vw - 6, platform - 8, 3, 4, 'n');
        c.rect(vx + 4, platform - 3, vw / 2, 1, 'i');
        // Slatted bench catches the light beneath the departures/data board.
        let bx = w * 80 / 100;
        for slat in 0..3 {
            c.rect(bx, platform - 12 + slat * 3, w - bx - 3, 2, 'm');
            c.rect(bx, platform - 12 + slat * 3, w - bx - 3, 1, 'o');
        }
        c.rect(bx - 1, platform - 4, w - bx, 2, 'o');
        for leg in [bx + 1, w - 6] {
            c.rect(leg, platform - 2, 2, 3, 'r');
        }
    }
    // Tiled canopy, deep crossbeam, joinery and hanging box lanterns.
    c.rect(0, 0, w, roof, 'm');
    for x in (0..w).step_by(4) {
        c.rect(x, 1, 1, roof - 1, 'o');
        c.rect(x + 1, 1, 1, roof - 1, 'x');
    }
    c.rect(0, roof, w, 2, 'r');
    for px in [w / 18, w * 44 / 100, w * 78 / 100] {
        c.rect(px, roof + 1, 2, platform - roof, 'r');
        c.rect(px + 2, roof + 2, 1, platform - roof - 1, 't');
        for step in 0..6 {
            c.rect(px - step, roof + 2 + step, 2, 1, 'r');
        }
    }
    for px in [w / 5, w * 84 / 100] {
        c.rect(px, roof + 1, 1, 3, 'i');
        c.rect(px - 1, roof + 4, 4, 6, 't');
        c.rect(px, roof + 4, 2, 5, 'j');
        c.glow(px, roof + 7, 7, 'j', 14);
    }
    if w >= 50 {
        tree(&mut c, w, platform, ms);
    }
    // A continuous platform gives every cat and its bubble the café's anchors.
    c.rect(0, platform, w, h - platform, 'f');
    c.rect(0, platform, w, 2, 'k');
    c.rect(0, platform + 2, w, 2, 'm');
    c.rect(0, platform + 4, w, 1, 'r');
    for tile in (0..w).step_by(9) {
        c.rect(tile, platform + 3, 1, 2, 'o');
    }
    // Track bed and ties stay behind the footer, visible on tall terminals.
    for tie in (0..w).step_by(8) {
        c.rect(tie, platform + 6, 3, h - platform, 'i');
    }
    c.rect(0, h - 2, w, 1, 't');
    if w >= 50 && h >= 36 {
        leaves(&mut c, w, platform, ms);
        let state = match connection {
            "CONNECTED" => 'h',
            "CONNECTING" => 'c',
            _ => 'a',
        };
        c.rect(w - 5, platform - 5, 2, 2, state);
    }
    let residents = if accounts.is_empty() {
        layout.capacity.min(2)
    } else {
        accounts.len()
    };
    for index in 0..residents {
        let card = layout.cards[index];
        let x = i32::from(card.x - area.x + card.width / 2) - 6;
        let account = accounts.get(index).copied();
        c.cafe_cat(
            index,
            account,
            x,
            platform,
            ms,
            actor_motion(layout, index, account, ms, all_limits),
        );
    }
    c.paint(frame, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> Pack {
        Pack::parse(include_bytes!("plugins/pixel-station.json")).unwrap()
    }

    #[test]
    fn station_pack_is_independent_and_theme_order_wraps_without_the_current_theme() {
        let pack = pack();
        assert_eq!(pack.theme, Theme::Station);
        for name in STATION_SPRITES {
            assert!(pack.sprites.contains_key(*name));
        }
        assert!(!pack.sprites.contains_key("agent"));
        assert_eq!(
            Theme::Cafe.alternatives().collect::<Vec<_>>(),
            [Theme::Office, Theme::Station]
        );
        assert_eq!(
            Theme::Station.alternatives().collect::<Vec<_>>(),
            [Theme::Cafe, Theme::Office]
        );
        let mut incomplete: Value =
            serde_json::from_slice(include_bytes!("plugins/pixel-station.json")).unwrap();
        incomplete["sprites"]
            .as_object_mut()
            .unwrap()
            .remove("cat_sleep");
        assert!(Pack::parse(&serde_json::to_vec(&incomplete).unwrap()).is_err());
        // Scene-only palette colors remain mandatory when sprites are plain.
        let mut plain: Value =
            serde_json::from_slice(include_bytes!("plugins/pixel-station.json")).unwrap();
        for sprite in plain["sprites"].as_object_mut().unwrap().values_mut() {
            sprite["frames"] = json!([["b"]]);
        }
        for role in ['x', 'l', 's'] {
            let mut invalid = plain.clone();
            invalid["palette"]
                .as_object_mut()
                .unwrap()
                .remove(&role.to_string());
            assert!(Pack::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
    }

    #[test]
    fn train_dwells_departs_and_arrives_smoothly_and_stays_inside_its_opening() {
        assert_eq!(train_offset(0, 45), Some(0));
        assert_eq!(train_offset(11999, 45), Some(0));
        assert_eq!(train_offset(20000, 45), None);
        assert_eq!(train_offset(25000, 45), Some(-53));
        assert_eq!(train_offset(35999, 45), Some(0));
        for start in [12000, 25000] {
            let offsets = (start..start + 6000)
                .step_by(20)
                .filter_map(|ms| train_offset(ms, 45))
                .collect::<Vec<_>>();
            assert!(offsets
                .windows(2)
                .all(|pair| pair[1] >= pair[0] && pair[1] - pair[0] <= 1));
        }
        let pack = pack();
        for ms in [0, 12000, 15000, 17999, 20000, 25000, 30000, 35999] {
            let mut c = Canvas::new(80, 60, &pack);
            train_window(&mut c, 8, 8, 45, 32, ms);
            let original = c.pixels.clone();
            for (i, color) in c.pixels.iter().enumerate() {
                if *color != pack.color('b') {
                    assert!((6..55).contains(&(i % 80)) && (6..42).contains(&(i / 80)));
                }
            }
            let mut repeated = Canvas::new(80, 60, &pack);
            train_window(&mut repeated, 8, 8, 45, 32, ms);
            assert_eq!(
                original, repeated.pixels,
                "same paused clock must produce the same train"
            );
        }
    }

    #[test]
    fn station_panels_keep_clear_of_expanded_bubbles_and_moving_heads() {
        let mut cat = SceneAccount::models_cat();
        cat.models = None;
        cat.limits = (0..4)
            .map(|i| (format!("Window {i}"), Some(30.0)))
            .collect();
        for (w, h) in [(160, 40), (180, 42), (240, 56), (240, 76), (300, 90)] {
            let area = Rect::new(2, 3, w, h);
            let panels = panels(area).unwrap();
            assert!(panels.machine.intersection(panels.board).is_empty());
            assert_eq!(panels.board.intersection(area), panels.board);
            let layout = SceneLayout::for_accounts(area, Theme::Station, &[&cat], true);
            for ms in (0..180000).step_by(211) {
                let bubble = speech_bubble(&layout, 0, &cat, Theme::Station, ms, true).0;
                for panel in [panels.machine, panels.board] {
                    assert!(
                        panel.bottom() <= bubble.y,
                        "{w}x{h} {ms}: {panel:?} / {bubble:?}"
                    );
                }
            }
        }
        for (w, h) in [(1, 1), (24, 10), (80, 24), (120, 40)] {
            assert!(panels(Rect::new(0, 0, w, h)).is_none());
        }
    }
}
