//! Shared authentication feedback. This contains no login requests or credentials.
use super::{label, mix, Canvas, Pack};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Hidden,
    Required,
    Pending,
    Checking,
    Failed,
    Unlocked,
}

pub(super) struct LoginAnimation {
    phase: Phase,
    since: u64,
    unlocked_at: Option<Instant>,
}

impl Default for LoginAnimation {
    fn default() -> Self {
        Self {
            phase: Phase::Hidden,
            since: 0,
            unlocked_at: None,
        }
    }
}

impl LoginAnimation {
    fn transition(&mut self, phase: Phase, ms: u64) {
        if self.phase != phase {
            self.phase = phase;
            self.since = ms;
            self.unlocked_at = (phase == Phase::Unlocked).then(Instant::now);
        }
    }

    pub(super) fn started(&mut self, ms: u64) {
        self.transition(Phase::Pending, ms);
    }

    pub(super) fn accepted(&mut self, ms: u64) {
        if self.phase == Phase::Pending {
            // A successful POST/cookie is not proof that the next session works.
            self.transition(Phase::Checking, ms);
        }
    }

    pub(super) fn failed(&mut self, ms: u64) {
        if matches!(self.phase, Phase::Pending | Phase::Checking) {
            self.transition(Phase::Failed, ms);
        }
    }

    pub(super) fn observe(&mut self, session: &Value, ms: u64) {
        match (
            session.get("enabled").and_then(Value::as_bool),
            session.get("authenticated").and_then(Value::as_bool),
        ) {
            (Some(false), _) => self.transition(Phase::Hidden, ms),
            (Some(true), Some(true)) => {
                if matches!(
                    self.phase,
                    Phase::Required | Phase::Checking | Phase::Failed
                ) {
                    self.transition(Phase::Unlocked, ms);
                }
            }
            (Some(true), Some(false)) => match self.phase {
                Phase::Checking => self.transition(Phase::Failed, ms),
                Phase::Pending | Phase::Failed => {}
                _ => self.transition(Phase::Required, ms),
            },
            // Unknown startup state and data cleared during login are not logout.
            _ => {}
        }
    }

    pub(super) fn visual(&self, ms: u64) -> Option<Visual> {
        self.visual_at(ms, Instant::now())
    }

    fn visual_at(&self, ms: u64, now: Instant) -> Option<Visual> {
        if self.phase == Phase::Hidden
            || self
                .unlocked_at
                .is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_millis(1500))
        {
            return None;
        }
        Some(Visual {
            phase: self.phase,
            age: ms.saturating_sub(self.since),
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Visual {
    pub(super) phase: Phase,
    pub(super) age: u64,
}

impl Visual {
    pub(super) fn required(ms: u64) -> Self {
        Self {
            phase: Phase::Required,
            age: ms,
        }
    }

    pub(super) fn locked(self) -> bool {
        self.phase != Phase::Unlocked
    }

    fn prompt(self, width: u16) -> &'static str {
        match self.phase {
            Phase::Pending => "Logging in…",
            Phase::Checking => "Verifying session…",
            Phase::Failed if width >= 27 => "Login failed · [o] Retry",
            Phase::Failed => "[o] Retry login",
            Phase::Unlocked => "Welcome back!",
            _ if width >= 30 => "Login required · [o] Log in",
            _ if width >= 18 => "Login · [o] Log in",
            _ => "[o] Log in",
        }
    }
}

/// Identical composition in every theme; palette roles alone adapt to the room.
pub(super) fn render(frame: &mut Frame<'_>, area: Rect, pack: &Pack, visual: Visual) {
    if area.is_empty() {
        return;
    }
    let width = area.width.min(44);
    let height = area.height.min(16);
    let panel = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let bg = mix(pack.color('b'), pack.color('r'), 80);
    let accent = pack.color(if visual.phase == Phase::Failed {
        'a'
    } else if visual.phase == Phase::Unlocked {
        'h'
    } else {
        'j'
    });
    frame.render_widget(Clear, panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(bg));
    let inner = if width >= 16 && height >= 4 {
        let inner = block.inner(panel);
        frame.render_widget(block, panel);
        inner
    } else {
        frame.render_widget(Block::default().style(Style::default().bg(bg)), panel);
        panel
    };
    let prompt = Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height));
    frame.render_widget(
        Paragraph::new(super::clipped(visual.prompt(inner.width), inner.width))
            .alignment(Alignment::Center)
            .style(Style::default().fg(pack.color('e')).bg(bg)),
        prompt,
    );
    if inner.height < 6 || inner.width < 16 {
        return;
    }
    let art_height = inner.height.saturating_sub(3).min(10);
    let art_width = inner.width.min(38);
    let art = Rect::new(
        inner.x + (inner.width - art_width) / 2,
        inner.y + 2,
        art_width,
        art_height,
    );
    let canvas = artwork(pack, visual, bg);
    // Keep native pixel proportions on roomy terminals; compact ones scale down.
    canvas.paint(frame, art);
    let hint = match visual.phase {
        Phase::Pending | Phase::Checking => "Please wait · ? help",
        Phase::Failed => "Still locked · try again",
        Phase::Unlocked => "Session confirmed",
        _ => "Your cats are waiting",
    };
    if inner.height > art_height + 2 {
        label(
            frame,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            "",
            pack.color('k'),
            bg,
            false,
        );
        frame.render_widget(
            Paragraph::new(super::clipped(hint, inner.width))
                .alignment(Alignment::Center)
                .style(Style::default().fg(pack.color('k')).bg(bg)),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

fn artwork<'a>(pack: &'a Pack, visual: Visual, bg: Color) -> Canvas<'a> {
    let mut c = Canvas::new(38, 20, pack);
    c.pixels.fill(bg);
    let t = visual.age as f64;
    let unlocked = visual.phase == Phase::Unlocked;
    let working = matches!(visual.phase, Phase::Pending | Phase::Checking);
    let idle = visual.age % 6400;
    let tap = !working && !unlocked && (3400..4300).contains(&idle);
    let reach = if tap {
        (((idle - 3400) as f64 / 900.0 * std::f64::consts::PI).sin() * 3.0).round() as i32
    } else {
        0
    };
    let hop = if unlocked && visual.age < 900 {
        ((t / 900.0 * std::f64::consts::PI).sin() * 4.0).round() as i32
    } else {
        0
    };
    let bob = if unlocked {
        0
    } else {
        (t / 900.0).sin().round() as i32
    };
    let accent = pack.color(if visual.phase == Phase::Failed {
        'a'
    } else if unlocked {
        'h'
    } else {
        'j'
    });
    let shine = mix(accent, pack.color('e'), 90);
    let shadow = mix(bg, accent, (40.0 + (t / 1200.0).sin() * 6.0).round() as u16);
    // Soft halo; failure never opens the lock.
    c.color_rect(21, 4 + bob, 14, 12, shadow);
    c.color_rect(20, 6 + bob, 16, 8, shadow);
    let shackle = if unlocked { 3 } else { 0 };
    c.color_rect(25 + shackle, 3 + bob, 5, 1, shine);
    c.color_rect(24 + shackle, 4 + bob, 1, 5, shine);
    c.color_rect(
        30 + shackle,
        4 + bob,
        1,
        if unlocked { 2 } else { 5 },
        shine,
    );
    c.color_rect(23, 8 + bob, 10, 7, accent);
    c.color_rect(24, 9 + bob, 8, 1, shine);
    c.color_rect(27, 11 + bob, 2, 2, bg);
    c.color_rect(27, 13 + bob, 1, 1, bg);
    // A fixed original host cat: never dependent on a theme's sprite schema.
    const CAT: [&str; 12] = [
        "..f...f.....",
        ".fff.fff....",
        ".fffffff....",
        ".fefffef....",
        "..ffnff.....",
        "..fffff.....",
        "..fffffff...",
        "..ffffffff..",
        "..fffffffff.",
        "..ssfffss...",
        ".fff..fff...",
        "............",
    ];
    let fur = Color::Rgb(225, 176, 92);
    let low = mix(fur, bg, 75);
    let blink = !unlocked && visual.age % 4700 > 4490;
    for (dy, row) in CAT.iter().enumerate() {
        for (dx, role) in row.chars().enumerate() {
            let color = match role {
                'f' => fur,
                's' => low,
                'e' if blink => low,
                'e' | 'n' => pack.color('b'),
                _ => continue,
            };
            c.color_rect(5 + dx as i32, 6 + dy as i32 - hop, 1, 1, color);
        }
    }
    // A relaxed tail sweeps from the hips; the paw reaches only during a tap.
    for i in 0..9 {
        let x = 14.0 + i as f64 * 0.5;
        let y = 16.0 - hop as f64 - i as f64 * 0.35
            + (t / 1100.0 + i as f64 / 5.0).sin() * i as f64 / 12.0;
        c.soft_pixel(x, y, fur, 1.0);
    }
    if reach > 0 {
        c.color_rect(13, 12 - reach, 7, 2, fur);
        c.color_rect(19, 11 - reach, 4, 2, fur);
    }
    if working {
        // Only a real pending login turns the key. No idle spinner/fake progress.
        let turn = (visual.age / 180) % 4;
        let key = pack.color('e');
        if turn % 2 == 0 {
            c.color_rect(17, 11 + bob, 6, 1, key);
            c.color_rect(16, 10 + bob, 2, 3, key);
            c.color_rect(21, 12 + bob, 1, 1, key);
        } else {
            c.color_rect(20, 8 + bob, 1, 6, key);
            c.color_rect(19, 7 + bob, 3, 2, key);
            c.color_rect(21, 12 + bob, 1, 1, key);
        }
    } else if unlocked {
        for (x, y) in [(20, 3), (34, 6), (19, 12)] {
            c.color_rect(x, y, 1, 1, shine);
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SOURCES: [&[u8]; 3] = [
        include_bytes!("plugins/pixel-cafe.json"),
        include_bytes!("plugins/pixel-office.json"),
        include_bytes!("plugins/pixel-station.json"),
    ];

    #[test]
    fn only_confirmed_sessions_unlock_and_unknown_data_is_not_authentication() {
        let mut state = LoginAnimation::default();
        state.observe(&Value::Null, 0);
        state.observe(&json!({"enabled":false}), 10);
        state.observe(&json!({"enabled":true,"authenticated":true}), 20);
        assert!(state.visual(20).is_none());
        state.observe(&json!({"enabled":true,"authenticated":false}), 30);
        assert_eq!(state.phase, Phase::Required);
        state.started(50);
        state.observe(&json!({"enabled":true,"authenticated":true}), 60);
        assert_eq!(state.phase, Phase::Pending);
        state.accepted(70);
        state.observe(&Value::Null, 80);
        assert_eq!(state.phase, Phase::Checking);
        state.observe(&json!({"enabled":true,"authenticated":true}), 90);
        assert_eq!(state.phase, Phase::Unlocked);
        state.observe(&json!({"enabled":true,"authenticated":true}), 100);
        assert_eq!(state.since, 90);
        // Wall-clock dismissal also works while the scene clock is paused.
        let at = state.unlocked_at.unwrap();
        assert!(state
            .visual_at(90, at + Duration::from_millis(1499))
            .is_some());
        assert!(state
            .visual_at(90, at + Duration::from_millis(1500))
            .is_none());
        state.observe(&json!({"enabled":true,"authenticated":false}), 110);
        assert_eq!(state.phase, Phase::Required);
    }

    #[test]
    fn failures_stay_locked_allow_retry_and_never_celebrate_disabled_auth() {
        let mut state = LoginAnimation::default();
        state.started(10);
        state.failed(20);
        state.observe(&json!({"enabled":true,"authenticated":false}), 30);
        assert_eq!(state.phase, Phase::Failed);
        assert_eq!(state.since, 20);
        assert!(state.visual(30).unwrap().locked());
        state.started(40);
        state.accepted(50);
        state.observe(&json!({"enabled":true,"authenticated":false}), 60);
        assert_eq!(state.phase, Phase::Failed);
        state.failed(70);
        assert_eq!(state.since, 60);
        state.started(80);
        state.accepted(90);
        state.observe(&json!({"enabled":false,"authenticated":true}), 100);
        assert!(state.visual(100).is_none());
        state.failed(110); // Non-login command errors are irrelevant.
        assert_eq!(state.phase, Phase::Hidden);
    }

    #[test]
    fn shared_art_is_deterministic_animates_and_never_uses_theme_sprites() {
        for source in SOURCES {
            let mut pack = Pack::parse(source).unwrap();
            pack.sprites.clear();
            let bg = pack.color('b');
            for phase in [
                Phase::Required,
                Phase::Pending,
                Phase::Checking,
                Phase::Failed,
                Phase::Unlocked,
            ] {
                let visual = Visual { phase, age: 300 };
                let a = artwork(&pack, visual, bg);
                assert_eq!(a.pixels, artwork(&pack, visual, bg).pixels);
                assert_ne!(
                    a.pixels,
                    artwork(&pack, Visual { phase, age: 1200 }, bg).pixels
                );
                assert!(a
                    .pixels
                    .iter()
                    .any(|color| *color == Color::Rgb(225, 176, 92)));
            }
        }
    }

    #[test]
    fn all_phases_fit_responsive_panels_and_keep_login_action_visible() {
        use ratatui::{backend::TestBackend, Terminal};
        for source in SOURCES {
            let pack = Pack::parse(source).unwrap();
            for (w, h) in [(1, 1), (12, 3), (20, 8), (38, 18), (80, 24), (180, 42)] {
                for phase in [
                    Phase::Required,
                    Phase::Pending,
                    Phase::Checking,
                    Phase::Failed,
                    Phase::Unlocked,
                ] {
                    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                    terminal
                        .draw(|frame| {
                            render(frame, frame.area(), &pack, Visual { phase, age: 700 })
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                    if w >= 12 && matches!(phase, Phase::Required | Phase::Failed) {
                        assert!(text.contains("[o]"));
                    }
                    if w >= 80 && phase == Phase::Required {
                        assert!(text.contains("Login required · [o] Log in"));
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "writes local authentication scene and motion previews"]
    fn write_login_previews() {
        use ratatui::{backend::TestBackend, Terminal};
        let directory = std::path::Path::new("target/iogw-previews");
        std::fs::create_dir_all(directory).unwrap();
        let css = |color| match color {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => "#101d22".into(),
        };
        for source in SOURCES {
            let pack = Pack::parse(source).unwrap();
            for (w, h) in [(20, 8), (80, 24), (180, 42)] {
                for phase in [
                    Phase::Required,
                    Phase::Pending,
                    Phase::Checking,
                    Phase::Failed,
                    Phase::Unlocked,
                ] {
                    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                    terminal
                        .draw(|frame| {
                            super::super::render_scene(
                                frame,
                                &pack,
                                &[],
                                "AUTH",
                                700,
                                0,
                                0,
                                false,
                                None,
                                false,
                                true,
                                &Default::default(),
                                None,
                                &Default::default(),
                                None,
                                Some(Visual { phase, age: 700 }),
                            )
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    let mut html = String::from("<!doctype html><meta charset=utf-8><style>body{background:#0c1015;margin:20px}pre{font:14px/16px 'DejaVu Sans Mono',monospace;white-space:pre}span{display:inline-block;height:16px;vertical-align:top}</style><pre>");
                    for row in buffer.content.chunks(w as usize) {
                        for cell in row {
                            let symbol = cell
                                .symbol()
                                .replace('&', "&amp;")
                                .replace('<', "&lt;")
                                .replace('>', "&gt;");
                            html.push_str(&format!(
                                "<span style=\"color:{};background:{}\">{symbol}</span>",
                                css(cell.fg),
                                css(cell.bg)
                            ));
                        }
                        html.push('\n');
                    }
                    html.push_str("</pre>");
                    std::fs::write(
                        directory.join(format!("login-{}-{phase:?}-{w}.html", pack.theme.id())),
                        html,
                    )
                    .unwrap();
                }
            }
        }
        // A contact sheet that actually plays the same Rust-generated frames.
        let pack = Pack::parse(SOURCES[0]).unwrap();
        let clips: Vec<_> = [
            Phase::Required,
            Phase::Pending,
            Phase::Failed,
            Phase::Unlocked,
        ]
        .into_iter()
        .map(|phase| {
            let frames: Vec<_> = (0..128)
                .map(|i| {
                    artwork(&pack, Visual { phase, age: i * 50 }, pack.color('b'))
                        .pixels
                        .into_iter()
                        .map(css)
                        .collect::<Vec<_>>()
                })
                .collect();
            json!({"name":format!("{phase:?}"),"frames":frames})
        })
        .collect();
        let html = format!(
            r#"<!doctype html><meta charset=utf-8><style>body{{background:#10151b;color:#ddd;font:16px monospace;display:flex;gap:32px}}canvas{{width:304px;height:160px;image-rendering:pixelated}}</style><script>const clips={};window.onload=()=>{{for(const c of clips){{let d=document.createElement('div');d.innerHTML='<p>'+c.name+'</p><canvas width=38 height=20></canvas>';document.body.append(d);c.ctx=d.querySelector('canvas').getContext('2d')}}setInterval(()=>{{for(const c of clips){{const f=c.frames[Math.floor(performance.now()/50)%c.frames.length];f.forEach((color,i)=>{{c.ctx.fillStyle=color;c.ctx.fillRect(i%38,Math.floor(i/38),1,1)}})}}}},50)}}</script>"#,
            serde_json::to_string(&clips).unwrap()
        );
        std::fs::write(directory.join("login-motion.html"), html).unwrap();
    }
}
