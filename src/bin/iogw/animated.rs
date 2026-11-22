//! Optional, data-only scene packs. No plugin code, paths, fonts or network access.
use super::{
    account_snapshot_values, config_dir, provider_label, quota_matches_account, GatewayData,
};
use clap::{Args, Subcommand};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[path = "login_animation.rs"]
mod login_animation;
#[path = "station.rs"]
mod station;

const MAX_BYTES: u64 = 256 * 1024;
const REQUIRED_COLORS: &str = "bwfgtimohcdarzqjvunek";
const CAFE_SPRITES: &[&str] = &[
    "cat",
    "cat_light",
    "cat_sleep",
    "plant",
    "mug",
    "terminal",
    "pastry",
    "cat_sign",
    "lock",
];
const OFFICE_SPRITES: &[&str] = &["desk", "agent", "plant", "terminal", "lock"];
const STATION_SPRITES: &[&str] = &["cat", "cat_light", "cat_sleep", "plant", "lock"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Theme {
    Cafe,
    Office,
    Station,
}

impl Theme {
    const ALL: [Self; 3] = [Self::Cafe, Self::Office, Self::Station];
    fn id(self) -> &'static str {
        match self {
            Self::Cafe => "pixel-cafe",
            Self::Office => "pixel-office",
            Self::Station => "pixel-station",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Cafe => "Café",
            Self::Office => "Office",
            Self::Station => "Station",
        }
    }
    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.id() == id)
    }
    fn path(self) -> PathBuf {
        plugins_dir().join(format!("{}.json", self.id()))
    }
    fn required_sprites(self) -> &'static [&'static str] {
        match self {
            Self::Cafe => CAFE_SPRITES,
            Self::Office => OFFICE_SPRITES,
            Self::Station => STATION_SPRITES,
        }
    }

    fn extra_colors(self) -> &'static str {
        // Station scenery uses foliage and brick colors even if a custom
        // pack's cat frames do not reference them. Validate before rendering.
        if self == Self::Station {
            "xls"
        } else {
            ""
        }
    }

    fn on_platform(self) -> bool {
        matches!(self, Self::Cafe | Self::Station)
    }

    fn alternatives(self) -> impl Iterator<Item = Self> {
        let current = Self::ALL.iter().position(|theme| *theme == self).unwrap();
        (1..Self::ALL.len()).map(move |step| Self::ALL[(current + step) % Self::ALL.len()])
    }
}

fn plugins_dir() -> PathBuf {
    config_dir().join("iogw/plugins")
}

pub(super) fn any_installed() -> bool {
    Theme::ALL.into_iter().any(|theme| theme.path().is_file())
}

#[derive(Args, Debug)]
pub(super) struct PluginsCommand {
    #[command(subcommand)]
    action: PluginsAction,
}

#[derive(Subcommand, Debug)]
enum PluginsAction {
    /// Validate and install a local data-only scene pack (no executable code).
    Install { path: PathBuf },
    /// Show installed scene packs and their locations.
    List,
    /// Remove the optional scene pack. The normal UI is unaffected.
    Remove { name: String },
}

pub(super) fn command(command: PluginsCommand) -> Result<Value, String> {
    match command.action {
        PluginsAction::Install { path } => {
            let bytes = read_bounded(&path)?;
            let pack = Pack::parse(&bytes)?;
            let target = pack.theme.path();
            install(&target, &bytes)?;
            Ok(
                json!({"installed": pack.theme.id(), "version": pack.version, "path": target,
                "shortcut": "F4/Ctrl+V: switch UI; t: theme; h/l or Left/Right: cat; j/k: account/model; Space: pause; Esc: return; PageUp/PageDown: page"}),
            )
        }
        PluginsAction::List => {
            let mut plugins = Vec::new();
            for theme in Theme::ALL {
                let path = theme.path();
                if path.exists() {
                    match Pack::load(&path) {
                        Ok(pack) => plugins
                            .push(json!({"id": theme.id(), "version": pack.version, "path": path})),
                        Err(error) => {
                            plugins.push(json!({"id": theme.id(), "path": path, "error": error}))
                        }
                    }
                }
            }
            Ok(json!({"plugins": plugins, "path": plugins_dir()}))
        }
        PluginsAction::Remove { name } => {
            let theme = Theme::from_id(&name).ok_or_else(|| format!("unknown plugin: {name}"))?;
            let target = theme.path();
            match fs::remove_file(&target) {
                Ok(()) => Ok(json!({"removed": theme.id(), "path": target})),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    Ok(json!({"removed": false, "path": target}))
                }
                Err(err) => Err(err.to_string()),
            }
        }
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    if !file.metadata().map_err(|err| err.to_string())?.is_file() {
        return Err("plugin must be a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("plugin exceeds the 256 KiB limit".into());
    }
    Ok(bytes)
}

fn install(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = target.parent().ok_or("invalid plugin directory")?;
    fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    // A unique, same-directory staging file keeps failed validation/writes from
    // destroying a working pack. Never follow an existing staging symlink.
    let staging = parent.join(format!(".iogw-scene-{:016x}.tmp", rand::random::<u64>()));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(|err| err.to_string())?;
        file.write_all(bytes).map_err(|err| err.to_string())?;
        file.sync_all().map_err(|err| err.to_string())?;
        drop(file);
        fs::rename(&staging, target).map_err(|err| err.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    id: String,
    version: String,
    palette: HashMap<char, String>,
    sprites: HashMap<String, Sprite>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sprite {
    frame_ms: u64,
    frames: Vec<Vec<String>>,
}

pub(super) struct Pack {
    theme: Theme,
    version: String,
    palette: HashMap<char, Color>,
    sprites: HashMap<String, Sprite>,
}

impl Pack {
    fn load(path: &Path) -> Result<Self, String> {
        let pack = Self::parse(&read_bounded(path)?)?;
        if path.file_stem().and_then(|name| name.to_str()) != Some(pack.theme.id()) {
            return Err("installed scene ID does not match its filename".into());
        }
        Ok(pack)
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() as u64 > MAX_BYTES {
            return Err("plugin exceeds the 256 KiB limit".into());
        }
        let manifest: Manifest = serde_json::from_slice(bytes).map_err(|err| err.to_string())?;
        let theme = Theme::from_id(&manifest.id).ok_or("unsupported plugin ID")?;
        if manifest.schema_version != 1 {
            return Err("unsupported scene schema version (expected API 1)".into());
        }
        if manifest.version.is_empty()
            || manifest.version.len() > 32
            || !manifest
                .version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        {
            return Err("invalid plugin version".into());
        }
        if manifest.palette.len() > 64 || manifest.sprites.len() > 32 {
            return Err("plugin contains too many colors or sprites".into());
        }
        let mut palette = HashMap::new();
        for (key, value) in manifest.palette {
            if !key.is_ascii_graphic() || key == '.' {
                return Err("palette keys must be printable ASCII other than '.'".into());
            }
            if value.len() != 7
                || !value.starts_with('#')
                || !value[1..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("palette colors must use #RRGGBB".into());
            }
            let rgb = u32::from_str_radix(&value[1..], 16).map_err(|err| err.to_string())?;
            palette.insert(
                key,
                Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8),
            );
        }
        if !REQUIRED_COLORS
            .chars()
            .chain(theme.extra_colors().chars())
            .all(|key| palette.contains_key(&key))
            || !theme
                .required_sprites()
                .iter()
                .all(|key| manifest.sprites.contains_key(*key))
        {
            return Err("plugin is missing required palette roles or sprites".into());
        }
        for sprite in manifest.sprites.values() {
            if !(80..=5000).contains(&sprite.frame_ms) || !(1..=16).contains(&sprite.frames.len()) {
                return Err("sprite frame count/timing is out of bounds".into());
            }
            let height = sprite.frames[0].len();
            let width = sprite.frames[0].first().map_or(0, String::len);
            if !(1..=32).contains(&height) || !(1..=40).contains(&width) {
                return Err("sprite dimensions are out of bounds".into());
            }
            for frame in &sprite.frames {
                if frame.len() != height
                    || frame.iter().any(|row| {
                        row.len() != width
                            || !row
                                .chars()
                                .all(|key| key == '.' || palette.contains_key(&key))
                    })
                {
                    return Err(
                        "sprite frames must be rectangular, equal-sized, and use palette colors"
                            .into(),
                    );
                }
            }
        }
        Ok(Self {
            theme,
            version: manifest.version,
            palette,
            sprites: manifest.sprites,
        })
    }

    fn color(&self, role: char) -> Color {
        self.palette[&role]
    }
}

pub(super) struct Animation {
    pack: Option<Pack>,
    pub(super) active: bool,
    pub(super) paused: bool,
    pub(super) page: usize,
    started: Instant,
    paused_at: Option<Instant>,
    paused_duration: Duration,
    activity: HashMap<(String, String), (u64, Option<Instant>)>,
    scene: Vec<SceneAccount>,
    order: Vec<usize>,
    theme: Theme,
    selected: usize,
    viewport: (u16, u16),
    notice: Option<String>,
    all_limits: bool,
    bubbles: bool,
    monitor: super::system_metrics::Monitor,
    resets: HashMap<(String, String, String), (String, Option<i64>)>,
    refreshing: Option<(&'static str, Instant, bool)>,
    models_cat: SceneAccount,
    dashboard: Dashboard,
    focus_from: Vec<Rect>,
    focus_to: Vec<Rect>,
    focus_at: u64,
    focus_context: Option<(usize, Theme, u16, u16)>,
    login: login_animation::LoginAnimation,
}

impl Animation {
    pub(super) fn new() -> Self {
        let theme = read_bounded(&plugins_dir().join("selected-theme.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .get("theme")
                    .and_then(Value::as_str)
                    .and_then(Theme::from_id)
            })
            .unwrap_or(Theme::Cafe);
        Self {
            pack: None,
            active: false,
            paused: false,
            page: 0,
            started: Instant::now(),
            paused_at: None,
            paused_duration: Duration::ZERO,
            activity: HashMap::new(),
            scene: Vec::new(),
            order: Vec::new(),
            theme,
            selected: 0,
            viewport: (80, 24),
            notice: None,
            all_limits: false,
            bubbles: true,
            monitor: super::system_metrics::Monitor::default(),
            resets: HashMap::new(),
            refreshing: None,
            models_cat: SceneAccount::models_cat(),
            dashboard: Dashboard::default(),
            focus_from: Vec::new(),
            focus_to: Vec::new(),
            focus_at: 0,
            focus_context: None,
            login: login_animation::LoginAnimation::default(),
        }
    }

    pub(super) fn toggle(&mut self) -> Result<(), String> {
        if self.active {
            self.exit();
            return Ok(());
        }
        // Reload on entry, including packs installed while the TUI is running.
        let pack = Theme::ALL
            .into_iter()
            .filter(|theme| *theme != self.theme)
            .fold(Pack::load(&self.theme.path()), |current, theme| {
                current.or_else(|_| Pack::load(&theme.path()))
            })
            .map_err(|err| {
                format!(
                    "animated plugin unavailable: {err}; install with iogw plugins install <path>"
                )
            })?;
        self.theme = pack.theme;
        self.pack = Some(pack);
        self.notice = None;
        self.active = true;
        self.monitor.start();
        if std::env::var_os("IOGW_REDUCED_MOTION").is_some_and(|value| value == "1") && !self.paused
        {
            self.pause();
        }
        self.resize(self.viewport.0, self.viewport.1);
        Ok(())
    }

    pub(super) fn cycle_theme(&mut self) {
        // Missing optional downloads must not interrupt cycling installed ones.
        let next = self.theme.alternatives().next().unwrap();
        let available = self
            .theme
            .alternatives()
            .find_map(|theme| Pack::load(&theme.path()).ok());
        match available {
            Some(pack) => {
                let next = pack.theme;
                self.pack = Some(pack);
                self.theme = next;
                self.notice = install(
                    &plugins_dir().join("selected-theme.json"),
                    &serde_json::to_vec(&json!({"theme": next.id()}))
                        .expect("JSON value serializes"),
                )
                .err()
                .map(|err| format!("theme changed; could not save preference: {err}"));
                self.resize(self.viewport.0, self.viewport.1);
            }
            None => {
                self.notice = Some(format!(
                    "{} theme unavailable: iogw plugins install {}.json",
                    next.name(),
                    next.id()
                ))
            }
        }
    }

    pub(super) fn resize(&mut self, width: u16, height: u16) {
        self.viewport = (width, height);
        self.selected = self.selected.min(self.order.len());
        self.page = self.selected / self.layout().capacity;
        self.update_focus();
    }

    fn focus_cards(&self) -> Vec<Rect> {
        interpolate_cards(
            &self.focus_from,
            &self.focus_to,
            self.elapsed().saturating_sub(self.focus_at),
        )
    }

    fn update_focus(&mut self) {
        let layout = self.layout();
        let accounts = std::iter::once(&self.models_cat)
            .chain(self.order.iter().filter_map(|index| self.scene.get(*index)))
            .collect::<Vec<_>>();
        let start = self.page * layout.capacity;
        let visible =
            &accounts[start.min(accounts.len())..(start + layout.capacity).min(accounts.len())];
        let target = if self.bubbles {
            layout.focused_cards(
                visible,
                self.selected.saturating_sub(start),
                self.all_limits,
            )
        } else {
            layout.cards.clone()
        };
        let context = Some((self.page, self.theme, self.viewport.0, self.viewport.1));
        if target != self.focus_to || context != self.focus_context {
            self.focus_from = if self.paused {
                target.clone()
            } else if context == self.focus_context && self.focus_to.len() == target.len() {
                self.focus_cards()
            } else {
                layout.cards.clone()
            };
            self.focus_to = target;
            self.focus_at = self.elapsed();
            self.focus_context = context;
        }
    }

    fn layout(&self) -> SceneLayout {
        let accounts = std::iter::once(&self.models_cat)
            .chain(self.order.iter().filter_map(|index| self.scene.get(*index)))
            .collect::<Vec<_>>();
        SceneLayout::for_accounts(
            Rect::new(0, 0, self.viewport.0, self.viewport.1),
            self.theme,
            &accounts,
            self.all_limits,
        )
    }

    pub(super) fn turn_page(&mut self, delta: isize) {
        let capacity = self.layout().capacity;
        let pages = (self.order.len() + 1).div_ceil(capacity);
        self.page = (self.page as isize + delta).rem_euclid(pages as isize) as usize;
        self.selected = self.page * capacity;
        self.update_focus();
    }

    pub(super) fn set_view(
        &mut self,
        order: Vec<usize>,
        selected: usize,
        all_limits: bool,
        models_focus: bool,
        model_selected: usize,
    ) {
        self.all_limits = all_limits;
        self.order = order
            .into_iter()
            .filter(|index| *index < self.scene.len())
            .collect();
        self.selected = if models_focus || self.order.is_empty() {
            0
        } else {
            selected + 1
        };
        self.models_cat.model_selected = model_selected;
        self.resize(self.viewport.0, self.viewport.1);
    }

    pub(super) fn selection(&self) -> usize {
        self.selected.saturating_sub(1)
    }

    pub(super) fn models_focused(&self) -> bool {
        self.selected == 0
    }

    pub(super) fn move_cat(&mut self, delta: isize) {
        self.selected =
            (self.selected as isize + delta).rem_euclid((self.order.len() + 1) as isize) as usize;
        self.resize(self.viewport.0, self.viewport.1);
    }

    pub(super) fn selected_identity(&self) -> &str {
        if self.models_focused() {
            return "Custom Models";
        }
        self.order
            .get(self.selection())
            .and_then(|index| self.scene.get(*index))
            .map_or("Account unavailable", SceneAccount::identity)
    }

    pub(super) fn focused_limits(&self, width: u16) -> Vec<String> {
        self.order
            .get(self.selection())
            .filter(|_| !self.models_focused())
            .and_then(|index| self.scene.get(*index))
            .map(|account| {
                (0..account.limits.len().max(1))
                    .map(|index| account.limit_line(index, width, 0))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(super) fn last(&mut self) {
        self.selected = self.order.len();
        self.resize(self.viewport.0, self.viewport.1);
    }

    pub(super) fn first(&mut self) {
        self.selected = 0;
        self.page = 0;
        self.update_focus();
    }

    pub(super) fn exit(&mut self) {
        self.monitor.stop();
        self.active = false;
        self.pack = None;
        self.scene.clear();
        self.order.clear();
        self.activity.clear();
        self.resets.clear();
        self.models_cat = SceneAccount::models_cat();
        self.dashboard = Dashboard::default();
        self.focus_from.clear();
        self.focus_to.clear();
        self.focus_context = None;
    }

    pub(super) fn toggle_bubbles(&mut self) {
        self.bubbles = !self.bubbles;
        self.update_focus();
    }

    pub(super) fn poll_system(&mut self, visible: bool) {
        if visible {
            self.monitor.start();
            self.monitor.poll();
        } else {
            self.monitor.stop();
        }
    }

    pub(super) fn refresh_started(&mut self, scope: &'static str) {
        self.refreshing = Some((scope, Instant::now(), true));
    }

    pub(super) fn refresh_pending(&mut self, quota_pending: bool, reads_pending: bool) {
        if let Some((scope, started, _)) = self.refreshing {
            let pending = quota_pending || (scope != "ACCOUNT" && reads_pending);
            self.refreshing = if pending || started.elapsed() < Duration::from_millis(900) {
                Some((scope, started, pending))
            } else {
                None
            };
        }
    }

    pub(super) fn pause(&mut self) {
        if let Some(at) = self.paused_at.take() {
            self.paused_duration += at.elapsed();
            self.paused = false;
        } else {
            self.paused_at = Some(Instant::now());
            self.paused = true;
        }
    }

    fn elapsed(&self) -> u64 {
        self.paused_at
            .unwrap_or_else(Instant::now)
            .duration_since(self.started)
            .saturating_sub(self.paused_duration)
            .as_millis() as u64
    }

    pub(super) fn observe(&mut self, data: &GatewayData) {
        self.login.observe(&data.session, self.elapsed());
        if self.pack.is_none() {
            return;
        }
        self.dashboard = Dashboard::observe(data);
        self.models_cat.models = Some(
            data.models
                .iter()
                .map(|model| {
                    (
                        clean(if model.alias.is_empty() {
                            &model.id
                        } else {
                            &model.alias
                        }),
                        model.enabled,
                    )
                })
                .collect(),
        );
        let mut previous = std::mem::take(&mut self.activity);
        let mut previous_resets = std::mem::take(&mut self.resets);
        self.scene.clear();
        for account in &data.accounts {
            let key = (account.provider.clone(), account.key.clone());
            let mut record = previous.remove(&key).unwrap_or((account.requests, None));
            if account.requests > record.0 {
                record.1 = Some(Instant::now() + Duration::from_secs(10));
            } else if account.requests < record.0 {
                record.1 = None;
            }
            record.0 = account.requests;
            let status = if !account.enabled {
                'd'
            } else if account.cooldown {
                'c'
            } else if !account.attention_reasons.is_empty() {
                'a'
            } else {
                'h'
            };
            let matching = data
                .quotas
                .iter()
                .filter(|quota| {
                    quota.provider == account.provider && quota_matches_account(quota, account)
                })
                .collect::<Vec<_>>();
            let quota = matching
                .iter()
                .copied()
                .filter(|quota| quota.used_percent_value.is_some_and(f64::is_finite))
                .max_by(|a, b| {
                    a.used_percent_value
                        .partial_cmp(&b.used_percent_value)
                        .unwrap()
                });
            let used = quota
                .and_then(|quota| quota.used_percent_value)
                .map(|used| used.clamp(0.0, 100.0));
            // Prefer the provider's overview buckets to duplicate model-level
            // detail. Never hide a detail bucket with higher usage, though:
            // MiniMax's headline is the first model, not necessarily the worst.
            let mut summaries = matching
                .iter()
                .copied()
                .filter(|quota| quota.overview)
                .collect::<Vec<_>>();
            if summaries.is_empty() {
                summaries = matching;
            } else {
                let headline = summaries
                    .iter()
                    .filter_map(|summary| {
                        summary.used_percent_value.filter(|used| used.is_finite())
                    })
                    .max_by(f64::total_cmp);
                summaries.extend(matching.iter().copied().filter(|detail| {
                    !detail.overview
                        && detail.used_percent_value.is_some_and(|used| {
                            used.is_finite() && headline.is_none_or(|headline| used > headline)
                        })
                }));
            }
            let mut limits = summaries
                .into_iter()
                .map(|quota| {
                    let reset_key = (
                        account.provider.clone(),
                        account.key.clone(),
                        quota.limit_label.clone(),
                    );
                    let clock = previous_resets
                        .remove(&reset_key)
                        .filter(|(raw, _)| raw == &quota.reset)
                        .unwrap_or_else(|| {
                            (
                                quota.reset.clone(),
                                reset_deadline(&quota.reset, chrono::Utc::now().timestamp()),
                            )
                        });
                    let deadline = clock.1;
                    self.resets.insert(reset_key, clock);
                    (
                        compact_quota_label(&account.provider, &quota.limit_label),
                        quota
                            .used_percent_value
                            .filter(|used| used.is_finite())
                            .map(|used| used.clamp(0.0, 100.0)),
                        deadline,
                    )
                })
                .collect::<Vec<_>>();
            let mut seen = HashSet::new();
            for (label, _, _) in &mut limits {
                let base = label.clone();
                let mut duplicate = 2;
                while !seen.insert(label.clone()) {
                    *label = format!("{base} ({duplicate})");
                    duplicate += 1;
                }
            }
            limits.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap());
            let (limits, resets) = limits
                .into_iter()
                .map(|(label, used, reset)| ((label, used), reset))
                .unzip();
            let values = account_snapshot_values(&data.snapshot, account);
            let credit_value = values
                .iter()
                .copied()
                .find(|value| value.get("credits").is_some());
            let credits = (["codex", "claude"].contains(&account.provider.as_str())
                || credit_value.is_some())
            .then(|| {
                let value = credit_value.unwrap_or(&Value::Null);
                super::provider_quota_summary::credit_fields(value)
                    .into_iter()
                    .find(|field| field.label == "Available credits")
                    .map(|field| clean(&field.value))
                    .unwrap_or_else(|| "Not reported".into())
            });
            let email = account_snapshot_values(&data.snapshot, account)
                .into_iter()
                .filter(|value| super::snapshot_value_matches_account(value, account))
                .find_map(|value| {
                    ["email", "account_email"]
                        .into_iter()
                        .find_map(|key| value.get(key).and_then(Value::as_str).and_then(email_only))
                })
                .or_else(|| {
                    [&account.label, &account.account_id, &account.key]
                        .into_iter()
                        .find_map(|value| email_only(value))
                })
                .unwrap_or_default();
            self.scene.push(SceneAccount {
                status,
                used,
                email,
                name: clean(&account.label).trim().to_owned(),
                provider: values
                    .iter()
                    .find_map(|value| match value["service"].as_str() {
                        Some("qwen_token_plan") => Some("Qwen Token Plan".to_string()),
                        Some("qwen_payg") => Some("Qwen Pay-as-you-go".to_string()),
                        _ => None,
                    })
                    .unwrap_or_else(|| clean(provider_label(&account.provider))),
                icon: provider_icon(super::account_display_provider(account)),
                quota: quota
                    .map(|quota| compact_quota_label(&account.provider, &quota.limit_label))
                    .unwrap_or_default(),
                requests: account.requests,
                limits,
                resets,
                credits,
                recent_until: record.1,
                models: None,
                model_selected: 0,
            });
            self.activity.insert(key, record);
        }
        self.order = (0..self.scene.len()).collect();
        self.resize(self.viewport.0, self.viewport.1);
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>, connection: &str) {
        let Some(pack) = &self.pack else {
            return;
        };
        let accounts = std::iter::once(&self.models_cat)
            .chain(self.order.iter().filter_map(|index| self.scene.get(*index)))
            .collect::<Vec<_>>();
        render_scene(
            frame,
            pack,
            &accounts,
            connection,
            self.elapsed(),
            self.page,
            self.selected,
            self.paused,
            self.notice.as_deref(),
            self.all_limits,
            self.bubbles,
            &self.monitor.snapshot,
            self.refreshing.map(|(scope, _, _)| scope),
            &self.dashboard,
            Some(&self.focus_cards()),
            self.login.visual(self.elapsed()),
        );
    }

    pub(super) fn login_started(&mut self) {
        self.login.started(self.elapsed());
    }

    pub(super) fn login_accepted(&mut self) {
        self.login.accepted(self.elapsed());
    }

    pub(super) fn login_failed(&mut self) {
        self.login.failed(self.elapsed());
    }
}

struct SceneAccount {
    status: char,
    recent_until: Option<Instant>,
    used: Option<f64>,
    email: String,
    name: String,
    provider: String,
    icon: &'static str,
    quota: String,
    requests: u64,
    limits: Vec<(String, Option<f64>)>,
    resets: Vec<Option<i64>>,
    credits: Option<String>,
    models: Option<Vec<(String, bool)>>,
    model_selected: usize,
}

#[derive(Default)]
struct Dashboard {
    requests: Option<u64>,
    tokens: Option<u64>,
    errors: Option<u64>,
    active: usize,
    accounts: usize,
    context: Vec<Option<f64>>,
}

impl Dashboard {
    fn observe(data: &GatewayData) -> Self {
        let totals = &data.summary["totals"];
        Self {
            requests: totals["requests"].as_u64(),
            tokens: totals["total_tokens"].as_u64(),
            errors: totals["errors"].as_u64(),
            active: super::active_account_count(&data.accounts),
            accounts: data.accounts.len(),
            // This is token throughput per bucket, NOT model context-window fullness.
            context: data
                .usage_buckets
                .iter()
                .map(|b| Some(b.total_tokens as f64))
                .collect(),
        }
    }
}

fn provider_color(provider: &str) -> Color {
    match provider.to_ascii_lowercase().as_str() {
        "codex" | "openai" => Color::Rgb(103, 205, 166),
        "claude" | "anthropic" => Color::Rgb(222, 150, 112),
        "gemini" | "gemini-cli" => Color::Rgb(122, 164, 242),
        "antigravity" => Color::Rgb(120, 208, 205),
        "minimax" => Color::Rgb(235, 135, 170),
        "grok" | "xai" => Color::Rgb(190, 194, 209),
        "qwen" => Color::Rgb(177, 153, 226),
        "deepseek" => Color::Rgb(109, 148, 219),
        "copilot" => Color::Rgb(194, 173, 237),
        "glm" => Color::Rgb(121, 193, 215),
        "custom models" => Color::Rgb(242, 201, 110),
        _ => Color::Rgb(181, 171, 145),
    }
}

fn email_only(value: &str) -> Option<String> {
    let clean = clean(value);
    clean
        .split(|c: char| c.is_whitespace() || "<>(),;\"".contains(c))
        .find(|word| {
            word.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty() && !domain.is_empty() && !domain.contains('@')
            })
        })
        .map(str::to_string)
}

fn provider_icon(provider: &str) -> &'static str {
    // Portable Unicode, not private-use/Nerd Font glyphs. Names stay visible too.
    match provider {
        "codex" | "openai" => "◎",
        "claude" | "anthropic" => "✳",
        "gemini" | "antigravity" => "✦",
        "qwen" => "◇",
        "iflow" => "≋",
        "copilot" | "github-copilot" => "◉",
        "deepseek" => "≈",
        "grok" => "×",
        "minimax" => "▧",
        "glm" => "◈",
        _ => "◆",
    }
}

fn clean(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()
        && !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'))
        .take(512).collect()
}

pub(super) fn clipped(value: &str, width: u16) -> String {
    let value = clean(value);
    if Line::from(value.as_str()).width() <= width as usize {
        return value;
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    for c in value.chars() {
        if Line::from(result.as_str()).width() + Line::from(c.to_string()).width() >= width as usize
        {
            break;
        }
        result.push(c);
    }
    result.push('…');
    result
}

fn compact_quota_label(provider: &str, value: &str) -> String {
    let value = clean(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Only shorten labels whose meaning is known. Do not strip model versions,
    // scopes or duplicate disambiguators, or edit the underlying detail data.
    let (label, duplicate) = value
        .rsplit_once(" (")
        .filter(|(_, suffix)| {
            suffix
                .strip_suffix(')')
                .is_some_and(|count| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()))
        })
        .map_or((value.as_str(), String::new()), |(label, suffix)| {
            (label, format!(" ({suffix}"))
        });
    let mut label = label.to_string();
    if provider == "codex" {
        for prefix in ["Code Gen", "Code Review"] {
            if label == prefix || label.starts_with(&format!("{prefix} ")) {
                label = label[5..].to_string();
                break;
            }
        }
    }
    if provider == "minimax" || matches!(label.as_str(), "5h window" | "Weekly window") {
        if let Some(short) = label.strip_suffix(" window") {
            label = short.to_string();
        }
    }
    if provider == "gemini"
        && matches!(
            label.as_str(),
            "Gemini Flash" | "Gemini Flash Lite" | "Gemini Pro"
        )
    {
        label = label[7..].to_string();
    }
    label = match label.as_str() {
        "Requests" => "Req".into(),
        "Input Tokens" => "In tokens".into(),
        "Output Tokens" => "Out tokens".into(),
        "Text requests" => "Text req".into(),
        "Image requests" => "Image req".into(),
        "Video requests" => "Video req".into(),
        _ => label,
    };
    label + &duplicate
}

fn usage_percent(used: f64) -> String {
    let value = format!("{used:.1}");
    format!("{}%", value.strip_suffix(".0").unwrap_or(&value))
}

fn reset_deadline(raw: &str, now: i64) -> Option<i64> {
    let raw = raw.trim();
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(date.timestamp());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
    }
    if raw.len() == 10 || raw.len() == 13 {
        if let Ok(epoch) = raw.parse::<i64>() {
            return Some(if raw.len() == 13 { epoch / 1000 } else { epoch });
        }
    }
    let lower = raw.to_ascii_lowercase();
    let mut rest = lower.trim();
    for prefix in ["resets in ", "reset in ", "in "] {
        if let Some(value) = rest.strip_prefix(prefix) {
            rest = value;
            break;
        }
    }
    if matches!(rest, "now" | "reset now" | "resets now") {
        return Some(now);
    }
    let mut seconds = 0i64;
    let mut count = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let value = rest[..digits].parse::<i64>().ok()?;
        rest = rest[digits..].trim_start();
        let unit = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        let multiplier = match &rest[..unit] {
            "d" | "day" | "days" => 86400,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            _ => return None,
        };
        seconds = seconds.checked_add(value.checked_mul(multiplier)?)?;
        count += 1;
        rest = rest[unit..].trim_start();
    }
    (count > 0).then(|| now.checked_add(seconds)).flatten()
}

fn reset_countdown(deadline: Option<i64>, now: i64) -> String {
    let Some(deadline) = deadline else {
        return "—".into();
    };
    let seconds = deadline.saturating_sub(now).max(0);
    if seconds == 0 {
        return "0m".into();
    }
    if seconds < 60 {
        return "<1m".into();
    }
    let mut remaining = seconds;
    let mut result = String::new();
    let mut count = 0;
    for (size, unit) in [(86400, 'd'), (3600, 'h'), (60, 'm')] {
        let value = remaining / size;
        remaining %= size;
        if value > 0 {
            result.push_str(&format!("{value}{unit}"));
            count += 1;
        }
        if count == 2 {
            break;
        }
    }
    result
}

impl SceneAccount {
    fn models_cat() -> Self {
        Self {
            status: 'h',
            recent_until: None,
            used: None,
            email: String::new(),
            name: String::new(),
            provider: "Custom Models".into(),
            icon: "◇",
            quota: String::new(),
            requests: 0,
            limits: Vec::new(),
            resets: Vec::new(),
            credits: None,
            models: Some(Vec::new()),
            model_selected: 0,
        }
    }
    fn heading(&self) -> String {
        format!("{} {} · {}", self.icon, self.provider, self.state())
    }

    fn identity(&self) -> &str {
        if !self.email.is_empty() {
            &self.email
        } else if !self.name.is_empty() {
            &self.name
        } else {
            "Account unavailable"
        }
    }

    fn quota_rows(&self, all_limits: bool) -> usize {
        if let Some(models) = &self.models {
            return models.len().clamp(1, 4);
        }
        self.limits.len().max(1).min(if all_limits { 4 } else { 2 })
            + usize::from(all_limits && self.credits.is_some())
    }

    fn credit_line(&self, width: u16) -> String {
        clipped(
            &format!(
                "Credits {}",
                self.credits.as_deref().unwrap_or("Not reported")
            ),
            width,
        )
    }

    fn limit_line(&self, index: usize, width: u16, hidden: usize) -> String {
        let (name, used) = self
            .limits
            .get(index)
            .map(|(name, used)| (name.as_str(), *used))
            .unwrap_or((self.quota.as_str(), self.used));
        let value = used.map(usage_percent).unwrap_or_else(|| {
            if name.is_empty() {
                "unavailable"
            } else {
                "—"
            }
            .into()
        });
        let countdown = reset_countdown(
            self.resets.get(index).copied().flatten(),
            chrono::Utc::now().timestamp(),
        );
        let value = if width < 18 {
            format!("{value} {countdown}")
        } else {
            format!("{value} · {countdown}")
        };
        let suffix = if hidden > 0
            && (width >= 18 || value.len() + 2 + hidden.to_string().len() <= width as usize)
        {
            format!("{value} +{hidden}")
        } else {
            value
        };
        // Reserve columns for the percentage and +N before clipping a label.
        let available = width.saturating_sub(Line::from(suffix.as_str()).width() as u16 + 1);
        let name = if available < 4 {
            String::new()
        } else {
            clipped(if name.is_empty() { "Usage" } else { name }, available)
        };
        if name.is_empty() {
            clipped(&suffix, width)
        } else {
            format!("{name} {suffix}")
        }
    }

    fn bubble_width(&self, all_limits: bool) -> u16 {
        if let Some(models) = &self.models {
            return (models
                .iter()
                .map(|(name, _)| Line::from(name.as_str()).width() + 8)
                .max()
                .unwrap_or(24)
                .clamp(26, 46)) as u16;
        }
        let show_credits = all_limits && self.credits.is_some();
        let rows = self.quota_rows(all_limits) - usize::from(show_credits);
        let hidden = self.limits.len().saturating_sub(rows);
        let usage_width = (0..rows)
            .map(|index| {
                Line::from(self.limit_line(index, 512, if index + 1 == rows { hidden } else { 0 }))
                    .width()
            })
            .max()
            .unwrap_or(0);
        (usage_width
            .max(if show_credits {
                Line::from(self.credit_line(512)).width()
            } else {
                0
            })
            .max(Line::from(self.heading()).width())
            .max(Line::from(self.identity()).width())
            .max(17)
            + 2)
        .min(46) as u16
    }
    fn state(&self) -> &'static str {
        match self.status {
            'd' => "disabled",
            'c' => "cooldown",
            'a' => "attention",
            _ => "ready",
        }
    }
    fn usage(&self) -> String {
        self.used
            .map(usage_percent)
            .unwrap_or_else(|| "Usage unavailable".into())
    }
}

struct SceneLayout {
    area: Rect,
    body: Rect,
    stage: Rect,
    cards: Vec<Rect>,
    seats: Vec<Rect>,
    capacity: usize,
    footer: Rect,
}

fn cafe_counter(height: i32) -> i32 {
    height * 80 / 100
}

/// Three detail rows plus a separate, theme-colored shortcut strip.
pub(super) fn footer_area(area: Rect) -> Rect {
    let height = 4.min(area.height);
    Rect::new(area.x, area.bottom() - height, area.width, height)
}

/// Uneven but deterministic lanes: actors and bubbles share exactly one anchor.
/// Keep enough width for long identities, rather than random offsets that collide.
fn lane_edge(width: u16, column: u16, columns: u16) -> u16 {
    let weights = [82u32, 91, 138, 124, 76, 105, 114];
    let total: u32 = (0..columns)
        .map(|i| weights[i as usize % weights.len()])
        .sum();
    let before: u32 = (0..column)
        .map(|i| weights[i as usize % weights.len()])
        .sum();
    let minimum = (width / columns.max(1)).min(14);
    let extra = width.saturating_sub(minimum * columns);
    minimum * column + (u32::from(extra) * before / total.max(1)) as u16
}

fn lamp_height(counter: i32) -> i32 {
    (counter / 7).clamp(4, 10)
}

/// A cat settles onto its side, shows its belly, then settles back. It never
/// rotates as a rigid square or makes a full pinwheel turn.
fn roll_pose(phase: u64) -> usize {
    const POSES: [usize; 12] = [0, 1, 2, 3, 3, 2, 1, 0, 0, 0, 0, 0];
    POSES[(phase % 3600 / 300) as usize]
}

fn cat_motion(index: usize, status: char, ms: u64) -> (i32, i32, u64) {
    let phase = ms + index as u64 * 347;
    let (_, cycle, period) = cat_clock(index, ms);
    let activity = cat_activity(index, status, ms);
    let age = cat_action_age(index, ms);
    let stroll = if activity == CatActivity::Roll {
        [0, 0, 1, 2][roll_pose(age)]
    } else if status == 'h' && activity == CatActivity::Watch && cycle >= 10000 {
        // Ease into a short stroll and back, rather than reversing instantly.
        let t = (cycle - 10000) as f64 / (period - 10000) as f64;
        (3.0 * (1.0 - (t * std::f64::consts::TAU).cos())).round() as i32
    } else {
        0
    };
    (
        stroll,
        if activity == CatActivity::Jump && (500..1500).contains(&age) {
            let t = (age - 500) as f64 / 1000.0;
            (12.0 * t * (1.0 - t)).round() as i32
        } else {
            0
        },
        phase,
    )
}

fn cat_action_age(index: usize, ms: u64) -> u64 {
    cat_clock(index, ms).1.saturating_sub(1200)
}

fn cat_clock(index: usize, ms: u64) -> (u64, u64, u64) {
    let period = 18000 + (index as u64 * 977 % 5000);
    let phase = ms + index as u64 * 347;
    (phase / period, phase % period, period)
}

fn cat_motion_for_height(index: usize, status: char, ms: u64, height: u16) -> (i32, i32, u64) {
    let (stroll, bob, phase) = cat_motion(index, status, ms);
    (stroll, bob.min(if height < 40 { 1 } else { 3 }), phase)
}

fn cat_head_height(index: usize, status: char, ms: u64) -> i32 {
    11 - cat_pose(index, status, ms).head_drop
}

fn ease(value: f64) -> f64 {
    let t = value.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A small articulated rig: feet stay grounded while the head, shoulders,
/// ears and tail move independently. It shares the bubble's scene clock.
struct CatPose {
    settle: f64,
    head_drop: i32,
    look: i32,
    breath: f64,
    blink: bool,
    ear: i32,
    tail: f64,
    paw: i32,
    stride: f64,
}

fn cat_pose(index: usize, status: char, ms: u64) -> CatPose {
    let activity = cat_activity(index, status, ms);
    let (_, tick, period) = cat_clock(index, ms);
    let phase = ms + index as u64 * 347;
    let age = cat_action_age(index, ms) as f64;
    let envelope =
        ease((tick as f64 - 1200.0) / 900.0) * ease((period as f64 - 1200.0 - tick as f64) / 900.0);
    let settle = match activity {
        CatActivity::Sleep if status == 'd' => 1.0,
        CatActivity::Burnout => 1.0,
        CatActivity::Sleep => envelope,
        CatActivity::Jump if age < 500.0 => ease(age / 240.0) * (1.0 - ease((age - 350.0) / 150.0)),
        CatActivity::Jump if age >= 1500.0 => {
            ease((age - 1500.0) / 160.0) * (1.0 - ease((age - 1660.0) / 740.0))
        }
        CatActivity::Stretch => ease(age / 700.0) * (1.0 - ease((age - 1800.0) / 1200.0)) * 0.7,
        CatActivity::Sleepy => envelope * (0.15 + 0.2 * (age / 1500.0).sin().max(0.0)),
        _ => 0.0,
    };
    let sleeping = matches!(activity, CatActivity::Sleep | CatActivity::Burnout);
    let head_drop = if activity == CatActivity::Roll && roll_pose(age as u64) != 0 {
        3
    } else {
        (3.0 * settle).round() as i32
    };
    let blink_clock = phase % (6300 + rain_hash(index as u64 + 43) % 3700);
    let look_clock = (phase % 14000) as f64;
    let look = if sleeping || settle > 0.5 {
        0
    } else {
        (ease((look_clock - 7000.0) / 900.0) * (1.0 - ease((look_clock - 9600.0) / 1000.0))).round()
            as i32
    };
    let tail_speed = match activity {
        CatActivity::Angry => 190.0,
        CatActivity::Play => 430.0,
        CatActivity::Wag => 720.0,
        _ => 1800.0,
    };
    CatPose {
        settle,
        head_drop,
        look,
        breath: (phase as f64 / if sleeping { 1000.0 } else { 780.0 }).sin() * 0.5 + 0.5,
        blink: (sleeping && settle > 0.5)
            || (1750..1920).contains(&blink_clock)
            || (activity == CatActivity::Sleepy && phase % 3900 < 800),
        ear: i32::from(!sleeping && (4800..5040).contains(&(phase % 11300))),
        tail: (phase as f64 / tail_speed).sin() * if sleeping { 0.25 } else { 1.0 },
        paw: if matches!(activity, CatActivity::Groom | CatActivity::Play) {
            ((phase as f64 / 450.0).sin() * 0.5 + 0.5)
                .mul_add(2.0, 0.0)
                .round() as i32
        } else {
            0
        },
        stride: if status == 'h' && activity == CatActivity::Watch && tick >= 10000 {
            let t = (tick - 10000) as f64 / (period - 10000) as f64;
            if (t * std::f64::consts::TAU).sin().abs() > 0.15 {
                (phase as f64 / 460.0).sin()
            } else {
                0.0
            }
        } else {
            0.0
        },
    }
}

fn actor_motion(
    layout: &SceneLayout,
    index: usize,
    account: Option<&SceneAccount>,
    ms: u64,
    all_limits: bool,
) -> (i32, i32, u64) {
    let lane = layout.cards[index];
    let bubble_width = account
        .map_or(13, |account| account.bubble_width(all_limits))
        .min(lane.width);
    // Shared gutters keep neighboring moving bubbles disjoint, even on a full
    // dense page. Cats and bubbles use this exact displacement, not two clocks.
    let left = i32::from((lane.width - bubble_width) / 2 + u16::from(lane.x > layout.body.x));
    let right = i32::from(
        (lane.width - bubble_width).div_ceil(2) + u16::from(lane.right() < layout.body.right()),
    );
    let (dx, bob, phase) = cat_motion_for_height(
        index,
        account.map_or('h', |a| a.status),
        ms,
        layout.stage.height,
    );
    let sprite_gutter = lane.width.saturating_add(2).saturating_sub(13);
    (
        dx.clamp(
            -left.min(i32::from(sprite_gutter / 2)),
            right.min(i32::from(sprite_gutter.div_ceil(2))),
        ),
        bob,
        phase,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatActivity {
    Watch,
    Sleep,
    Roll,
    Jump,
    Wag,
    Play,
    Sleepy,
    Groom,
    Stretch,
    Burnout,
    Angry,
}

fn cat_activity(index: usize, status: char, ms: u64) -> CatActivity {
    if status == 'd' {
        return CatActivity::Sleep;
    }
    if status == 'a' {
        return CatActivity::Angry;
    }
    if status == 'c' {
        return CatActivity::Burnout;
    }
    let (epoch, tick, period) = cat_clock(index, ms);
    // Long quiet rests between individual actions, with preparation and
    // recovery. Different cats retain staggered, deterministic clocks.
    if tick < 1200 || tick >= period - 1200 {
        return CatActivity::Watch;
    }
    match rain_hash(epoch + index as u64 * 137) % 9 {
        0 => CatActivity::Sleep,
        1 if tick < 4800 => CatActivity::Roll,
        2 if tick < 3600 => CatActivity::Jump,
        3 => CatActivity::Wag,
        4 if tick < 8200 => CatActivity::Play,
        5 => CatActivity::Sleepy,
        6 if tick < 8000 => CatActivity::Groom,
        7 if tick < 4200 => CatActivity::Stretch,
        _ => CatActivity::Watch,
    }
}

fn speech_bubble(
    layout: &SceneLayout,
    index: usize,
    account: &SceneAccount,
    theme: Theme,
    ms: u64,
    all_limits: bool,
) -> (Rect, u16) {
    let mut card = layout.cards[index];
    let center = i32::from(card.x) + i32::from(card.width) / 2;
    let (dx, bob, _) = actor_motion(layout, index, Some(account), ms, all_limits);
    let width = account.bubble_width(all_limits).min(card.width);
    card.x += (card.width - width) / 2;
    card.width = width;
    card.x = (i32::from(card.x) + dx).max(i32::from(layout.body.x)) as u16;
    let height = (account.quota_rows(all_limits) as u16 + 4).min(card.height);
    card.y += card.height - height;
    card.height = height;
    let tail = if theme.on_platform() {
        let head = (cafe_counter(i32::from(layout.stage.height) * 2)
            - cat_head_height(index, account.status, ms)
            - bob)
            .max(0) as u16
            / 2;
        card.height = card
            .height
            .min((layout.stage.y + head).saturating_sub(layout.body.y + 1));
        card.y = (layout.stage.y + head.saturating_sub(card.height + 1)).max(layout.body.y);
        card.height = card.height.min(layout.body.bottom().saturating_sub(card.y));
        (center + dx).clamp(i32::from(card.x), i32::from(card.right().saturating_sub(1))) as u16
    } else {
        card.y = (card.y + ((11 - cat_head_height(index, account.status, ms)) / 2) as u16)
            .saturating_sub((bob / 2) as u16)
            .max(layout.body.y)
            .min(layout.body.bottom().saturating_sub(card.height));
        (center + dx).clamp(i32::from(card.x), i32::from(card.right().saturating_sub(1))) as u16
    };
    (card, tail)
}

impl SceneLayout {
    /// Borrow only the horizontal space the focused bubble needs. Neighbors
    /// stay ordered, on this page, and within twelve columns of their seats.
    fn focused_cards(
        &self,
        accounts: &[&SceneAccount],
        selected: usize,
        all_limits: bool,
    ) -> Vec<Rect> {
        let mut result = self.cards.clone();
        let Some(account) = accounts.get(selected) else {
            return result;
        };
        let Some(focus) = self.cards.get(selected) else {
            return result;
        };
        let row = self
            .cards
            .iter()
            .take(accounts.len())
            .enumerate()
            .filter(|(_, card)| card.y == focus.y)
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let Some(chosen) = row.iter().position(|i| *i == selected) else {
            return result;
        };
        let wanted = account
            .bubble_width(all_limits)
            .min(focus.width.saturating_add(24))
            .min(self.body.width.saturating_sub(2));
        for width in (focus.width.saturating_add(1)..=wanted).rev() {
            let widths = row
                .iter()
                .map(|i| {
                    if *i == selected {
                        width
                    } else {
                        self.cards[*i].width.min(11)
                    }
                })
                .collect::<Vec<_>>();
            let original = row
                .iter()
                .map(|i| i32::from(self.cards[*i].x + self.cards[*i].width / 2))
                .collect::<Vec<_>>();
            let mut centers = original.clone();
            let left_edge = i32::from(self.body.x + 1);
            let right_edge = i32::from(self.body.right().saturating_sub(1));
            let spacing = (0..row.len().saturating_sub(1))
                .map(|i| (i32::from(widths[i].div_ceil(2) + widths[i + 1] / 2) + 2).max(13))
                .collect::<Vec<_>>();
            let mut lower = original
                .iter()
                .enumerate()
                .map(|(i, center)| {
                    (center - 12)
                        .max(left_edge + i32::from(widths[i] / 2))
                        .max(i32::from(self.body.x) + 6)
                })
                .collect::<Vec<_>>();
            let mut upper = original
                .iter()
                .enumerate()
                .map(|(i, center)| {
                    (center + 12)
                        .min(right_edge - i32::from(widths[i].div_ceil(2)))
                        .min(i32::from(self.body.right()) - 7)
                })
                .collect::<Vec<_>>();
            for i in 1..row.len() {
                lower[i] = lower[i].max(lower[i - 1] + spacing[i - 1]);
            }
            for i in (0..row.len().saturating_sub(1)).rev() {
                upper[i] = upper[i].min(upper[i + 1] - spacing[i]);
            }
            if lower.iter().zip(&upper).any(|(low, high)| low > high) {
                continue;
            }
            // Near an edge the focused cat can also take a small step inward,
            // instead of needlessly pushing its outer neighbor off screen.
            centers[chosen] = centers[chosen].clamp(lower[chosen], upper[chosen]);
            for i in (0..chosen).rev() {
                centers[i] = centers[i]
                    .clamp(lower[i], upper[i])
                    .min(centers[i + 1] - spacing[i]);
            }
            for i in chosen + 1..row.len() {
                centers[i] = centers[i]
                    .clamp(lower[i], upper[i])
                    .max(centers[i - 1] + spacing[i - 1]);
            }
            if centers.iter().enumerate().any(|(i, center)| {
                (*center - original[i]).abs() > 12
                    || *center - i32::from(widths[i] / 2) < left_edge
                    || *center + i32::from(widths[i].div_ceil(2)) > right_edge
                    || *center - 6 < i32::from(self.body.x)
                    || *center + 7 > i32::from(self.body.right())
            }) {
                continue;
            }
            for (i, index) in row.iter().enumerate() {
                result[*index].x = (centers[i] - i32::from(widths[i] / 2)) as u16;
                result[*index].width = widths[i];
            }
            // Preserve an unaffected neighbor's original width when it fits.
            for index in &row {
                if *index == selected {
                    continue;
                }
                let center = result[*index].x + result[*index].width / 2;
                let mut restored = self.cards[*index];
                restored.x = center.saturating_sub(restored.width / 2);
                if row.iter().filter(|i| **i != *index).all(|i| {
                    let other = result[*i];
                    restored.right().saturating_add(1) <= other.x
                        || other.right().saturating_add(1) <= restored.x
                }) {
                    result[*index] = restored;
                }
            }
            break;
        }
        result
    }

    #[cfg(test)]
    fn new(area: Rect, theme: Theme) -> Self {
        Self::for_content(area, theme, 2, 30, usize::MAX)
    }

    fn for_accounts(
        area: Rect,
        theme: Theme,
        accounts: &[&SceneAccount],
        all_limits: bool,
    ) -> Self {
        let limits = accounts
            .iter()
            // A four-model list must not halve the office's desk rows. Its
            // contents scroll in the same space as an account's short summary.
            .filter(|account| theme != Theme::Office || account.models.is_none())
            .map(|account| account.quota_rows(all_limits))
            .max()
            .unwrap_or(2);
        let width = accounts
            .iter()
            .map(|account| account.bubble_width(all_limits) + 2)
            .max()
            .unwrap_or(30)
            .max(30);
        Self::for_content(area, theme, limits, width, accounts.len().max(1))
    }

    fn for_content(
        area: Rect,
        theme: Theme,
        limits: usize,
        preferred_width: u16,
        count: usize,
    ) -> Self {
        let width = area.width;
        let header = area.height.min(if width >= 120 && area.height >= 32 {
            7
        } else if theme.on_platform() && width >= 70 && area.height >= 20 {
            6
        } else if area.height >= 12 {
            4
        } else {
            2
        });
        let footer_height = footer_area(area)
            .height
            .min(area.height.saturating_sub(header));
        let body = Rect::new(
            area.x,
            area.y + header,
            width,
            area.height - header - footer_height,
        );
        let footer = Rect::new(area.x, body.bottom(), width, footer_height);
        let columns = (width / preferred_width)
            .max(1)
            .saturating_mul(2)
            .min((width / 14).max(1))
            .min(count.min(u16::MAX as usize) as u16);
        let available = if theme.on_platform() {
            ((cafe_counter(i32::from(area.height) * 2) - 11).max(0) as u16 / 2)
                .saturating_sub(header + 1)
        } else {
            body.height.saturating_sub(10)
        };
        let card_height = (limits.clamp(1, 5) as u16 + 4)
            .min(available.max(4))
            .min(body.height);
        let rows = if theme == Theme::Office {
            (body.height / (card_height + 9))
                .max(1)
                .min(count.div_ceil(columns as usize).min(u16::MAX as usize) as u16)
        } else {
            1
        };
        let capacity = usize::from(columns * rows);
        let stage = area;
        let cards = (0..columns * rows)
            .map(|index| {
                let column = index % columns;
                let row = index / columns;
                let left = body.x
                    + if theme.on_platform() {
                        lane_edge(width, column, columns)
                    } else {
                        column * width / columns
                    };
                let right = body.x
                    + if theme.on_platform() {
                        lane_edge(width, column + 1, columns)
                    } else {
                        (column + 1) * width / columns
                    };
                let top = body.y + row * body.height / rows;
                let bottom = body.y + (row + 1) * body.height / rows;
                let y = if theme.on_platform() {
                    let head = (cafe_counter(i32::from(area.height) * 2) - 11).max(0) as u16 / 2;
                    area.y + head.saturating_sub(card_height + 1)
                } else {
                    top + (bottom - top).saturating_sub(card_height + 10) / 2
                };
                Rect::new(
                    left + u16::from(right > left + 2),
                    y,
                    (right - left).saturating_sub(if right > left + 2 { 2 } else { 0 }),
                    card_height.min(bottom.saturating_sub(y)),
                )
            })
            .collect::<Vec<_>>();
        Self {
            area,
            body,
            stage,
            seats: cards.clone(),
            cards,
            capacity,
            footer,
        }
    }
}

fn interpolate_cards(from: &[Rect], to: &[Rect], elapsed: u64) -> Vec<Rect> {
    if from.len() != to.len() || elapsed >= 360 {
        return to.to_vec();
    }
    let t = elapsed as f64 / 360.0;
    let eased = t * t * (3.0 - 2.0 * t);
    from.iter()
        .zip(to)
        .map(|(from, to)| {
            // Interpolate edges, not width and position separately: adjacent
            // bubbles retain their gutters throughout the short eased transition.
            let edge = |a: u16, b: u16| {
                (f64::from(a) + (f64::from(b) - f64::from(a)) * eased).round() as u16
            };
            let left = edge(from.x, to.x);
            Rect::new(
                left,
                to.y,
                edge(from.right(), to.right()).saturating_sub(left),
                to.height,
            )
        })
        .collect()
}

fn label(frame: &mut Frame<'_>, area: Rect, text: &str, fg: Color, bg: Color, bold: bool) {
    if area.is_empty() {
        return;
    }
    let style = Style::default().fg(fg).bg(bg);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(clipped(text, area.width)).style(if bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }),
        area,
    );
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut Frame<'_>,
    pack: &Pack,
    accounts: &[&SceneAccount],
    connection: &str,
    ms: u64,
    page: usize,
    selected: usize,
    paused: bool,
    notice: Option<&str>,
    all_limits: bool,
    bubbles: bool,
    system: &super::system_metrics::Snapshot,
    refreshing: Option<&str>,
    dashboard: &Dashboard,
) {
    render_scene(
        frame, pack, accounts, connection, ms, page, selected, paused, notice, all_limits, bubbles,
        system, refreshing, dashboard, None, None,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_scene(
    frame: &mut Frame<'_>,
    pack: &Pack,
    accounts: &[&SceneAccount],
    connection: &str,
    ms: u64,
    page: usize,
    selected: usize,
    paused: bool,
    notice: Option<&str>,
    all_limits: bool,
    bubbles: bool,
    system: &super::system_metrics::Snapshot,
    refreshing: Option<&str>,
    dashboard: &Dashboard,
    cards: Option<&[Rect]>,
    login: Option<login_animation::Visual>,
) {
    let login =
        login.or_else(|| (connection == "AUTH").then(|| login_animation::Visual::required(ms)));
    let locked = login.is_some_and(login_animation::Visual::locked);
    // Never render stale/protected account bubbles behind a locked session.
    let accounts = if locked { &[][..] } else { accounts };
    let mut layout = SceneLayout::for_accounts(frame.area(), pack.theme, accounts, all_limits);
    if let Some(cards) = cards.filter(|cards| cards.len() == layout.cards.len()) {
        layout.cards = cards.to_vec();
    }
    let area = layout.area;
    frame.render_widget(
        Block::default().style(Style::default().bg(pack.color('b'))),
        frame.area(),
    );
    if area.is_empty() {
        return;
    }
    let pages = accounts.len().max(1).div_ceil(layout.capacity);
    let page = page.min(pages - 1);
    let start = page * layout.capacity;
    let visible =
        &accounts[start.min(accounts.len())..(start + layout.capacity).min(accounts.len())];
    match pack.theme {
        Theme::Cafe => render_cafe(frame, &layout, pack, visible, connection, ms, all_limits),
        Theme::Office => render_office(frame, &layout, pack, visible, ms, all_limits),
        Theme::Station => {
            station::render(frame, &layout, pack, visible, connection, ms, all_limits)
        }
    }
    label(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        &format!(
            " {} / {}  ·  {} accounts  ·  page {}/{}{}",
            pack.theme.name(),
            connection,
            accounts.iter().filter(|a| a.models.is_none()).count(),
            page + 1,
            pages,
            if paused { "  ·  PAUSED" } else { "" }
        ),
        pack.color('e'),
        pack.color('r'),
        true,
    );
    render_system(frame, area, pack, system, ms);
    render_tv(frame, area, pack, dashboard);
    let mut bubble_order = (0..visible.len()).collect::<Vec<_>>();
    bubble_order.sort_by_key(|index| selected == start + index);
    for index in bubble_order.into_iter().filter(|_| bubbles) {
        let account = visible[index];
        let (card, tail_x) = speech_bubble(&layout, index, account, pack.theme, ms, all_limits);
        let mut perimeter = Vec::new();
        if !card.is_empty() {
            for y in card.y..card.bottom() {
                for x in card.x..card.right() {
                    if y == card.y || y + 1 == card.bottom() || x == card.x || x + 1 == card.right()
                    {
                        let cell = &frame.buffer_mut()[(x, y)];
                        perimeter.push((
                            x,
                            y,
                            if y == card.y && cell.symbol() == "▀" {
                                cell.fg
                            } else {
                                cell.bg
                            },
                        ));
                    }
                }
            }
        }
        frame.render_widget(Clear, card);
        let chosen = selected == start + index;
        let bubble_bg = mix(
            pack.color('b'),
            provider_color(&account.provider),
            if chosen { 36 } else { 16 },
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ratatui::symbols::border::Set {
                top_left: "▗",
                top_right: "▖",
                bottom_left: "▝",
                bottom_right: "▘",
                horizontal_top: "▄",
                horizontal_bottom: "▀",
                vertical_left: "▐",
                vertical_right: "▌",
            })
            .border_style(Style::default().fg(bubble_bg))
            .style(Style::default().bg(bubble_bg));
        let inner = if card.height >= 5 && card.width >= 8 {
            let inner = block.inner(card);
            frame.render_widget(block, card);
            inner
        } else {
            card
        };
        // The outside half of every edge keeps the actual room behind it.
        for (x, y, color) in perimeter {
            if card.height >= 5 && card.width >= 8 {
                frame.buffer_mut()[(x, y)].set_bg(color);
            }
        }
        let mut lines = if let Some(models) = &account.models {
            let mut lines = vec![
                (
                    if inner.width < 18 {
                        format!("◇ Models {}", models.len())
                    } else {
                        format!("◇ Custom Models · {}", models.len())
                    },
                    provider_color(&account.provider),
                    true,
                ),
                (
                    format!(
                        "{} enabled",
                        models.iter().filter(|(_, enabled)| *enabled).count()
                    ),
                    pack.color('k'),
                    false,
                ),
            ];
            let slots = account
                .quota_rows(all_limits)
                .min(inner.height.saturating_sub(2) as usize);
            let offset = account
                .model_selected
                .saturating_sub(slots.saturating_sub(1));
            if models.is_empty() {
                lines.push(("No custom models".into(), pack.color('k'), false));
            }
            for (i, (name, enabled)) in models.iter().enumerate().skip(offset).take(slots) {
                let remaining = models.len().saturating_sub(i + 1);
                let suffix = if i + 1 == offset + slots && remaining > 0 {
                    format!(" +{remaining}")
                } else {
                    String::new()
                };
                let text = if inner.width < 16 {
                    format!(
                        "{}{}",
                        clipped(name, inner.width.saturating_sub(suffix.len() as u16)),
                        suffix
                    )
                } else {
                    format!(
                        "{} {} {}{}",
                        if chosen && i == account.model_selected {
                            "›"
                        } else {
                            " "
                        },
                        if *enabled { "●" } else { "○" },
                        clipped(name, inner.width.saturating_sub(4 + suffix.len() as u16)),
                        suffix
                    )
                };
                lines.push((
                    text,
                    pack.color(if *enabled { 'e' } else { 'k' }),
                    chosen && i == account.model_selected,
                ));
            }
            lines
        } else {
            vec![
                (
                    if Line::from(account.heading()).width() > inner.width as usize {
                        format!(
                            "{} {} {}",
                            account.icon,
                            account.provider,
                            match account.status {
                                'd' => "○",
                                'c' => "◷",
                                'a' => "!",
                                _ => "●",
                            }
                        )
                    } else {
                        account.heading()
                    },
                    pack.color(account.status),
                    true,
                ),
                (account.identity().into(), pack.color('e'), chosen),
            ]
        };
        let slots = account
            .quota_rows(all_limits)
            .min(inner.height.saturating_sub(2) as usize);
        // Keep the balance visible even when the scene only has room for a
        // short quota summary. Overflow still counts quota windows only.
        let show_credits = all_limits && account.credits.is_some() && slots > 0;
        let limit_slots = slots - usize::from(show_credits);
        for index in (0..limit_slots).filter(|_| account.models.is_none()) {
            let hidden = if index + 1 == limit_slots {
                account.limits.len().saturating_sub(limit_slots)
            } else {
                0
            };
            let used = account
                .limits
                .get(index)
                .map_or(account.used, |(_, used)| *used);
            lines.push((
                account.limit_line(index, inner.width, hidden),
                pack.color(if used.is_some_and(|used| used >= 90.0) {
                    'a'
                } else {
                    'e'
                }),
                true,
            ));
        }
        if show_credits && account.models.is_none() {
            lines.push((account.credit_line(inner.width), pack.color('e'), true));
        }
        for (row, (text, color, bold)) in lines.into_iter().enumerate().take(inner.height as usize)
        {
            label(
                frame,
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                &text,
                color,
                bubble_bg,
                bold,
            );
        }
        if card.bottom() < layout.body.bottom() {
            label(
                frame,
                Rect::new(tail_x, card.bottom(), 1, 1),
                "◥",
                bubble_bg,
                pack.color('w'),
                false,
            );
        }
    }
    if !locked && accounts.iter().all(|a| a.models.is_some()) && layout.body.height > 0 {
        let message_y = if pack.theme == Theme::Station {
            station::message_y(area)
                .max(layout.body.y)
                .min(layout.body.bottom() - 1)
        } else if pack.theme == Theme::Cafe {
            CafeRack::for_area(area)
                .map_or(layout.body.y, |rack| rack.area.bottom().saturating_add(1))
                .max(layout.body.y)
                .min(layout.body.bottom() - 1)
        } else {
            layout.body.y
        };
        label(
            frame,
            Rect::new(
                layout.body.x + 1.min(layout.body.width),
                message_y,
                layout.body.width.saturating_sub(2),
                1,
            ),
            "No account data · R refresh",
            pack.color('e'),
            pack.color('b'),
            false,
        );
    }
    let details = accounts
        .get(selected)
        .map(|account| {
            if account.models.is_some() {
                return "◇ Custom Models".into();
            }
            format!(
                "{} {} | {}",
                account.icon,
                account.provider,
                account.identity()
            )
        })
        .unwrap_or_default();
    let quota = accounts
        .get(selected)
        .map(|account| {
            if account.models.is_some() {
                return "j/k models · e/d enable/disable · x delete".into();
            }
            format!(
                "{}{}{} · {} requests · {}",
                account.usage(),
                if account.quota.is_empty() { "" } else { " · " },
                account.quota,
                account.requests,
                account.state()
            )
        })
        .unwrap_or_default();
    for (index, text) in [
        details.as_str(),
        notice.unwrap_or(&quota),
        "h/l ←/→ cats · j/k item · b bubbles · t theme · a quota · n add · e/d toggle · x delete · r account · R all · Ctrl+R hard · ? keys · Space pause · F4/Esc UI",
    ]
    .into_iter()
    .enumerate()
    {
        if index < layout.footer.height as usize {
            label(
                frame,
                Rect::new(
                    layout.footer.x,
                    if index == 2 { layout.footer.bottom() - 1 } else { layout.footer.y + index as u16 },
                    layout.footer.width,
                    1,
                ),
                text,
                pack.color(if index == 0 { 'e' } else { 'k' }),
                pack.color(if index == 2 { 'r' } else { 'b' }),
                index == 0,
            );
        }
    }
    if let Some(scope) = refreshing {
        render_refresh_bot(frame, area, pack, ms, scope);
    }
    if let Some(login) = login {
        // Shared overlay lives above scenery, below ordinary help/login modals.
        let top = area.y.saturating_add(1).min(layout.footer.y);
        login_animation::render(
            frame,
            Rect::new(area.x, top, area.width, layout.footer.y.saturating_sub(top)),
            pack,
            login,
        );
    }
}

/// Connected braille dots, fixed 0..100 for utilization. Missing samples break
/// the stroke; short histories are right-aligned and never padded with fake data.
fn line_chart(
    frame: &mut Frame<'_>,
    area: Rect,
    values: &[Option<f64>],
    maximum: f64,
    fg: Color,
    bg: Color,
) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
    let width = usize::from(area.width) * 2;
    let height = usize::from(area.height) * 4;
    let mut dots = vec![0u8; usize::from(area.width) * usize::from(area.height)];
    let visible = &values[values.len().saturating_sub(width)..];
    let offset = width - visible.len();
    let mut previous: Option<(usize, usize)> = None;
    for (i, value) in visible.iter().enumerate() {
        let Some(value) = value.filter(|v| v.is_finite()) else {
            previous = None;
            continue;
        };
        let x = offset + i;
        let y = ((1.0 - (value / maximum.max(1.0)).clamp(0.0, 1.0)) * (height - 1) as f64).round()
            as usize;
        let (from_x, from_y) = previous.unwrap_or((x, y));
        let steps = x.abs_diff(from_x).max(y.abs_diff(from_y)).max(1);
        for step in 0..=steps {
            let px = (from_x as isize
                + (x as isize - from_x as isize) * step as isize / steps as isize)
                as usize;
            let py = (from_y as isize
                + (y as isize - from_y as isize) * step as isize / steps as isize)
                as usize;
            let bit = [[0, 3], [1, 4], [2, 5], [6, 7]][py % 4][px % 2];
            dots[py / 4 * usize::from(area.width) + px / 2] |= 1 << bit;
        }
        previous = Some((x, y));
    }
    for y in 0..area.height {
        for x in 0..area.width {
            let mask = dots[usize::from(y) * usize::from(area.width) + usize::from(x)];
            let cell = &mut frame.buffer_mut()[(area.x + x, area.y + y)];
            if mask != 0 {
                cell.set_char(char::from_u32(0x2800 + u32::from(mask)).unwrap())
                    .set_fg(fg)
                    .set_bg(bg);
            }
        }
    }
}

/// Utilization history: one real sample per bar, blank gaps stay unknown.
fn bar_chart(
    frame: &mut Frame<'_>,
    area: Rect,
    values: &[Option<f64>],
    ceiling: f64,
    fg: Color,
    bg: Color,
) {
    const BLOCKS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let visible = &values[values.len().saturating_sub(area.width as usize)..];
    let padding = area.width as usize - visible.len();
    for x in 0..area.width {
        let value = (x as usize)
            .checked_sub(padding)
            .and_then(|i| visible.get(i))
            .copied()
            .flatten();
        let level = value
            .filter(|v| v.is_finite())
            .map(|v| (v / ceiling.max(1.0)).clamp(0.0, 1.0) * f64::from(area.height) * 8.0);
        for y in 0..area.height {
            let units = level.map_or(0, |v| {
                (v - f64::from(area.height - 1 - y) * 8.0)
                    .ceil()
                    .clamp(0.0, 8.0) as usize
            });
            frame.buffer_mut()[(area.x + x, area.y + y)]
                .set_symbol(BLOCKS[units])
                .set_fg(fg)
                .set_bg(bg);
        }
    }
}

fn tv_area(area: Rect, theme: Theme) -> Option<Rect> {
    if theme == Theme::Station {
        return station::tv_area(area);
    }
    if area.width >= 120 && area.height >= 40 && theme == Theme::Cafe {
        let x = area.x + area.width * 80 / 100;
        let lamp_bottom =
            ((lamp_height(cafe_counter(i32::from(area.height) * 2)) + 1) / 2 + 1) as u16;
        // Two additional terminal rows of quiet wall below the short lamp.
        let y = area.y + lamp_bottom + 2;
        // Reserve the highest jumping pose plus the tallest quota bubble.
        let bubble_top = (cafe_counter(i32::from(area.height) * 2) - 14).max(0) as u16 / 2;
        let height = (area.y + bubble_top.saturating_sub(9))
            .saturating_sub(y)
            .min(12);
        Some(Rect::new(
            x,
            y,
            area.right().saturating_sub(x + 2),
            height.max(4),
        ))
    } else if area.width >= 70 && area.height >= 20 && theme == Theme::Cafe {
        Some(Rect::new(area.x + 1, area.y + 4, area.width - 2, 2))
    } else if area.width >= 120 && area.height >= 32 {
        // Office has a ceiling-wide hardware station; the TV is a shallow strip.
        Some(Rect::new(area.x + 1, area.y + 6, area.width - 2, 1))
    } else {
        None
    }
}

fn render_tv(frame: &mut Frame<'_>, area: Rect, pack: &Pack, data: &Dashboard) {
    let Some(tv) = tv_area(area, pack.theme) else {
        return;
    };
    let count = |v: Option<u64>| v.map(super::format_short).unwrap_or_else(|| "—".into());
    let accounts = if data.requests.is_some() {
        format!("{}/{}", data.active, data.accounts)
    } else {
        "—".into()
    };
    let errors = data
        .requests
        .zip(data.errors)
        .map(|(r, e)| super::error_rate(r, e))
        .unwrap_or_else(|| "—".into());
    let total = if data.context.is_empty() {
        "—".into()
    } else {
        super::format_short(data.context.iter().flatten().sum::<f64>() as u64)
    };
    if tv.height <= 3 {
        let context = format!("Context Usage 24h · {total} tokens");
        label(
            frame,
            Rect::new(tv.x, tv.y, tv.width, 1),
            &format!(
                "▣ Requests {} · Tokens {} · Accounts {} · Error Rate {}{}",
                count(data.requests),
                count(data.tokens),
                accounts,
                errors,
                if tv.height == 1 {
                    format!(" · {context}")
                } else {
                    String::new()
                }
            ),
            pack.color('e'),
            pack.color('n'),
            true,
        );
        if tv.height == 1 {
            return;
        }
        let label_width = tv.width.min(35);
        label(
            frame,
            Rect::new(tv.x, tv.y + 1, label_width, 1),
            &context,
            pack.color('j'),
            pack.color('n'),
            false,
        );
        line_chart(
            frame,
            Rect::new(tv.x + label_width, tv.y + 1, tv.width - label_width, 1),
            &data.context,
            data.context.iter().flatten().copied().fold(1.0, f64::max),
            pack.color('h'),
            pack.color('n'),
        );
        return;
    }
    frame.render_widget(Clear, tv);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(pack.color('t')))
            .style(Style::default().bg(pack.color('n'))),
        tv,
    );
    let inner = Rect::new(
        tv.x + 2,
        tv.y + 1,
        tv.width.saturating_sub(4),
        tv.height - 2,
    );
    let compact = inner.height < 9;
    let rows = if inner.height < 5 {
        vec![
            format!("Req {} · Tok {}", count(data.requests), count(data.tokens)),
            format!("Accounts {accounts}"),
            format!("Error Rate {errors}"),
            format!("Context 24h {total}"),
        ]
    } else if compact {
        vec![
            format!("Requests {}", count(data.requests)),
            format!("Tokens {}", count(data.tokens)),
            format!("Accounts {accounts}"),
            format!("Error Rate {errors}"),
            format!("Context Usage 24h · {total}"),
        ]
    } else {
        vec![
            "▣ GATEWAY".into(),
            format!("Requests   {}", count(data.requests)),
            format!("Tokens     {}", count(data.tokens)),
            format!("Accounts   {accounts}"),
            format!("Error Rate {errors}"),
            "Context Usage 24h".into(),
        ]
    };
    for (row, text) in rows.iter().enumerate() {
        label(
            frame,
            Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            text,
            pack.color(if row == 0 { 'j' } else { 'e' }),
            pack.color('n'),
            row == 0,
        );
    }
    let chart_y = rows.len() as u16;
    line_chart(
        frame,
        Rect::new(
            inner.x,
            inner.y + chart_y,
            inner.width,
            inner.height.saturating_sub(chart_y).min(2),
        ),
        &data.context,
        data.context.iter().flatten().copied().fold(1.0, f64::max),
        pack.color('h'),
        pack.color('n'),
    );
    if compact && inner.width >= 23 && inner.height <= 5 {
        line_chart(
            frame,
            Rect::new(inner.x + 18, inner.y + chart_y - 1, inner.width - 18, 1),
            &data.context,
            data.context.iter().flatten().copied().fold(1.0, f64::max),
            pack.color('h'),
            pack.color('n'),
        );
    }
    if !compact {
        label(
            frame,
            Rect::new(inner.x, inner.y + 8, inner.width, 1),
            &format!("{total} tokens · active/all"),
            pack.color('k'),
            pack.color('n'),
            false,
        );
    }
    label(
        frame,
        Rect::new(tv.x + tv.width / 2 - 2, tv.bottom(), 4, 1),
        "╲━━╱",
        pack.color('t'),
        pack.color('w'),
        false,
    );
}

fn render_refresh_bot(frame: &mut Frame<'_>, area: Rect, pack: &Pack, ms: u64, scope: &str) {
    if area.width < 22 || area.height < 10 {
        return;
    }
    let x = area.right().saturating_sub(24).max(area.x);
    let y = footer_area(area).y.saturating_sub(3).max(area.y);
    let arm = ["/", "—", "\\", "|"][(ms / 120 % 4) as usize];
    for (row, text) in [
        "   ┌─┴─┐".into(),
        format!(" {arm} [o o]  ↻ {scope}"),
        "   ╰┬─┬╯".into(),
    ]
    .iter()
    .enumerate()
    {
        label(
            frame,
            Rect::new(x, y + row as u16, 22, 1),
            text,
            pack.color('j'),
            pack.color('r'),
            true,
        );
    }
}

fn render_system(
    frame: &mut Frame<'_>,
    area: Rect,
    pack: &Pack,
    sample: &super::system_metrics::Snapshot,
    ms: u64,
) {
    use super::system_metrics::bytes;
    if area.width < 20 || area.height < 6 {
        return;
    }
    let empty = super::system_metrics::Snapshot::default();
    let sample = if sample
        .sampled_at
        .is_some_and(|at| at.elapsed() > Duration::from_secs(5))
    {
        &empty
    } else {
        sample
    };
    let pct = |value: Option<f64>| {
        value
            .map(|v| format!("{v:.0}%"))
            .unwrap_or_else(|| "N/A".into())
    };
    let capacity = |value: Option<(u64, u64)>| {
        value
            .map(|(used, total)| {
                format!(
                    "{}/{} {}",
                    bytes(used as f64),
                    bytes(total as f64),
                    pct(Some(used as f64 * 100.0 / total.max(1) as f64))
                )
            })
            .unwrap_or_else(|| "N/A".into())
    };
    let rate = |value: Option<(f64, f64)>, down: &str, up: &str| {
        value
            .map(|(r, w)| format!("{down}{}/s {up}{}/s", bytes(r), bytes(w)))
            .unwrap_or_else(|| "N/A".into())
    };
    let names = [
        format!("CPU {}", pct(sample.cpu)),
        format!(
            "GPU {}{}",
            pct(sample.gpu),
            if sample.gpu_source == super::system_metrics::GpuSource::IntelClients {
                "*"
            } else {
                ""
            }
        ),
        format!("RAM {}", capacity(sample.memory)),
        format!("DISK / {}", capacity(sample.disk)),
        format!("NET {}", rate(sample.network, "↓", "↑")),
    ];
    let details = [
        "LOCAL".into(),
        match sample.gpu_source {
            super::system_metrics::GpuSource::IntelClients => "Intel · *readable clients".into(),
            super::system_metrics::GpuSource::IntelPmu => "Intel · system-wide".into(),
            super::system_metrics::GpuSource::MacIoreg => "Apple · shared memory".into(),
            _ => format!("VRAM {}", capacity(sample.vram)),
        },
        "used / total".into(),
        format!("I/O {}", rate(sample.disk_io, "r", "w")),
        if sample.interface.is_empty() {
            "LOCAL · no route".into()
        } else {
            format!("LOCAL · {}", clean(&sample.interface))
        },
    ];
    if pack.theme == Theme::Station {
        if station::render_system(frame, area, pack, sample, ms, &names, &details) {
            return;
        }
    } else if pack.theme == Theme::Cafe {
        if let Some(rack) = CafeRack::for_area(area) {
            render_cafe_rack(frame, rack, pack, sample, ms, &names, &details);
            return;
        }
    }
    let wide = area.width >= 120 && area.height >= 32;
    if wide {
        let width = area.width.min(150);
        let mut c = Canvas::new(width as usize, 6, pack);
        c.rect(0, 0, i32::from(width), 6, 'w');
        for index in 0..5 {
            let x = ((index * 2 + 1) * usize::from(width) / 10) as i32;
            let load = match index {
                0 => sample.cpu,
                1 => sample.gpu,
                2 => sample
                    .memory
                    .map(|(a, b)| a as f64 * 100.0 / b.max(1) as f64),
                3 => sample.disk.map(|(a, b)| a as f64 * 100.0 / b.max(1) as f64),
                _ => sample.network.map(|(a, b)| (a + b).min(100.0)),
            };
            let role = if load.is_none() {
                'k'
            } else if load.is_some_and(|v| v >= 90.0) && index != 4 {
                'a'
            } else {
                'h'
            };
            match index {
                0 | 1 => {
                    c.rect(x - 9, 1, 18, 5, 't');
                    c.rect(x - 8, 2, 16, 3, 'i');
                    let tick = if load.is_some_and(|v| v > 0.0) {
                        ms / (260 - load.unwrap().clamp(0.0, 100.0) as u64 * 2)
                    } else {
                        0
                    };
                    for fan in if index == 0 {
                        vec![x]
                    } else {
                        vec![x - 4, x + 4]
                    } {
                        c.rect(fan, 3, 1, 1, role);
                        for (dx, dy) in if tick % 2 == 0 {
                            [(-2, 0), (2, 0), (0, -1), (0, 1)]
                        } else {
                            [(-2, -1), (2, 1), (2, -1), (-2, 1)]
                        } {
                            c.rect(fan + dx, 3 + dy, 1, 1, role);
                        }
                    }
                    for pin in (x - 7..x + 8).step_by(3) {
                        c.rect(pin, 0, 1, 1, 'm');
                    }
                }
                2 => {
                    c.rect(x - 10, 1, 20, 4, 't');
                    for chip in 0..6 {
                        c.rect(
                            x - 9 + chip * 3,
                            2,
                            2,
                            2,
                            if load.is_some_and(|v| v / 100.0 * 6.0 > f64::from(chip)) {
                                role
                            } else {
                                'i'
                            },
                        );
                        c.rect(x - 9 + chip * 3, 5, 2, 1, 'm');
                    }
                    if sample.memory.is_some() {
                        c.rect(x - 9 + ((ms / 240) % 18) as i32, 1, 1, 1, 'j');
                    }
                }
                3 => {
                    c.rect(x - 9, 0, 18, 6, 't');
                    for shelf in [1, 3] {
                        c.rect(x - 8, shelf, 16, 1, 'i');
                        c.rect(x - 6, shelf, 4, 1, 'm');
                    }
                    let busy = sample.disk_io.is_some_and(|(r, w)| r + w > 0.0);
                    c.rect(
                        x + 5,
                        3,
                        2,
                        1,
                        if busy && ms / 180 % 2 == 0 { 'j' } else { 'k' },
                    );
                }
                _ => {
                    c.rect(x - 8, 3, 16, 3, 't');
                    c.rect(x - 7, 0, 1, 3, 'm');
                    c.rect(x + 6, 0, 1, 3, 'm');
                    for led in 0..4 {
                        c.rect(x - 6 + led * 3, 4, 1, 1, role);
                    }
                    if let Some((rx, tx)) = sample.network {
                        if rx > 0.0 {
                            c.rect(x - 8 + ((ms / 90) % 16) as i32, 1, 2, 1, 'h');
                        }
                        if tx > 0.0 {
                            c.rect(x + 6 - ((ms / 90) % 16) as i32, 2, 2, 1, 'j');
                        }
                    }
                }
            }
        }
        c.paint(frame, Rect::new(area.x, area.y + 1, width, 3));
        for unit in 0..3 {
            let center = ((unit * 2 + 1) * usize::from(width) / 10) as u16;
            let values = sample.history.iter().map(|v| v[unit]).collect::<Vec<_>>();
            line_chart(
                frame,
                Rect::new(area.x + center - 8, area.y + 2, 16, 1),
                &values,
                100.0,
                pack.color(['h', 'j', 'z'][unit]),
                pack.color('n'),
            );
        }
        for index in 0..5 {
            let left = area.x + index as u16 * width / 5;
            let right = area.x + (index as u16 + 1) * width / 5;
            label(
                frame,
                Rect::new(left, area.y + 4, right - left, 1),
                &names[index],
                pack.color('e'),
                pack.color('w'),
                true,
            );
            label(
                frame,
                Rect::new(left, area.y + 5, right - left, 1),
                &details[index],
                pack.color('k'),
                pack.color('w'),
                false,
            );
        }
    } else {
        let cpu_fan = if sample.cpu.is_some_and(|v| v > 0.0) {
            ["+", "×"][(ms / 160 % 2) as usize]
        } else {
            "·"
        };
        let first = format!("LOCAL {cpu_fan} {} · {} · {}", names[0], names[1], names[2]);
        let second = format!("{} · {}", names[3], names[4]);
        let third = format!("{} · {} · {}", details[1], details[3], details[4]);
        let (first, second, third) = if area.width < 60 {
            let percent = |value: Option<(u64, u64)>| {
                pct(value.map(|(used, total)| used as f64 * 100.0 / total.max(1) as f64))
            };
            (
                format!("L {cpu_fan} {} {}", names[0], names[1]),
                format!(
                    "RAM {} · DISK {}",
                    percent(sample.memory),
                    percent(sample.disk)
                ),
                names[4].clone(),
            )
        } else {
            (first, second, third)
        };
        for (index, text) in [first, second, third]
            .iter()
            .enumerate()
            .take(if area.height >= 12 { 3 } else { 1 })
        {
            label(
                frame,
                Rect::new(area.x, area.y + 1 + index as u16, area.width, 1),
                text,
                pack.color(if index == 2 { 'k' } else { 'e' }),
                pack.color('w'),
                index < 2,
            );
        }
    }
}

/// The cabinet must fit above even an eight-row bubble and a bobbing cat.
/// Small terminals use five adjacent rack bays instead of stealing quota rows.
#[derive(Clone, Copy)]
struct CafeRack {
    area: Rect,
    stacked: bool,
}

impl CafeRack {
    fn for_area(area: Rect) -> Option<Self> {
        let head = (cafe_counter(i32::from(area.height) * 2) - 12).max(0) as u16 / 2;
        let bubble_top = head.saturating_sub(9).max(4);
        if area.width >= 120 && bubble_top >= 16 {
            let width = (area.width * 44 / 100)
                .clamp(50, 84)
                .min(area.width * 58 / 100 - 20);
            Some(Self {
                area: Rect::new(area.x + 1, area.y + 2, width, 13),
                stacked: true,
            })
        } else if area.width >= 70 && area.height >= 20 {
            Some(Self {
                area: Rect::new(area.x + 1, area.y + 1, area.width - 2, 3),
                stacked: false,
            })
        } else {
            None
        }
    }
}

fn rack_load(sample: &super::system_metrics::Snapshot, unit: usize) -> Option<f64> {
    match unit {
        0 => sample.cpu,
        1 => sample.gpu,
        2 => sample
            .memory
            .map(|(used, total)| used as f64 * 100.0 / total.max(1) as f64),
        3 => sample
            .disk
            .map(|(used, total)| used as f64 * 100.0 / total.max(1) as f64),
        // Network throughput is not a utilization percentage: no link speed is known.
        _ => sample.network.map(|_| 0.0),
    }
}

fn rack_fan(c: &mut Canvas<'_>, x: i32, y: i32, load: Option<f64>, ms: u64) {
    let role = if load.is_none() {
        'd'
    } else if load.is_some_and(|v| v >= 90.0) {
        'a'
    } else {
        'h'
    };
    c.rect(x - 2, y - 1, 5, 3, 'i');
    let tick = load
        .filter(|v| *v > 0.0)
        .map_or(0, |v| ms / (300 - v.clamp(0.0, 100.0) as u64 * 2));
    c.rect(x, y, 1, 1, role);
    for (dx, dy) in if tick % 2 == 0 {
        [(-2, 0), (2, 0), (0, -1), (0, 1)]
    } else {
        [(-2, -1), (2, 1), (2, -1), (-2, 1)]
    } {
        c.rect(x + dx, y + dy, 1, 1, role);
    }
}

fn rack_device(
    c: &mut Canvas<'_>,
    area: Rect,
    unit: usize,
    sample: &super::system_metrics::Snapshot,
    ms: u64,
) {
    let (x, y, w) = (i32::from(area.x), i32::from(area.y), i32::from(area.width));
    let load = rack_load(sample, unit);
    let role = if load.is_none() {
        'd'
    } else if unit != 4 && load.is_some_and(|v| v >= 90.0) {
        'a'
    } else {
        'h'
    };
    match unit {
        0 | 1 => {
            rack_fan(c, x + w / 3, y + 1, load, ms);
            if w >= 12 {
                rack_fan(c, x + w * 2 / 3, y + 1, load, ms + 70);
            }
        }
        2 => {
            for chip in 0..5 {
                let px = x + chip * w / 5;
                c.rect(
                    px,
                    y,
                    (w / 5 - 1).max(1),
                    2,
                    if load.is_some_and(|v| v / 20.0 > f64::from(chip)) {
                        role
                    } else {
                        'i'
                    },
                );
                c.rect(px, y + 2, (w / 5 - 1).max(1), 1, 'm');
            }
        }
        3 => {
            for bay in 0..3 {
                let px = x + bay * w / 3;
                c.rect(px, y, (w / 3 - 1).max(1), 3, 't');
                c.rect(px + 1, y + 1, (w / 3 - 3).max(1), 1, 'i');
                let busy = sample.disk_io.is_some_and(|(r, w)| r + w > 0.0);
                c.rect(
                    px,
                    y + 2,
                    1,
                    1,
                    if busy && (ms / 180 + bay as u64) % 2 == 0 {
                        'j'
                    } else {
                        'd'
                    },
                );
            }
        }
        _ => {
            for port in 0..4 {
                c.rect(x + port * w / 4, y + 1, (w / 4 - 1).max(1), 2, 't');
                c.rect(x + port * w / 4, y + 2, 1, 1, role);
            }
            if let Some((rx, tx)) = sample.network {
                let step = ((ms / 95) % w.max(1) as u64) as i32;
                if rx > 0.0 {
                    c.rect(x + step, y, 1, 1, 'h');
                }
                if tx > 0.0 {
                    c.rect(x + w - 1 - step, y + 1, 1, 1, 'j');
                }
            }
        }
    }
}

fn render_cafe_rack(
    frame: &mut Frame<'_>,
    rack: CafeRack,
    pack: &Pack,
    sample: &super::system_metrics::Snapshot,
    ms: u64,
    names: &[String; 5],
    details: &[String; 5],
) {
    use super::system_metrics::bytes;
    let area = rack.area;
    let (w, h) = (area.width, area.height * 2);
    let chart_width = (w / 3).clamp(15, 24);
    let mut c = Canvas::new(w as usize, h as usize, pack);
    c.rect(0, 0, i32::from(w), i32::from(h), 'w');
    c.rect(0, 0, i32::from(w), i32::from(h - 1), 't');
    c.rect(2, 1, i32::from(w - 4), i32::from(h - 3), 'n');
    if rack.stacked {
        for unit in 0..5 {
            let y = 2 + unit as u16 * 4;
            c.rect(4, i32::from(y), i32::from(w - 8), 3, 'n');
            c.rect(3, i32::from(y + 3), i32::from(w - 6), 1, 't');
            // Mounting ears, rack screws and pull handles join all five units.
            for x in [1, w - 2] {
                c.rect(i32::from(x), i32::from(y + 1), 1, 1, 'm');
            }
            for x in [4, w - 6] {
                c.rect(i32::from(x), i32::from(y), 2, 3, 'm');
                c.rect(i32::from(x), i32::from(y + 1), 1, 1, 'i');
            }
            if unit >= 3 {
                rack_device(&mut c, Rect::new(7, y, chart_width, 3), unit, sample, ms);
            }
        }
        // Wall shelf, two brackets, and bundled patch cables beneath the switch.
        c.rect(0, 23, i32::from(w), 1, 'm');
        for x in [6, w - 8] {
            c.rect(i32::from(x), 24, 2, 2, 't');
        }
        for x in [11, 14, 17] {
            c.rect(x, 22, 1, 3, 'c');
        }
    } else {
        for unit in 0..5 {
            let left = 2 + unit as u16 * (w - 4) / 5;
            let right = 2 + (unit as u16 + 1) * (w - 4) / 5;
            c.rect(i32::from(left), 1, 1, 4, 'm');
            // Preserve rack artwork even in the three-line compact variant.
            rack_device(
                &mut c,
                Rect::new(left + 2, 3, right - left - 3, 3),
                unit,
                sample,
                ms,
            );
        }
    }
    c.paint(frame, area);
    if rack.stacked {
        label(
            frame,
            Rect::new(area.x + 4, area.y, w - 8, 1),
            "Machine",
            pack.color('k'),
            pack.color('n'),
            true,
        );
        for unit in 0..5 {
            let x = area.x + 7 + chart_width + 2;
            let y = area.y + 1 + unit as u16 * 2;
            let width = w - (x - area.x) - 7;
            let color = if rack_load(sample, unit).is_none() {
                'd'
            } else {
                'e'
            };
            label(
                frame,
                Rect::new(x, y, width, 1),
                &names[unit],
                pack.color(color),
                pack.color('n'),
                true,
            );
            label(
                frame,
                Rect::new(x, y + 1, width, 1),
                if unit == 1 {
                    &details[unit]
                } else if unit <= 2 {
                    "0—100% · 1s"
                } else {
                    &details[unit]
                },
                pack.color('k'),
                pack.color('n'),
                false,
            );
            if unit <= 2 {
                let chart_area = Rect::new(area.x + 7, y, chart_width, 2);
                let values = sample.history.iter().map(|v| v[unit]).collect::<Vec<_>>();
                bar_chart(
                    frame,
                    chart_area,
                    &values,
                    100.0,
                    pack.color(['h', 'j', 'z'][unit]),
                    pack.color('n'),
                );
            }
        }
    } else {
        label(
            frame,
            Rect::new(area.x + 3, area.y, w - 6, 1),
            if sample.gpu_source == super::system_metrics::GpuSource::IntelClients {
                "Machine · NET B/s · GPU* readable clients"
            } else {
                "Machine · NET B/s"
            },
            pack.color('k'),
            pack.color('n'),
            true,
        );
        for unit in 0..5 {
            let left = 2 + unit as u16 * (w - 4) / 5;
            let right = 2 + (unit as u16 + 1) * (w - 4) / 5;
            let readout = match unit {
                0 | 1 => names[unit].clone(),
                2 | 3 => format!(
                    "{} {}",
                    if unit == 2 { "RAM" } else { "DISK /" },
                    rack_load(sample, unit)
                        .map(|v| format!("{v:.0}%"))
                        .unwrap_or_else(|| "N/A".into())
                ),
                _ => sample
                    .network
                    .map(|(rx, tx)| format!("NET ↓{}↑{}", bytes(rx), bytes(tx)))
                    .unwrap_or_else(|| "NET N/A".into()),
            };
            label(
                frame,
                Rect::new(area.x + left + 1, area.y + 1, right - left - 1, 1),
                &readout,
                pack.color('e'),
                pack.color('n'),
                true,
            );
            if unit <= 2 {
                let values = sample.history.iter().map(|v| v[unit]).collect::<Vec<_>>();
                bar_chart(
                    frame,
                    Rect::new(area.x + left + 2, area.y + 2, right - left - 3, 1),
                    &values,
                    100.0,
                    pack.color(['h', 'j', 'z'][unit]),
                    pack.color('n'),
                );
            }
        }
    }
}

fn render_office(
    frame: &mut Frame<'_>,
    layout: &SceneLayout,
    pack: &Pack,
    accounts: &[&SceneAccount],
    ms: u64,
    all_limits: bool,
) {
    let area = layout.stage;
    if area.is_empty() {
        return;
    }
    let mut c = Canvas::new(area.width as usize, area.height as usize * 2, pack);
    let (w, h) = (c.width as i32, c.height as i32);
    c.rect(0, 0, w, h, 'w');
    c.rect(0, 9, w, h - 9, 'f');
    for x in (3..w).step_by(9) {
        c.rect(x, 10, 1, h - 10, 'g');
    }
    for x in (8..w - 18).step_by(40) {
        rainy_window(&mut c, x, 0, 23, 8, ms);
    }
    // Furniture stays fixed while the cats step sideways to make room.
    for (index, card) in layout.seats.iter().take(accounts.len().max(1)).enumerate() {
        let account = accounts.get(index).copied();
        let tint = account.map_or('h', |account| account.status);
        let x = i32::from(card.x - area.x) + i32::from(card.width) / 2;
        let desk_y = i32::from(card.bottom() - area.y + 2) * 2 + 8;
        if desk_y < 7 {
            continue;
        }
        let phase = if tint == 'd' {
            0
        } else {
            ms + index as u64 * 311
        };
        let desk_width = i32::from(card.width).min(24);
        c.rect(x - desk_width / 2, desk_y + 4, desk_width, 5, 'i');
        c.rect(x - desk_width / 2, desk_y, desk_width, 2, 'm');
        c.rect(x - desk_width / 2 + 1, desk_y + 2, 1, 6, 't');
        c.rect(x + desk_width / 2 - 2, desk_y + 2, 1, 6, 't');
        c.sprite("terminal", x - 7, desk_y - 8, phase, Some(tint));
        if account.is_some() {
            let actor = layout.cards[index];
            let actor_x = i32::from(actor.x - area.x) + i32::from(actor.width) / 2;
            c.cafe_cat(
                index,
                account,
                actor_x - 6,
                (desk_y + 8).min(i32::from(layout.footer.y - area.y) * 2 - 1),
                ms,
                actor_motion(layout, index, account, ms, all_limits),
            );
        }
        if account.is_some_and(|account| {
            account
                .recent_until
                .is_some_and(|until| Instant::now() < until)
        }) {
            c.rect(x - 6 + ((ms / 100) % 12) as i32, desk_y - 5, 1, 1, 'j');
        }
        if card.width >= 24 {
            c.sprite(
                "plant",
                i32::from(card.right() - area.x) - 11,
                desk_y - 6,
                phase,
                None,
            );
        }
    }
    c.paint(frame, area);
}

/// One terminal cell is two square-ish pixels; labels are a separate text layer.
struct Canvas<'a> {
    width: usize,
    height: usize,
    pixels: Vec<Color>,
    pack: &'a Pack,
}

impl<'a> Canvas<'a> {
    fn pixel_mut(&mut self, x: i32, y: i32) -> Option<&mut Color> {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return None;
        }
        self.pixels.get_mut(y as usize * self.width + x as usize)
    }
    fn new(width: usize, height: usize, pack: &'a Pack) -> Self {
        Self {
            width,
            height,
            pixels: vec![pack.color('b'); width * height],
            pack,
        }
    }

    fn rect(&mut self, x: i32, y: i32, width: i32, height: i32, role: char) {
        let color = self.pack.color(role);
        self.color_rect(x, y, width, height, color);
    }

    fn color_rect(&mut self, x: i32, y: i32, width: i32, height: i32, color: Color) {
        for py in y.max(0)..(y + height).min(self.height as i32) {
            for px in x.max(0)..(x + width).min(self.width as i32) {
                self.pixels[py as usize * self.width + px as usize] = color;
            }
        }
    }

    fn cafe_cat(
        &mut self,
        index: usize,
        account: Option<&SceneAccount>,
        x: i32,
        counter: i32,
        ms: u64,
        motion: (i32, i32, u64),
    ) {
        let status = account.map_or('h', |a| a.status);
        let (stroll, bob, phase) = motion;
        let activity = cat_activity(index, status, ms);
        let pose = cat_pose(index, status, ms);
        let name = if index % 2 == 0 { "cat" } else { "cat_light" };
        let fur = account.map(|a| provider_color(&a.provider));
        let (x, y) = (x + stroll, counter - 11 - bob);
        if activity == CatActivity::Roll {
            self.rolling_cat(
                name,
                x,
                counter - bob,
                cat_action_age(index, ms),
                status,
                fur,
            );
        } else {
            self.posed_cat(name, x, y, status, fur, &pose);
        }
        let coat = fur.unwrap_or(self.pack.color('e'));
        match activity {
            CatActivity::Play => {
                // The forepaw leads the toy; the haunches remain planted.
                self.color_rect(x + 7, y + 7 - pose.paw, 3, 1, coat);
                self.color_rect(x + 9, y + 7 - pose.paw, 1, 2, coat);
                let bounce = ((phase as f64 / 600.0).sin().abs() * 1.7).round() as i32;
                self.rect(x + 11, counter - 1 - bob - bounce, 1, 1, 'x');
            }
            CatActivity::Groom => {
                self.color_rect(x + 7, y + 6 - pose.paw, 2, 2, coat);
                self.color_rect(x + 6, y + 5 - pose.paw, 2, 1, coat);
            }
            CatActivity::Stretch if pose.settle > 0.2 => {
                self.color_rect(x + 6, y + 9, 5, 1, coat);
                self.color_rect(x + 10, y + 10, 2, 1, mix(coat, self.pack.color('e'), 50));
            }
            CatActivity::Angry => {
                self.rect(x + 2 + pose.look, y + 2, 1, 1, 'a');
                self.rect(x + 6 + pose.look, y + 2, 1, 1, 'a');
            }
            _ => {}
        }
    }

    fn soft_pixel(&mut self, x: f64, y: f64, color: Color, opacity: f64) {
        let (left, top) = (x.floor() as i32, y.floor() as i32);
        for (dx, wx) in [(0, 1.0 - x.fract()), (1, x.fract())] {
            for (dy, wy) in [(0, 1.0 - y.fract()), (1, y.fract())] {
                let amount = (opacity * wx * wy * 255.0).round() as u16;
                if amount > 0 {
                    if let Some(pixel) = self.pixel_mut(left + dx, top + dy) {
                        *pixel = mix(*pixel, color, amount.min(255));
                    }
                }
            }
        }
    }

    fn posed_cat(
        &mut self,
        name: &str,
        x: i32,
        y: i32,
        status: char,
        fur: Option<Color>,
        pose: &CatPose,
    ) {
        let coat = fur.unwrap_or(self.pack.color('e'));
        let sleeping = pose.settle > 0.95;
        // The tail is drawn once, behind the body. Fractional coverage lets a
        // relaxed curve move at 20 FPS without flashing between rigid frames.
        let tail_tip =
            (4.0 + pose.tail * 1.8) * (1.0 - pose.settle) + (8.5 + pose.tail * 0.5) * pose.settle;
        for segment in 0..18 {
            let t = segment as f64 / 17.0;
            let tx = 9.0 + 6.0 * t - 3.0 * t * t;
            let ty = 8.0 - (8.0 - tail_tip) * t * t;
            self.soft_pixel(f64::from(x) + tx, f64::from(y) + ty, coat, 0.85);
        }
        // Preserve optional API-1 artwork and provider colors. The office pack
        // uses the same rig with its small procedural cat as a source frame.
        let mut source = Canvas::new(13, 11, self.pack);
        source.cat_sprite(
            if sleeping { "cat_sleep" } else { name },
            0,
            0,
            2500,
            status,
            fur,
        );
        let background = self.pack.color('b');
        for sy in 0..11 {
            for sx in 0..10 {
                let color = source.pixels[sy * 13 + sx];
                if color == background {
                    continue;
                }
                let drop = if sleeping {
                    0
                } else {
                    ((10 - sy) as f64 * 0.3 * pose.settle).round() as i32
                };
                let head = sy <= if sleeping { 7 } else { 5 };
                let look = if head { pose.look } else { 0 };
                let ear = if sy == 0 && sx <= 2 { pose.ear } else { 0 };
                let foot_lift = if sy >= 9 && !sleeping {
                    i32::from(
                        (sx <= 4 && pose.stride > 0.35)
                            || ((6..=8).contains(&sx) && pose.stride < -0.35),
                    )
                } else {
                    0
                };
                self.color_rect(
                    x + sx as i32 + look + ear,
                    y + sy as i32 + drop - foot_lift,
                    1,
                    1,
                    color,
                );
            }
        }
        // Chest expansion is local: neither the paws nor the entire sprite bob.
        self.soft_pixel(
            f64::from(x + 9),
            f64::from(y + 6 + pose.head_drop / 2),
            coat,
            pose.breath * 0.45,
        );
        if pose.blink && !sleeping {
            let shade = mix(coat, self.pack.color('n'), 100);
            let eye_y = y + 3 + (7.0 * 0.3 * pose.settle).round() as i32;
            self.color_rect(x + 2 + pose.look, eye_y, 1, 1, shade);
            self.color_rect(x + 6 + pose.look, eye_y, 1, 1, shade);
        }
        // A tiny warm highlight gives the coat depth without changing identity.
        if !sleeping {
            self.color_rect(
                x + 3,
                y + 8 + (pose.settle > 0.6) as i32,
                1,
                1,
                mix(coat, self.pack.color('e'), 35),
            );
        }
    }

    fn rolling_cat(
        &mut self,
        name: &str,
        x: i32,
        floor: i32,
        phase: u64,
        status: char,
        fur: Option<Color>,
    ) {
        let pose = roll_pose(phase);
        if pose <= 1 {
            self.cat_sprite(
                if pose == 0 { name } else { "cat_sleep" },
                x,
                floor - 11,
                phase,
                status,
                fur,
            );
            return;
        }
        let coat = fur.unwrap_or(self.pack.color('e'));
        let shadow = mix(coat, self.pack.color('n'), 80);
        let belly = mix(coat, self.pack.color('e'), 75);
        // Long, low body stays in contact with the counter. Only the shoulders,
        // belly and paws change position; the head does not orbit the torso.
        self.color_rect(x + 4, floor - 6, 6, 1, coat);
        self.color_rect(x + 3, floor - 5, 8, 4, coat);
        self.color_rect(x + 4, floor - 1, 6, 1, shadow);
        self.color_rect(x + 1, floor - 6, 4, 4, coat);
        self.color_rect(x + 1, floor - 7, 1, 2, coat);
        self.color_rect(x + 4, floor - 7, 1, 2, coat);
        self.rect(x + 2, floor - 4, 1, 1, 'n');
        self.rect(x + 4, floor - 4, 1, 1, 'n');
        self.rect(x + 3, floor - 3, 1, 1, 'x');
        self.rect(x + 5, floor - 4, 1, 2, status);
        self.color_rect(x + 10, floor - 3, 3, 1, coat);
        self.color_rect(x + 12, floor - 5, 1, 2, coat);
        if pose == 3 {
            self.color_rect(x + 6, floor - 5, 3, 3, belly);
            let twitch = (phase / 180 % 2) as i32;
            for px in [6, 9] {
                self.color_rect(x + px, floor - 8 + twitch, 1, 3, coat);
                self.rect(x + px, floor - 8 + twitch, 1, 1, 'x');
            }
            self.color_rect(x + 5, floor - 2, 2, 2, belly);
            self.color_rect(x + 9, floor - 2, 2, 2, belly);
        } else {
            self.color_rect(x + 5, floor - 2, 6, 1, shadow);
            self.color_rect(x + 6, floor - 1, 2, 1, belly);
            self.color_rect(x + 9, floor - 1, 2, 1, belly);
        }
    }

    /// API-1 Office packs predate cat sprites. Keep them compatible without
    /// bundling another theme's artwork or retaining human figures on screen.
    fn cat_sprite(
        &mut self,
        name: &str,
        x: i32,
        y: i32,
        phase: u64,
        status: char,
        fur: Option<Color>,
    ) {
        if self.pack.sprites.contains_key(name) {
            self.colored_sprite(name, x, y, phase, Some(status), fur);
            return;
        }
        let coat = fur.unwrap_or(self.pack.color('e'));
        let shade = mix(coat, self.pack.color('n'), 80);
        let sleepy = name == "cat_sleep";
        let head = y + if sleepy { 3 } else { 0 };
        self.color_rect(x + 2, head, 1, 2, coat);
        self.color_rect(x + 6, head, 1, 2, coat);
        self.color_rect(x + 1, head + 2, 7, 3, coat);
        self.rect(x + 2, head + 3, 1, 1, if sleepy { 's' } else { 'n' });
        self.rect(x + 6, head + 3, 1, 1, if sleepy { 's' } else { 'n' });
        self.rect(x + 4, head + 4, 1, 1, 'x');
        self.color_rect(
            x + 3,
            y + 5 + i32::from(sleepy) * 3,
            7,
            if sleepy { 2 } else { 5 },
            coat,
        );
        self.color_rect(x + 2, y + 10, 3, 1, shade);
        self.color_rect(x + 7, y + 10, 3, 1, shade);
        self.rect(x + 4, head + 5, 2, 1, status);
        self.color_rect(x + 9, y + 8, 3, 1, coat);
        self.color_rect(x + 11, y + 6 + (phase / 300 % 2) as i32, 1, 2, coat);
    }

    fn glow(&mut self, x: i32, y: i32, radius: i32, role: char, strength: u16) {
        let color = self.pack.color(role);
        for py in (y - radius).max(0)..(y + radius).min(self.height as i32) {
            for px in (x - radius).max(0)..(x + radius).min(self.width as i32) {
                let distance = (px - x).pow(2) + (py - y).pow(2);
                let amount = ((radius * radius - distance).max(0) as u32
                    * u32::from(strength.min(255))
                    / (radius * radius) as u32) as u16;
                let index = py as usize * self.width + px as usize;
                self.pixels[index] = mix(self.pixels[index], color, amount);
            }
        }
    }

    fn sprite(&mut self, name: &str, x: i32, y: i32, elapsed: u64, tint: Option<char>) {
        self.colored_sprite(name, x, y, elapsed, tint, None);
    }

    fn colored_sprite(
        &mut self,
        name: &str,
        x: i32,
        y: i32,
        elapsed: u64,
        tint: Option<char>,
        fur: Option<Color>,
    ) {
        let sprite = &self.pack.sprites[name];
        let frame = if matches!(name, "cat" | "cat_light") {
            // Blinks last only ~100 ms, separated by several seconds; tail
            // motion no longer forces a long closed-eye frame every second.
            let blink = elapsed % 6700 < 110;
            if blink && sprite.frames.len() > 2 {
                2
            } else {
                (elapsed / 900) as usize % sprite.frames.len().min(2)
            }
        } else {
            (elapsed / sprite.frame_ms) as usize % sprite.frames.len()
        };
        let pixels = &sprite.frames[frame];
        for (dy, row) in pixels.iter().enumerate() {
            for (dx, mut role) in row.chars().enumerate() {
                if role == '.' {
                    continue;
                }
                if role == 'h' {
                    role = tint.unwrap_or(role);
                }
                let color = if let Some(fur) = fur {
                    match role {
                        'n' if name == "cat" => mix(self.pack.color('n'), fur, 190),
                        'e' if name != "cat" => mix(fur, self.pack.color('e'), 40),
                        's' if name != "cat" => mix(fur, self.pack.color('n'), 75),
                        _ => self.pack.color(role),
                    }
                } else {
                    self.pack.color(role)
                };
                let (px, py) = (x + dx as i32, y + dy as i32);
                if px >= 0 && py >= 0 && px < self.width as i32 && py < self.height as i32 {
                    self.pixels[py as usize * self.width + px as usize] = color;
                }
            }
        }
    }

    fn paint(&self, frame: &mut Frame<'_>, area: Rect) {
        let buffer = frame.buffer_mut();
        for y in 0..area.height as usize {
            for x in 0..area.width as usize {
                let sample = |py: usize| {
                    let sx = x * self.width / usize::from(area.width);
                    let sy = py * self.height / (usize::from(area.height) * 2);
                    self.pixels[sy * self.width + sx]
                };
                let cell = &mut buffer[(area.x + x as u16, area.y + y as u16)];
                cell.reset();
                cell.set_symbol("▀")
                    .set_fg(sample(y * 2))
                    .set_bg(sample(y * 2 + 1));
            }
        }
    }
}

fn mix(from: Color, to: Color, amount: u16) -> Color {
    let (Color::Rgb(r, g, b), Color::Rgb(tr, tg, tb)) = (from, to) else {
        return from;
    };
    let blend =
        |a: u8, b: u8| ((u16::from(a) * (255 - amount) + u16::from(b) * amount) / 255) as u8;
    Color::Rgb(blend(r, tr), blend(g, tg), blend(b, tb))
}

fn rain_hash(mut seed: u64) -> u64 {
    seed = seed.wrapping_add(0x9e3779b97f4a7c15);
    seed = (seed ^ (seed >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    seed = (seed ^ (seed >> 27)).wrapping_mul(0x94d049bb133111eb);
    seed ^ (seed >> 31)
}

struct RainDrop {
    x: f64,
    y: f64,
    length: usize,
    opacity: u16,
    slope: f64,
    splash: f64,
}

fn rain_drop(index: u64, seed: u64, width: i32, height: i32, ms: u64) -> RainDrop {
    let hash = rain_hash(seed ^ index.wrapping_mul(7919));
    let near = index % 3 == 0;
    let speed = if near { 20.0 } else { 9.0 } + (hash % 100) as f64 / 10.0;
    let length = if near {
        3 + (hash % 2) as usize
    } else {
        1 + (hash % 2) as usize
    };
    let period =
        (f64::from(height) + length as f64 + 4.0) / speed + 0.15 + (hash % 71) as f64 / 100.0;
    let time = ms as f64 / 1000.0 + (hash % 1000) as f64 / 113.0;
    let cycle = (time / period).floor() as u64;
    let age = time % period;
    // A new launch point each cycle avoids obvious, repeating vertical lanes.
    let launch = rain_hash(hash ^ cycle.wrapping_mul(104729));
    let slope = 0.18 + 0.045 * (ms as f64 / 9000.0 + (seed % 31) as f64).sin();
    let y = age * speed - length as f64 - 2.0;
    let x =
        (launch % (width.max(1) as u64 * 100)) as f64 / 100.0 + 2.0 - (y + length as f64) * slope;
    RainDrop {
        x,
        y,
        length,
        slope,
        opacity: if near {
            155 + (hash % 45) as u16
        } else {
            55 + (hash % 45) as u16
        },
        splash: if near && hash % 2 == 0 {
            (age - (f64::from(height) + length as f64) / speed) / 0.22
        } else {
            -1.0
        },
    }
}

fn rainy_window(c: &mut Canvas<'_>, x: i32, y: i32, w: i32, h: i32, ms: u64) {
    if w < 4 || h < 4 {
        return;
    }
    c.rect(x, y, w, h, 't');
    for py in 1..h - 1 {
        let color = mix(c.pack.color('v'), c.pack.color('u'), (py * 36 / h) as u16);
        for px in 1..w - 1 {
            if let Some(pixel) = c.pixel_mut(x + px, y + py) {
                *pixel = color;
            }
        }
    }
    for dx in 1..w - 1 {
        let building =
            (2 + ((rain_hash((dx / 5) as u64 + 7) % (h / 4).max(2) as u64) as i32)).min(h - 2);
        c.rect(x + dx, y + h - building - 1, 1, building, 'n');
        if dx % 5 == 2 {
            c.rect(x + dx, y + h - building + 1, 1, 1, 'z');
        }
    }
    let seed = (x as u64).wrapping_mul(37) ^ (y as u64).wrapping_mul(101);
    // Two depth layers: short/dim/distant and longer/brighter/faster foreground.
    let drops = (w * h / 60).clamp(10, 120) as u64;
    let rain_color = c.pack.color('q');
    let mut point = |px: f64, py: f64, amount: u16| {
        // Fractional coverage softens motion without changing simulation speed
        // with the frame rate. Never paint over wood, mullions or the room.
        for (ix, wx) in [
            (px.floor() as i32, 1.0 - px.fract()),
            (px.floor() as i32 + 1, px.fract()),
        ] {
            for (iy, wy) in [
                (py.floor() as i32, 1.0 - py.fract()),
                (py.floor() as i32 + 1, py.fract()),
            ] {
                if ix > 0 && ix < w - 1 && iy > 0 && iy < h - 1 {
                    if let Some(pixel) = c.pixel_mut(x + ix, y + iy) {
                        *pixel = mix(
                            *pixel,
                            rain_color,
                            (f64::from(amount) * wx * wy).round() as u16,
                        );
                    }
                }
            }
        }
    };
    for index in 0..drops {
        let drop = rain_drop(index, seed, w - 2, h - 2, ms);
        for tail in 0..drop.length {
            let fade = 1.0 - tail as f64 / drop.length as f64;
            point(
                drop.x + tail as f64 * drop.slope,
                drop.y - tail as f64,
                (f64::from(drop.opacity) * fade) as u16,
            );
        }
        if (0.0..1.0).contains(&drop.splash) {
            let radius = 0.5 + drop.splash * 2.0;
            let rise = (drop.splash * std::f64::consts::PI).sin() * 1.3;
            let alpha = (120.0 * (1.0 - drop.splash)) as u16;
            point(drop.x - radius, f64::from(h - 2) - rise, alpha);
            point(drop.x + radius, f64::from(h - 2) - rise, alpha);
        }
    }
    c.rect(x + w / 2, y, 1, h, 't');
    c.rect(x, y + h / 2, w, 2, 't');
    c.rect(x - 1, y + h, w + 2, 1, 'm');
}
fn steam(c: &mut Canvas<'_>, x: i32, y: i32, ms: u64) {
    for trail in 0..3 {
        let rise = ((ms / 140 + trail * 3) % 9) as i32;
        let sway = (((ms / 330 + trail) % 3) as i32 - 1).abs();
        let (px, py) = (x + 2 + sway, y - 1 - rise);
        if px >= 0 && px < c.width as i32 && py >= 0 && py < c.height as i32 {
            let index = py as usize * c.width + px as usize;
            c.pixels[index] = mix(c.pixels[index], c.pack.color('e'), (95 - rise * 8) as u16);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CoffeeTool {
    Beans,
    Grinder,
    Kettle,
    Dripper,
    Cups,
    Scale,
    Syrup,
    Espresso,
}

impl CoffeeTool {
    fn width(self) -> i32 {
        match self {
            Self::Espresso => 13,
            Self::Kettle => 9,
            Self::Cups => 8,
            _ => 7,
        }
    }
}

fn coffee_tool(c: &mut Canvas<'_>, tool: CoffeeTool, x: i32, y: i32, ms: u64) {
    match tool {
        CoffeeTool::Beans => {
            c.rect(x + 1, y - 6, 5, 6, 'k');
            c.rect(x + 2, y - 7, 3, 1, 't');
            c.rect(x + 2, y - 4, 3, 2, 'e');
            c.rect(x + 3, y - 4, 1, 2, 'r');
        }
        CoffeeTool::Grinder => {
            c.rect(x + 1, y - 8, 5, 2, 'q');
            c.rect(x + 2, y - 6, 3, 2, 'r');
            c.rect(x + 1, y - 4, 5, 3, 'i');
            c.rect(x + 2, y - 3, 1, 1, 'j');
            c.rect(x, y - 1, 7, 1, 's');
            // Occasional motor light, not a claim about gateway activity.
            c.rect(
                x + 4,
                y - 3,
                1,
                1,
                if ms % 13000 < 1600 { 'h' } else { 't' },
            );
        }
        CoffeeTool::Kettle => {
            c.rect(x + 2, y - 5, 4, 4, 'q');
            c.rect(x + 3, y - 6, 2, 1, 's');
            c.rect(x + 3, y - 7, 1, 1, 'r');
            c.rect(x + 1, y - 4, 1, 3, 's');
            c.rect(x, y - 4, 1, 1, 'q');
            c.rect(x + 6, y - 4, 2, 1, 's');
            c.rect(x + 7, y - 3, 1, 2, 's');
            c.rect(x + 6, y - 1, 2, 1, 's');
            c.rect(x + 1, y - 1, 5, 1, 's');
            if ms % 15000 < 6000 {
                steam(c, x, y - 7, ms / 2);
            }
        }
        CoffeeTool::Dripper => {
            c.rect(x + 1, y - 7, 5, 1, 'e');
            c.rect(x + 2, y - 6, 3, 2, 'o');
            c.rect(x + 3, y - 4, 1, 1, 'k');
            c.rect(x + 2, y - 2, 3, 2, 'q');
            c.rect(x + 5, y - 2, 1, 1, 's');
            if ms % 9000 < 5000 {
                c.rect(x + 3, y - 3 + (ms / 260 % 2) as i32, 1, 1, 'k');
            }
        }
        CoffeeTool::Cups => {
            c.rect(x, y - 1, 7, 1, 'e');
            for level in 0..2 {
                c.rect(
                    x + 1,
                    y - 3 - level * 2,
                    4,
                    2,
                    if level == 0 { 's' } else { 'e' },
                );
                c.rect(x + 5, y - 3 - level * 2, 2, 1, 'e');
            }
        }
        CoffeeTool::Scale => {
            c.rect(x, y - 2, 7, 2, 'i');
            c.rect(x + 4, y - 1, 2, 1, 'h');
            c.rect(x + 1, y - 3, 4, 1, 's');
            c.rect(x + 2, y - 5, 3, 2, 'k');
            c.rect(x + 3, y - 6, 1, 1, 'k');
        }
        CoffeeTool::Syrup => {
            c.rect(x + 1, y - 5, 4, 5, 'o');
            c.rect(x + 2, y - 7, 2, 2, 'k');
            c.rect(x + 2, y - 8, 4, 1, 'r');
            c.rect(x + 1, y - 3, 4, 2, 'e');
            c.rect(x + 2, y - 3, 2, 1, 'z');
        }
        CoffeeTool::Espresso => {
            c.rect(x, y - 7, 12, 3, 's');
            c.rect(x, y - 4, 2, 4, 'i');
            c.rect(x + 10, y - 4, 2, 4, 'i');
            c.rect(x + 2, y - 5, 8, 1, 'n');
            c.rect(x + 3, y - 4, 3, 1, 'r');
            c.rect(x + 6, y - 4, 3, 1, 'k');
            c.rect(x + 4, y - 2, 3, 2, 'e');
            c.rect(x, y - 1, 12, 1, 's');
            c.rect(x + 2, y - 6, 1, 1, 'h');
            c.rect(x + 9, y - 6, 1, 1, 'z');
            if ms % 17000 < 5000 {
                steam(c, x + 6, y - 4, ms / 2);
            }
        }
    }
}

fn shelves(c: &mut Canvas<'_>, w: i32, counter: i32, ms: u64) {
    let stacked =
        CafeRack::for_area(Rect::new(0, 0, w as u16, (c.height / 2) as u16)).filter(|r| r.stacked);
    let x = if w >= 110 { w * 6 / 100 } else { 2 };
    let width = if stacked.is_some() {
        (w * 58 / 100 - x - 2).max(14)
    } else {
        (w * 25 / 100).max(16)
    };
    let top = (counter * 30 / 100).max(8);
    // Two unevenly stocked shelves, with room for taller brewing equipment.
    let bottles = if stacked.is_some() {
        (counter * 43 / 100).max(stacked.map_or(0, |r| i32::from(r.area.bottom()) * 2 + 9))
    } else {
        top + (counter / 10).max(2)
    };
    let mugs = if stacked.is_some() {
        (counter * 61 / 100).max(bottles + 10)
    } else {
        (top + (counter / 4).max(10)).max(bottles + 8)
    };
    for (row, y) in [bottles, mugs].into_iter().enumerate() {
        c.rect(x, y, width, 2, 't');
        c.rect(x, y, width, 1, 'm');
        let inventory = if row == 0 {
            [
                CoffeeTool::Beans,
                CoffeeTool::Grinder,
                CoffeeTool::Kettle,
                CoffeeTool::Syrup,
                CoffeeTool::Dripper,
                CoffeeTool::Cups,
            ]
        } else {
            [
                CoffeeTool::Espresso,
                CoffeeTool::Cups,
                CoffeeTool::Scale,
                CoffeeTool::Dripper,
                CoffeeTool::Beans,
                CoffeeTool::Kettle,
            ]
        };
        let mut jx = x + 2;
        let mut item = 0;
        while jx < x + width - 3 {
            let tool = inventory[item % inventory.len()];
            if jx + tool.width() >= x + width {
                break;
            }
            coffee_tool(c, tool, jx, y, ms + item as u64 * 731);
            jx += tool.width() + if item % 3 == 1 { 3 } else { 2 };
            item += 1;
        }
    }
}

fn render_cafe(
    frame: &mut Frame<'_>,
    layout: &SceneLayout,
    pack: &Pack,
    accounts: &[&SceneAccount],
    connection: &str,
    ms: u64,
    all_limits: bool,
) {
    let area = layout.stage;
    let capacity = layout.capacity;
    if area.is_empty() {
        return;
    }
    let w = area.width as usize;
    let h = area.height as usize * 2;
    let mut c = Canvas::new(w, h, pack);
    c.rect(0, 0, w as i32, h as i32, 'w');
    let (w, h) = (w as i32, h as i32);
    let status = match connection {
        "CONNECTED" => 'h',
        "AUTH" => 'a',
        "STALE" => 'a',
        _ => 'c',
    };
    if w < 54 || h < 24 {
        // Tiny terminals still get a graphic-only, animated connection beacon.
        c.sprite("cat_light", (w - 12) / 2, (h - 14) / 2, ms, Some(status));
        c.paint(frame, area);
        return;
    }
    let counter = cafe_counter(h);
    let floor = h * 92 / 100;
    c.rect(0, 0, w, h, 'w');
    // Quiet wood-grain texture and a warm radial pool around the pendant lamp.
    for y in 1..counter {
        for x in 2..w - 2 {
            if (x * 17 + y * 31) % 41 == 0 {
                let index = y as usize * c.width + x as usize;
                c.pixels[index] = mix(pack.color('w'), pack.color('i'), 18);
            }
        }
    }
    let lamp_x = w * 88 / 100;
    let lamp_y = lamp_height(counter);
    c.glow(lamp_x, lamp_y, (w / 5).clamp(12, 40), 'o', 24);
    c.rect(lamp_x, 1, 1, (lamp_y - 1).max(1), 'r');
    c.rect(lamp_x - 1, lamp_y, 3, 2, 'z');
    c.rect(lamp_x, lamp_y, 1, 1, 'j');

    rainy_window(
        &mut c,
        w * 58 / 100,
        4,
        (w * 20 / 100).max(16),
        counter - 7,
        ms,
    );
    shelves(&mut c, w, counter, ms);
    if w >= 110 {
        let sx = w * 58 / 100 - 17;
        let sy = if CafeRack::for_area(area).is_some_and(|r| r.stacked) {
            5
        } else {
            13
        };
        c.glow(sx + 8, sy + 8, 14, 'z', 26);
        c.rect(sx - 1, sy - 1, 17, 17, 'r');
        c.rect(sx, sy, 15, 15, 'z');
        c.rect(sx + 1, sy + 1, 13, 13, 'n');
        c.sprite("cat_sign", sx + 1, sy + 1, ms, None);
    }

    // Single long cafe counter, kickboard, floorboards and bar stools.
    c.rect(0, counter, w, floor - counter, 'r');
    c.rect(0, counter, w, 3, 'm');
    c.rect(0, counter, w, 1, 'o');
    c.rect(0, counter + 3, w, 1, 't');
    c.rect(0, floor, w, h - floor, 'f');
    c.rect(0, floor, w, 1, 't');
    for x in (6..w - 2).step_by(17) {
        c.rect(x, floor + 1, 1, h - floor - 3, 'g');
    }
    let stools = ((w - 12) / 26).clamp(2, 7);
    let stool_y = counter + (floor - counter) / 2;
    for index in 0..stools {
        let x = (index + 1) * (w - 8) / (stools + 1);
        c.rect(x - 4, stool_y, 10, 2, 'i');
        c.rect(x - 3, stool_y + 2, 8, 1, 't');
        c.rect(x, stool_y + 3, 2, h - stool_y - 7, 't');
        c.rect(x - 3, h - 5, 8, 1, 'r');
    }

    c.sprite("plant", 4, counter - 13, ms, None);
    if w >= 110 {
        c.sprite("pastry", w - 21, counter - 16, ms, None);
        c.sprite("plant", w - 33, counter - 13, ms + 500, None);
        c.sprite("terminal", w - 53, counter - 11, ms, Some(status));
    } else {
        c.rect(w - 10, counter - 4, 5, 3, 'i');
        c.rect(w - 9, counter - 3, 3, 1, status);
    }
    if connection == "CONNECTING" {
        c.rect(w - 13 + ((ms / 180) % 5) as i32, counter + 1, 1, 1, 'c');
    }
    let mug_x = if w >= 110 { w - 62 } else { w - 21 };
    c.sprite("mug", mug_x, counter - 6, ms, None);
    steam(&mut c, mug_x, counter - 6, ms);

    let visible = accounts;
    let residents = if accounts.is_empty() {
        capacity.min(2)
    } else {
        visible.len()
    };
    for index in 0..residents {
        let card = layout.cards[index];
        let x = i32::from(card.x - area.x + card.width / 2) - 6;
        let account = visible.get(index).copied();
        c.cafe_cat(
            index,
            account,
            x,
            counter,
            ms,
            actor_motion(layout, index, account, ms, all_limits),
        );
    }
    c.paint(frame, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    // Tests may embed artwork; release binaries must load the optional file.
    const SOURCE: &[u8] = include_bytes!("plugins/pixel-cafe.json");
    const OFFICE_SOURCE: &[u8] = include_bytes!("plugins/pixel-office.json");
    const STATION_SOURCE: &[u8] = include_bytes!("plugins/pixel-station.json");

    fn theme_source(theme: Theme) -> &'static [u8] {
        match theme {
            Theme::Cafe => SOURCE,
            Theme::Office => OFFICE_SOURCE,
            Theme::Station => STATION_SOURCE,
        }
    }

    fn sample_system() -> super::super::system_metrics::Snapshot {
        super::super::system_metrics::Snapshot {
            cpu: Some(42.0),
            gpu: Some(30.0),
            memory: Some((8 << 30, 16 << 30)),
            disk: Some((128 << 30, 512 << 30)),
            vram: Some((2 << 30, 8 << 30)),
            network: Some((1024.0, 2048.0)),
            disk_io: Some((4096.0, 8192.0)),
            interface: "eth0".into(),
            sampled_at: Some(Instant::now()),
            ..Default::default()
        }
    }

    #[test]
    fn page_capacity_doubles_with_sprite_width_safety_and_preserves_real_counts() {
        for (width, cafe, office) in [(80, 4, 8), (120, 8, 16), (180, 12, 24)] {
            for (theme, expected) in [
                (Theme::Cafe, cafe),
                (Theme::Office, office),
                (Theme::Station, cafe),
            ] {
                let layout = SceneLayout::new(Rect::new(0, 0, width, 42), theme);
                assert_eq!(layout.capacity, expected);
                assert!(layout.cards.iter().all(|card| card.width >= 12));
            }
        }
        assert_eq!(
            SceneLayout::new(Rect::new(0, 0, 20, 18), Theme::Cafe).capacity,
            1
        );
        let one = example_account();
        assert_eq!(
            SceneLayout::for_accounts(Rect::new(0, 0, 180, 42), Theme::Cafe, &[&one], false)
                .capacity,
            1
        );
    }

    #[test]
    fn focused_bubble_borrows_bounded_space_without_changing_pages_or_crossing_bubbles() {
        let accounts = sample_accounts(40);
        let refs = accounts.iter().collect::<Vec<_>>();
        for theme in Theme::ALL {
            for (width, height) in [(20, 8), (80, 24), (120, 40), (180, 42), (240, 76)] {
                let mut layout =
                    SceneLayout::for_accounts(Rect::new(2, 3, width, height), theme, &refs, true);
                let base = layout.cards.clone();
                let visible = &refs[..layout.capacity.min(refs.len())];
                for selected in 0..visible.len() {
                    layout.cards = base.clone();
                    let target = layout.focused_cards(visible, selected, true);
                    assert_eq!(target.len(), base.len());
                    assert!(target[selected].width >= base[selected].width);
                    for (old, new) in base.iter().zip(&target) {
                        assert_eq!((old.y, old.height), (new.y, new.height));
                        assert!(
                            (i32::from(old.x + old.width / 2) - i32::from(new.x + new.width / 2))
                                .abs()
                                <= 12
                        );
                    }
                    if width == 180 {
                        assert!(
                            target[selected].width >= 21,
                            "{theme:?} selected {selected}: {:?}",
                            target[selected]
                        );
                    }
                    for elapsed in [0, 70, 180, 290, 360] {
                        layout.cards = interpolate_cards(&base, &target, elapsed);
                        for ms in [0, 1600, 2700, 16000, 37000] {
                            let bubbles = visible
                                .iter()
                                .enumerate()
                                .map(|(index, account)| {
                                    speech_bubble(&layout, index, account, theme, ms, true).0
                                })
                                .collect::<Vec<_>>();
                            for (index, bubble) in bubbles.iter().enumerate() {
                                assert!(
                                    bubble.x >= layout.area.x
                                        && bubble.right() <= layout.area.right()
                                );
                                for other in &bubbles[index + 1..] {
                                    assert!(bubble.intersection(*other).is_empty(), "{theme:?} {width}x{height}, selected {selected}, easing {elapsed}, time {ms}: {bubble:?} {other:?}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn reduced_motion_focus_snaps_and_bubble_toggle_restores_seating() {
        let mut animation = Animation::new();
        animation.theme = Theme::Cafe;
        animation.scene = sample_accounts(25);
        animation.order = (0..25).collect();
        animation.paused = true;
        animation.paused_at = Some(Instant::now());
        animation.resize(180, 42);
        animation.move_cat(5);
        assert_eq!(animation.focus_cards(), animation.focus_to);
        assert!(animation.focus_to[5].width > animation.layout().cards[5].width);
        animation.toggle_bubbles();
        assert_eq!(animation.focus_cards(), animation.layout().cards);
        animation.toggle_bubbles();
        animation.turn_page(1);
        assert_eq!(animation.page, 1);
        assert_eq!(animation.focus_cards(), animation.focus_to);
    }

    #[test]
    fn cat_actions_rest_between_moves_and_jump_once_with_smooth_lift_and_landing() {
        let mut jumps = 0;
        for epoch in 0..80 {
            let start = epoch * 18000;
            assert_eq!(cat_activity(0, 'h', start + 100), CatActivity::Watch);
            assert_eq!(cat_motion(0, 'h', start + 100).1, 0);
            if cat_activity(0, 'h', start + 2000) != CatActivity::Jump {
                continue;
            }
            jumps += 1;
            let heights = (0..2400)
                .step_by(50)
                .map(|age| cat_motion(0, 'h', start + 1200 + age).1)
                .collect::<Vec<_>>();
            assert_eq!((heights[0], *heights.last().unwrap()), (0, 0));
            assert_eq!(*heights.iter().max().unwrap(), 3);
            assert!(heights
                .windows(2)
                .all(|pair| (pair[0] - pair[1]).abs() <= 1));
            assert_eq!(cat_head_height(0, 'h', start + 1550), 8);
        }
        assert!(jumps > 0);
        for ms in (0..18000).step_by(100) {
            assert_eq!(cat_motion(0, 'd', ms).0, 0, "Sleeping cats must not glide");
            assert_eq!(cat_motion(0, 'd', ms).1, 0);
        }
    }

    #[test]
    fn articulated_cat_rig_stays_grounded_inside_its_lane_in_both_themes() {
        for source in [SOURCE, OFFICE_SOURCE, STATION_SOURCE] {
            let pack = Pack::parse(source).unwrap();
            let mut account = example_account();
            for status in ['h', 'c', 'd', 'a'] {
                account.status = status;
                for ms in (0..180000).step_by(223) {
                    let pose = cat_pose(0, status, ms);
                    assert!((0..=3).contains(&pose.head_drop));
                    assert!((0.0..=1.0).contains(&pose.breath));
                    let mut c = Canvas::new(40, 40, &pack);
                    c.cafe_cat(0, Some(&account), 10, 30, ms, (0, 0, ms));
                    let points = c
                        .pixels
                        .iter()
                        .enumerate()
                        .filter(|(_, color)| **color != pack.color('b'))
                        .map(|(i, _)| (i % 40, i / 40))
                        .collect::<Vec<_>>();
                    assert!(
                        points
                            .iter()
                            .all(|(x, y)| (10..=22).contains(x) && (19..30).contains(y)),
                        "{:?} {status} {ms}: {points:?}",
                        pack.theme
                    );
                    assert!(
                        points.iter().any(|(_, y)| *y == 29),
                        "Feet must touch the counter"
                    );
                }
            }
        }
        // Sleep entry/exit moves the head one logical pixel at a time.
        for epoch in 0..40 {
            let start = epoch * 18000;
            if cat_activity(0, 'h', start + 2400) != CatActivity::Sleep {
                continue;
            }
            let heights = (0..18000)
                .step_by(50)
                .map(|t| cat_head_height(0, 'h', start + t))
                .collect::<Vec<_>>();
            assert!(heights.windows(2).all(|p| (p[0] - p[1]).abs() <= 1));
        }
    }

    #[test]
    fn intel_gpu_readings_always_identify_their_scope_and_never_invent_vram() {
        use super::super::system_metrics::GpuSource;
        for source in [SOURCE, OFFICE_SOURCE, STATION_SOURCE] {
            let pack = Pack::parse(source).unwrap();
            for (width, height) in [(80, 24), (180, 42)] {
                for (gpu_source, expected, reading) in [
                    (GpuSource::IntelClients, "readable clients", "GPU 43%*"),
                    (GpuSource::IntelPmu, "system-wide", "GPU 43%"),
                ] {
                    let sample = super::super::system_metrics::Snapshot {
                        gpu: Some(43.0),
                        gpu_source,
                        sampled_at: Some(Instant::now()),
                        ..Default::default()
                    };
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| render_system(frame, frame.area(), &pack, &sample, 1000))
                        .unwrap();
                    let screen = text(terminal.backend().buffer());
                    assert!(screen.contains(reading), "{width} {height}: {screen}");
                    // Compact café uses a shared GPU* legend; other layouts
                    // retain their per-unit source label.
                    if gpu_source == GpuSource::IntelClients
                        || !pack.theme.on_platform()
                        || width >= 120
                    {
                        assert!(screen.contains(expected), "missing {expected}: {screen}");
                    }
                    assert!(!screen.contains("VRAM 0"));
                }
            }
        }
    }

    #[test]
    fn coffee_tools_have_distinct_silhouettes_and_bounded_decorative_animation() {
        let pack = Pack::parse(SOURCE).unwrap();
        let mut silhouettes = HashSet::new();
        for tool in [
            CoffeeTool::Beans,
            CoffeeTool::Grinder,
            CoffeeTool::Kettle,
            CoffeeTool::Dripper,
            CoffeeTool::Cups,
            CoffeeTool::Scale,
            CoffeeTool::Syrup,
            CoffeeTool::Espresso,
        ] {
            let mut c = Canvas::new(24, 30, &pack);
            coffee_tool(&mut c, tool, 3, 25, 700);
            silhouettes.insert(format!("{:?}", c.pixels));
            for (index, color) in c
                .pixels
                .iter()
                .enumerate()
                .filter(|(_, color)| **color != pack.color('b'))
            {
                assert!(
                    (3..3 + tool.width() as usize).contains(&(index % 24)),
                    "{tool:?}"
                );
                assert!(index / 24 < 25);
                assert_ne!(*color, Color::Reset);
            }
        }
        assert_eq!(silhouettes.len(), 8);
    }

    #[test]
    fn whole_bubbles_share_cat_motion_and_follow_lower_rolling_and_sleeping_heads() {
        let account = example_account();
        let layout =
            SceneLayout::for_accounts(Rect::new(2, 3, 180, 56), Theme::Cafe, &[&account], false);
        let lane = layout.cards[0];
        let mut positions = HashSet::new();
        for ms in (0..150000).step_by(211) {
            let (dx, bob, _) = actor_motion(&layout, 0, Some(&account), ms, false);
            let (bubble, tail) = speech_bubble(&layout, 0, &account, Theme::Cafe, ms, false);
            let expected_x = i32::from(lane.x + (lane.width - bubble.width) / 2) + dx;
            assert_eq!(i32::from(bubble.x), expected_x);
            assert_eq!(i32::from(tail), i32::from(lane.x + lane.width / 2) + dx);
            let head = layout.stage.y
                + ((cafe_counter(i32::from(layout.stage.height) * 2)
                    - cat_head_height(0, account.status, ms)
                    - bob)
                    / 2) as u16;
            assert_eq!(bubble.bottom() + 1, head);
            positions.insert((bubble.x, bubble.y));
        }
        assert!(
            positions.len() > 6,
            "The body, not just the tail, must move"
        );
    }

    #[test]
    fn dense_moving_bubbles_never_cross_neighboring_bubbles() {
        let accounts = sample_accounts(40);
        let refs = accounts.iter().collect::<Vec<_>>();
        for theme in Theme::ALL {
            for (width, height) in [(80, 24), (120, 40), (180, 42), (240, 76)] {
                let layout =
                    SceneLayout::for_accounts(Rect::new(2, 3, width, height), theme, &refs, true);
                for ms in (0..60000).step_by(499) {
                    let bubbles = refs
                        .iter()
                        .take(layout.capacity)
                        .enumerate()
                        .map(|(index, account)| {
                            speech_bubble(&layout, index, account, theme, ms, true).0
                        })
                        .collect::<Vec<_>>();
                    for (index, bubble) in bubbles.iter().enumerate() {
                        for other in &bubbles[index + 1..] {
                            assert!(
                                bubble.intersection(*other).is_empty(),
                                "{theme:?} {width}x{height} at {ms}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rolling_is_grounded_side_and_belly_poses_not_a_rotating_sprite() {
        let pack = Pack::parse(SOURCE).unwrap();
        let mut poses = HashSet::new();
        for phase in (0..3600).step_by(150) {
            let mut c = Canvas::new(36, 32, &pack);
            c.rolling_cat("cat", 10, 24, phase, 'h', Some(provider_color("Codex")));
            let points = c
                .pixels
                .iter()
                .enumerate()
                .filter(|(_, color)| **color != pack.color('b'))
                .map(|(i, _)| ((i % 36) as i32, (i / 36) as i32))
                .collect::<Vec<_>>();
            let min_x = points.iter().map(|p| p.0).min().unwrap();
            let max_x = points.iter().map(|p| p.0).max().unwrap();
            let min_y = points.iter().map(|p| p.1).min().unwrap();
            let max_y = points.iter().map(|p| p.1).max().unwrap();
            assert_eq!(max_y, 23, "Every pose remains grounded");
            assert!(max_x - min_x >= max_y - min_y);
            assert!(min_x >= 10 && max_x <= 22);
            if roll_pose(phase) >= 2 {
                assert!(min_y >= 16);
            }
            poses.insert(roll_pose(phase));
        }
        assert_eq!(poses.len(), 4);
    }

    #[test]
    fn neither_theme_draws_humans_and_cafe_floor_stays_empty() {
        let accounts = sample_accounts(25);
        let refs = accounts.iter().collect::<Vec<_>>();
        let area = Rect::new(0, 0, 180, 42);
        let pack = Pack::parse(SOURCE).unwrap();
        let layout = SceneLayout::for_accounts(area, Theme::Cafe, &refs, false);
        let draw = |ms| {
            let mut terminal = Terminal::new(TestBackend::new(180, 42)).unwrap();
            terminal
                .draw(|frame| {
                    render_cafe(
                        frame,
                        &layout,
                        &pack,
                        &refs[..layout.capacity],
                        "CONNECTED",
                        ms,
                        false,
                    )
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let before = draw(0);
        let later = draw(20000);
        for y in (cafe_counter(84) / 2 + 1) as u16..42 {
            for x in 0..180 {
                assert_eq!(before[(x, y)], later[(x, y)]);
            }
        }
        let mut office = Pack::parse(OFFICE_SOURCE).unwrap();
        office.sprites.remove("agent");
        let layout = SceneLayout::for_accounts(area, Theme::Office, &refs, false);
        let mut terminal = Terminal::new(TestBackend::new(180, 42)).unwrap();
        terminal
            .draw(|frame| {
                render_office(
                    frame,
                    &layout,
                    &office,
                    &refs[..layout.capacity],
                    700,
                    false,
                )
            })
            .unwrap();
    }

    #[test]
    fn machine_bars_keep_fixed_scale_missing_samples_and_alignment() {
        let mut terminal = Terminal::new(TestBackend::new(5, 2)).unwrap();
        terminal
            .draw(|frame| {
                bar_chart(
                    frame,
                    frame.area(),
                    &[Some(0.0), Some(50.0), None, Some(100.0)],
                    100.0,
                    Color::Green,
                    Color::Black,
                )
            })
            .unwrap();
        let b = terminal.backend().buffer();
        for x in [0, 1, 3] {
            assert_eq!(b[(x, 0)].symbol(), " ");
            assert_eq!(b[(x, 1)].symbol(), " ");
        }
        assert_eq!(b[(2, 0)].symbol(), " ");
        assert_eq!(b[(2, 1)].symbol(), "█");
        assert_eq!(b[(4, 0)].symbol(), "█");
        assert_eq!(b[(4, 1)].symbol(), "█");
    }

    #[test]
    fn cat_moods_keep_status_truthful_and_all_poses_fit_above_the_footer() {
        for ms in (0..200000).step_by(233) {
            assert_eq!(cat_activity(0, 'd', ms), CatActivity::Sleep);
            assert_eq!(cat_activity(0, 'c', ms), CatActivity::Burnout);
            assert_eq!(cat_activity(0, 'a', ms), CatActivity::Angry);
        }
        let mut poses = HashSet::new();
        let pack = Pack::parse(SOURCE).unwrap();
        let account = example_account();
        for height in [20, 24, 40, 42, 56, 90] {
            let area = Rect::new(0, 0, 180, height);
            let counter = cafe_counter(i32::from(height) * 2);
            for ms in (0..200000).step_by(233) {
                for index in 0..6 {
                    poses.insert(format!("{:?}", cat_activity(index, 'h', ms)));
                }
                let mut c = Canvas::new(180, height as usize * 2, &pack);
                c.cafe_cat(
                    0,
                    Some(&account),
                    40,
                    counter,
                    ms,
                    cat_motion_for_height(0, account.status, ms, height),
                );
                let footer = footer_area(area);
                assert!(c.pixels[footer.y as usize * 2 * 180..]
                    .iter()
                    .all(|p| *p == pack.color('b')));
            }
        }
        assert_eq!(poses.len(), 9);
    }

    #[test]
    fn tv_is_below_the_short_lamp_and_details_never_exceed_three_rows() {
        let mut accounts = sample_accounts(25);
        for account in &mut accounts {
            account.limits.extend([
                ("Review 5h".into(), Some(12.0)),
                ("Review Weekly".into(), Some(8.0)),
            ]);
        }
        let refs = accounts.iter().collect::<Vec<_>>();
        for height in 40..100 {
            let area = Rect::new(2, 3, 180, height);
            let tv = tv_area(area, Theme::Cafe).unwrap();
            let counter = cafe_counter(i32::from(height) * 2);
            assert!(tv.y >= area.y + ((lamp_height(counter) + 2) as u16).div_ceil(2) + 2);
            assert!(footer_area(area).y > area.y + (counter + 2) as u16 / 2);
            let layout = SceneLayout::for_accounts(area, Theme::Cafe, &refs, true);
            let machine = CafeRack::for_area(area).unwrap().area;
            for ms in (0..150000).step_by(500) {
                for (index, account) in refs.iter().take(layout.capacity).enumerate() {
                    let bubble = speech_bubble(&layout, index, account, Theme::Cafe, ms, true).0;
                    assert!(tv.intersection(bubble).is_empty(), "TV {height} {ms}");
                    assert!(
                        machine.intersection(bubble).is_empty(),
                        "Machine {height} {ms}"
                    );
                }
            }
        }
        for height in 0..100 {
            assert!(footer_area(Rect::new(0, 0, 180, height)).height <= 4);
        }
    }

    #[test]
    fn focused_footer_packs_live_account_data_but_never_credential_contents() {
        use super::super::{
            btop_style, draw_animated_overview_context, focused_account_footer, pack_footer_fields,
            AccountRow, GatewayClient, ModelRow, QuotaRow, TuiApp,
        };
        let mut app =
            TuiApp::new(GatewayClient::new("http://127.0.0.1:1".into()).unwrap()).unwrap();
        app.animation.pack = Some(Pack::parse(SOURCE).unwrap());
        app.data.accounts = vec![AccountRow {
            provider: "codex".into(),
            provider_label: "Codex".into(),
            key: "one".into(),
            label: "Never show account name".into(),
            enabled: true,
            requests: 1234,
            errors: 3,
            total_tokens: 99000,
            last_success_at: "2026-11-16T00:00:00Z".into(),
            ..Default::default()
        }];
        app.data.quotas = vec![QuotaRow {
            provider: "codex".into(),
            account_key: "one".into(),
            limit_label: "Code Gen 5h".into(),
            used_percent_value: Some(7.0),
            reset: "resets in 1h 23m".into(),
            ..Default::default()
        }];
        app.data.snapshot = json!({"generated_at":"2026-11-16T01:00:00Z", "quotas":{"codex":{"accounts":[{
            "key":"one", "email":"person@example.com", "plan_type":"plus",
            "access_token":"SECRET_ACCESS_TOKEN", "refresh_token":"SECRET_REFRESH_TOKEN",
            "rate_limit_reset_credits":{"available_count":1}, "models":[{"id":"model-one"}]
        }]}}});
        app.animation.observe(&app.data);
        app.animation.set_view(vec![0], 0, false, false, 0);
        let lines = focused_account_footer(&app, 180);
        let text = lines.join("\n");
        for expected in [
            "req",
            "tokens",
            "errors",
            "Status Enabled",
            "5h 7%",
            "1h23m",
            "Plan plus",
            "Reset credits 1",
            "Snapshot",
            "Last OK",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!text.contains("SECRET"));
        assert!(!text.contains("Never show account name"));
        for width in [1, 12, 40, 80, 180] {
            assert!(focused_account_footer(&app, width)
                .iter()
                .all(|line| Line::from(line.as_str()).width() <= width as usize));
        }
        for source in [SOURCE, OFFICE_SOURCE, STATION_SOURCE] {
            let pack = Pack::parse(source).unwrap();
            for (width, height) in [(32, 12), (80, 24), (180, 42), (180, 76)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        render(
                            frame,
                            &pack,
                            &[&app.animation.scene[0]],
                            "CONNECTED",
                            700,
                            0,
                            0,
                            false,
                            None,
                            false,
                            true,
                            &super::super::system_metrics::Snapshot::default(),
                            None,
                            &Dashboard::default(),
                        );
                        draw_animated_overview_context(frame, &app);
                    })
                    .unwrap();
                let footer = footer_area(Rect::new(0, 0, width, height));
                assert_eq!(footer.height - 1, 3);
                let buffer = terminal.backend().buffer();
                for x in 0..width {
                    for y in footer.y..footer.bottom() - 1 {
                        assert_eq!(buffer[(x, y)].bg, btop_style().bg.unwrap());
                    }
                    assert_eq!(buffer[(x, footer.bottom() - 1)].bg, pack.color('r'));
                }
            }
        }
        let unicode = pack_footer_fields(&["日本語 😀🙂 café\n\u{1b} extra".into()], 12);
        assert!(unicode
            .iter()
            .all(|line| Line::from(line.as_str()).width() <= 12 && !line.contains('\u{1b}')));
        app.data.models = vec![ModelRow {
            id: "custom-one".into(),
            enabled: true,
            ..Default::default()
        }];
        app.animation.first();
        let models = focused_account_footer(&app, 80).join("\n");
        assert!(models.contains("custom-one on"));
        assert!(!models.contains("99K"));
    }

    #[test]
    fn provider_fur_is_stable_across_status_and_collar_remains_independent() {
        let pack = Pack::parse(SOURCE).unwrap();
        let draw = |provider: &str, status| {
            let mut c = Canvas::new(16, 16, &pack);
            c.colored_sprite("cat", 0, 0, 0, Some(status), Some(provider_color(provider)));
            c.pixels
        };
        let codex = draw("Codex", 'h');
        let disabled = draw("Codex", 'd');
        let claude = draw("Claude", 'h');
        assert_eq!(codex[2], disabled[2]);
        assert_ne!(codex[2], claude[2]);
        assert_eq!(codex[5 * 16 + 4], pack.color('h'));
        assert_eq!(disabled[5 * 16 + 4], pack.color('d'));
        assert_ne!(provider_color("Antigravity"), provider_color("Gemini"));
    }

    #[test]
    fn chart_is_a_connected_fixed_scale_line_with_honest_gaps_and_no_art_residue() {
        let draw = |values: &[Option<f64>]| {
            let mut terminal = Terminal::new(TestBackend::new(4, 2)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new("xxxxxxxx\nxxxxxxxx"), frame.area());
                    line_chart(
                        frame,
                        frame.area(),
                        values,
                        100.0,
                        Color::Green,
                        Color::Black,
                    );
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        assert!(text(&draw(&[None; 8])).trim().is_empty());
        let line = draw(&[Some(0.0), Some(100.0)]);
        assert!(!text(&line).contains('x'));
        assert!(line[(3, 0)]
            .symbol()
            .chars()
            .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)));
        let gap = draw(&[Some(100.0), None, None, None, None, None, None, Some(0.0)]);
        assert_eq!(gap[(1, 0)].symbol(), " ");
        assert_eq!(gap[(2, 1)].symbol(), " ");
        assert_eq!(draw(&[Some(f64::NAN)]), draw(&[None]));
    }

    #[test]
    fn bubbles_have_convex_filled_corners_with_the_room_outside_not_inside() {
        let pack = Pack::parse(SOURCE).unwrap();
        let cats = sample_accounts(3);
        let refs = cats.iter().collect::<Vec<_>>();
        for (width, height) in [(80, 24), (120, 40), (180, 42)] {
            let draw = |bubbles| {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        render(
                            frame,
                            &pack,
                            &refs,
                            "CONNECTED",
                            700,
                            0,
                            0,
                            false,
                            None,
                            false,
                            bubbles,
                            &sample_system(),
                            None,
                            &Dashboard::default(),
                        )
                    })
                    .unwrap();
                terminal.backend().buffer().clone()
            };
            let room = draw(false);
            let filled = draw(true);
            let layout = SceneLayout::for_accounts(room.area, Theme::Cafe, &refs, false);
            let (card, _) = speech_bubble(&layout, 0, &cats[0], Theme::Cafe, 700, false);
            let fill = filled[(card.x + 1, card.y + 1)].bg;
            for (x, y, symbol) in [
                (card.x, card.y, "▗"),
                (card.right() - 1, card.y, "▖"),
                (card.x, card.bottom() - 1, "▝"),
                (card.right() - 1, card.bottom() - 1, "▘"),
            ] {
                assert_eq!(filled[(x, y)].symbol(), symbol);
                assert_eq!(filled[(x, y)].fg, fill);
                assert_eq!(
                    filled[(x, y)].bg,
                    if y == card.y {
                        room[(x, y)].fg
                    } else {
                        room[(x, y)].bg
                    }
                );
                assert_ne!(filled[(x, y)].bg, fill);
            }
        }
    }

    #[test]
    fn wall_tv_and_model_bubbles_fit_without_colliding_with_quota_bubbles() {
        let mut cats = sample_accounts(25);
        for cat in &mut cats {
            cat.credits = Some("8.25".into());
            cat.limits.extend([
                ("Review 5h".into(), Some(12.0)),
                ("Review Weekly".into(), Some(8.0)),
            ]);
        }
        let mut models = SceneAccount::models_cat();
        models.models = Some((0..9).map(|i| (format!("custom-{i}"), true)).collect());
        models.model_selected = 8;
        cats.insert(0, models);
        let refs = cats.iter().collect::<Vec<_>>();
        let dashboard = Dashboard {
            requests: Some(100),
            tokens: Some(1234),
            errors: Some(4),
            active: 2,
            accounts: 25,
            context: vec![Some(42.0)],
        };
        for (width, height) in [
            (70, 20),
            (80, 24),
            (110, 40),
            (120, 40),
            (180, 42),
            (300, 90),
        ] {
            let area = Rect::new(0, 0, width, height);
            let layout = SceneLayout::for_accounts(area, Theme::Cafe, &refs, true);
            let tv = tv_area(area, Theme::Cafe).unwrap();
            for ms in [0, 160, 700, 12000] {
                for (index, cat) in refs.iter().take(layout.capacity).enumerate() {
                    let (bubble, _) = speech_bubble(&layout, index, cat, Theme::Cafe, ms, true);
                    assert!(tv.intersection(bubble).is_empty(), "{width}×{height}");
                }
            }
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let pack = Pack::parse(SOURCE).unwrap();
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        &pack,
                        &refs,
                        "CONNECTED",
                        700,
                        0,
                        0,
                        false,
                        None,
                        true,
                        true,
                        &sample_system(),
                        None,
                        &dashboard,
                    )
                })
                .unwrap();
            let screen = text(terminal.backend().buffer());
            for label in [
                "Requests",
                "Tokens",
                "Accounts",
                "Error Rate",
                "Context Usage 24h",
            ] {
                assert!(screen.contains(label), "{width}: {label}");
            }
            assert!(
                screen.contains("custom-8"),
                "selected models must scroll into the bubble"
            );
            if width >= 120 {
                let centers = layout
                    .cards
                    .iter()
                    .map(|r| r.x + r.width / 2)
                    .collect::<Vec<_>>();
                let gaps = centers
                    .windows(2)
                    .map(|p| p[1] - p[0])
                    .collect::<HashSet<_>>();
                assert!(gaps.len() > 1, "seats must not form an equal-spaced grid");
            }
        }
    }

    #[test]
    fn model_cat_navigation_never_targets_a_credential_and_normal_sections_still_work() {
        use super::super::{AccountRow, GatewayClient, ModelRow, OverviewSection, TuiApp};
        let mut app =
            TuiApp::new(GatewayClient::new("http://127.0.0.1:1".into()).unwrap()).unwrap();
        app.hidden_usage_accounts.clear();
        app.hidden_usage_providers.clear();
        app.data.accounts.push(AccountRow {
            provider: "codex".into(),
            key: "a".into(),
            file_name: "a.json".into(),
            enabled: true,
            ..Default::default()
        });
        app.animation.active = true;
        app.animation.pack = Some(Pack::parse(SOURCE).unwrap());
        app.animation.observe(&app.data);
        app.sync_animated_accounts();
        app.animation.move_cat(-1);
        app.sync_from_animated_cat();
        assert!(app.overview_section == OverviewSection::CustomModels);
        assert!(app.animation.models_focused());
        app.open_selected_account_details();
        assert!(!app.show_account_details);
        app.data.models.push(ModelRow {
            alias: "model-one".into(),
            id: "model-one".into(),
            ..Default::default()
        });
        app.open_selected_overview_details();
        assert!(app.show_model_details);
        app.show_model_details = false;
        app.refresh_quota(false);
        assert!(app.message.contains("admin login required"));
        app.enable_selected();
        assert!(!app.network.command_pending());
        app.disable_selected();
        assert!(!app.network.command_pending());
        app.request_delete_confirmation();
        assert_eq!(
            app.confirmation_modal
                .as_ref()
                .map(|modal| modal.title.as_str()),
            Some("Delete Custom Model")
        );
        app.cancel_confirmation();
        app.animation.move_cat(1);
        app.sync_from_animated_cat();
        assert!(app.overview_section == OverviewSection::AccountUsage);
        assert_eq!(app.overview_account_selected, 0);
        app.open_selected_account_details();
        assert!(app.show_account_details);
        app.show_account_details = false;
        app.overview_section = OverviewSection::Metrics;
        app.sync_animated_accounts();
        assert!(app.overview_section == OverviewSection::AccountUsage);
        app.data.accounts.clear();
        app.animation.observe(&app.data);
        app.sync_animated_accounts();
        assert!(app.overview_section == OverviewSection::CustomModels);
        assert!(app.animation.models_focused());
        app.animation.exit();
        app.overview_section = OverviewSection::AccountUsage;
        app.previous_overview_section();
        assert!(app.overview_section == OverviewSection::CustomModels);
        app.previous_overview_section();
        assert!(app.overview_section == OverviewSection::ContextUsage);
    }

    #[test]
    fn reset_labels_are_real_countdowns_with_at_most_two_units() {
        let now = 1_800_000_000;
        for (raw, seconds, expected) in [
            ("resets in 1d 2h 30m", 95400, "1d2h"),
            ("in 1h23m", 4980, "1h23m"),
            ("22m", 1320, "22m"),
            ("reset now", 0, "0m"),
            ("45 seconds", 45, "<1m"),
            ("2 days 4 hours", 187200, "2d4h"),
        ] {
            let deadline = reset_deadline(raw, now);
            assert_eq!(deadline, Some(now + seconds), "{raw}");
            assert_eq!(reset_countdown(deadline, now), expected);
            assert_eq!(reset_countdown(deadline, now + seconds + 10), "0m");
        }
        for raw in [
            "",
            "Monthly",
            "weekly window",
            "unknown",
            "1m junk",
            "99999999999999999999d",
        ] {
            assert_eq!(reset_deadline(raw, now), None, "{raw}");
        }
        assert_eq!(reset_countdown(None, now), "—");
        assert_eq!(reset_deadline("1800004980", now), Some(now + 4980));
        assert_eq!(reset_deadline("1800004980000", now), Some(now + 4980));
        let stamp = chrono::DateTime::from_timestamp(now + 4980, 0)
            .unwrap()
            .to_rfc3339();
        assert_eq!(reset_deadline(&stamp, now), Some(now + 4980));
    }

    #[test]
    fn unrelated_observations_do_not_restart_reset_clocks() {
        let mut animation = Animation::new();
        animation.pack = Some(Pack::parse(SOURCE).unwrap());
        let mut data = GatewayData::default();
        data.accounts.push(super::super::AccountRow {
            provider: "codex".into(),
            key: "one".into(),
            ..Default::default()
        });
        data.quotas.push(super::super::QuotaRow {
            provider: "codex".into(),
            account_key: "one".into(),
            limit_label: "5h".into(),
            reset: "1h23m".into(),
            used_percent_value: Some(20.0),
            ..Default::default()
        });
        animation.observe(&data);
        let key = ("codex".into(), "one".into(), "5h".into());
        animation.resets.get_mut(&key).unwrap().1 = Some(12345);
        animation.observe(&data);
        assert_eq!(animation.scene[0].resets, vec![Some(12345)]);
        data.quotas[0].reset = "2h".into();
        animation.observe(&data);
        assert_ne!(animation.scene[0].resets, vec![Some(12345)]);
        animation.observe(&GatewayData::default());
        assert!(animation.resets.is_empty());
    }

    #[test]
    fn cafe_rack_is_mounted_above_bubbles_at_all_supported_sizes() {
        let mut accounts = sample_accounts(25);
        for account in &mut accounts {
            account.credits = Some("8.25".into());
            account.limits.extend([
                ("Review 5h".into(), Some(12.0)),
                ("Review Weekly".into(), Some(8.0)),
            ]);
        }
        for (width, height) in [
            (70, 20),
            (80, 24),
            (80, 42),
            (110, 40),
            (120, 40),
            (180, 42),
            (300, 90),
        ] {
            let area = Rect::new(2, 3, width, height);
            let rack = CafeRack::for_area(area).unwrap();
            assert_eq!(rack.stacked, width >= 120 && height >= 40);
            assert!(rack.area.x > area.x && rack.area.right() < area.right());
            assert!(rack.area.y > area.y && rack.area.bottom() < area.bottom() - 3);
            if rack.stacked {
                // The rack lives on the upper-left wall, not over the rain glass.
                assert!(rack.area.right() <= area.x + width * 53 / 100);
            }
            for all_limits in [false, true] {
                let layout = SceneLayout::for_accounts(
                    area,
                    Theme::Cafe,
                    &accounts.iter().collect::<Vec<_>>(),
                    all_limits,
                );
                for ms in [0, 100, 700, 10300, 12000] {
                    for (index, account) in accounts.iter().take(layout.capacity).enumerate() {
                        let bubble =
                            speech_bubble(&layout, index, account, Theme::Cafe, ms, all_limits).0;
                        assert!(
                            rack.area.intersection(bubble).is_empty(),
                            "{width}×{height} {all_limits}"
                        );
                    }
                }
            }
        }
        for (width, height) in [(1, 1), (20, 8), (38, 18), (69, 24)] {
            assert!(CafeRack::for_area(Rect::new(0, 0, width, height)).is_none());
        }
    }

    #[test]
    fn rack_motion_tracks_available_activity_and_unknown_or_idle_devices_stay_still() {
        let pack = Pack::parse(SOURCE).unwrap();
        for (width, height) in [(80, 24), (180, 42)] {
            let draw = |sample: &super::super::system_metrics::Snapshot, ms| {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| render_system(frame, frame.area(), &pack, sample, ms))
                    .unwrap();
                terminal.backend().buffer().clone()
            };
            let sample = sample_system();
            assert_ne!(draw(&sample, 0), draw(&sample, 300));
            assert_eq!(draw(&sample, 300), draw(&sample, 300)); // Pause / reduced motion.
            let unknown = super::super::system_metrics::Snapshot::default();
            let buffer = draw(&unknown, 0);
            assert_eq!(buffer, draw(&unknown, 1000));
            for name in ["CPU N/A", "GPU N/A", "RAM N/A", "DISK / N/A", "NET N/A"] {
                assert!(text(&buffer).contains(name), "{width}: {name}");
            }
            let idle = super::super::system_metrics::Snapshot {
                cpu: Some(0.0),
                gpu: Some(0.0),
                disk_io: Some((0.0, 0.0)),
                network: Some((0.0, 0.0)),
                ..sample.clone()
            };
            assert_eq!(draw(&idle, 0), draw(&idle, 1000));
            let mut stale = sample.clone();
            stale.sampled_at = Some(Instant::now() - Duration::from_secs(10));
            assert_eq!(draw(&stale, 0), draw(&unknown, 0));
        }
    }

    #[test]
    fn cafe_rack_keeps_live_readouts_inside_the_cabinet_and_office_is_unchanged() {
        let sample = sample_system();
        for (width, height) in [(80, 24), (180, 42)] {
            let pack = Pack::parse(SOURCE).unwrap();
            let rack = CafeRack::for_area(Rect::new(0, 0, width, height)).unwrap();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_system(frame, frame.area(), &pack, &sample, 700))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert!(text(buffer).contains("Machine"));
            for y in rack.area.y..rack.area.bottom() {
                for x in 0..width {
                    if !rack.area.contains((x, y).into()) {
                        assert_eq!(buffer[(x, y)].symbol(), " ");
                    }
                }
            }
        }
        let pack = Pack::parse(OFFICE_SOURCE).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(180, 42)).unwrap();
        terminal
            .draw(|frame| render_system(frame, frame.area(), &pack, &sample, 700))
            .unwrap();
        assert!(!text(terminal.backend().buffer()).contains("RACK"));
    }

    #[test]
    fn system_station_bubble_toggle_and_refresh_bot_render_without_padding() {
        let sample = super::super::system_metrics::Snapshot {
            cpu: Some(42.0),
            gpu: Some(30.0),
            memory: Some((8 << 30, 16 << 30)),
            disk: Some((128 << 30, 512 << 30)),
            network: Some((1024.0, 2048.0)),
            disk_io: Some((4096.0, 8192.0)),
            interface: "eth0".into(),
            sampled_at: Some(Instant::now()),
            ..Default::default()
        };
        let accounts = sample_accounts(2);
        for theme in Theme::ALL {
            let pack = Pack::parse(theme_source(theme)).unwrap();
            for (width, height) in [(80, 24), (180, 42), (300, 90)] {
                let area = Rect::new(0, 0, width, height);
                assert_eq!(
                    SceneLayout::for_accounts(
                        area,
                        theme,
                        &accounts.iter().collect::<Vec<_>>(),
                        false
                    )
                    .stage,
                    area
                );
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        render(
                            frame,
                            &pack,
                            &accounts.iter().collect::<Vec<_>>(),
                            "CONNECTED",
                            700,
                            0,
                            0,
                            false,
                            None,
                            false,
                            false,
                            &sample,
                            Some("ACCOUNT"),
                            &Dashboard::default(),
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let screen = text(buffer);
                for expected in [
                    "CPU 42%", "GPU 30%", "RAM", "DISK /", "NET", "ACCOUNT", "[o o]",
                ] {
                    assert!(screen.contains(expected), "{theme:?} {width}: {expected}");
                }
                // Identity in the footer is intentional; the speech card is gone.
                assert!(!buffer.content.iter().any(|cell| cell.symbol() == "╭"));
                for y in 7..footer_area(area).y.min(height - 6) {
                    for x in [0, width - 1] {
                        assert_eq!(buffer[(x, y)].symbol(), "▀");
                        assert_ne!(buffer[(x, y)].bg, pack.color('b'));
                    }
                }
                terminal
                    .draw(|frame| {
                        render(
                            frame,
                            &pack,
                            &accounts.iter().collect::<Vec<_>>(),
                            "CONNECTED",
                            700,
                            0,
                            0,
                            false,
                            None,
                            false,
                            true,
                            &sample,
                            None,
                            &Dashboard::default(),
                        )
                    })
                    .unwrap();
                assert!(terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .any(|cell| cell.symbol() == "▗"));
            }
        }
    }

    fn image(
        width: u16,
        height: u16,
        connection: &str,
        ms: u64,
        count: usize,
    ) -> ratatui::buffer::Buffer {
        image_theme(width, height, connection, ms, count, Theme::Cafe)
    }

    fn image_theme(
        width: u16,
        height: u16,
        connection: &str,
        ms: u64,
        count: usize,
        theme: Theme,
    ) -> ratatui::buffer::Buffer {
        let pack = Pack::parse(theme_source(theme)).unwrap();
        let accounts = sample_accounts(count);
        image_accounts(width, height, connection, ms, &pack, &accounts, false)
    }

    fn sample_accounts(count: usize) -> Vec<SceneAccount> {
        (0..count)
            .map(|i| SceneAccount {
                status: ['h', 'c', 'd', 'a'][i % 4],
                used: Some(30.0),
                email: format!("agent{i:02}@example.com"),
                name: format!("Account {i:02}"),
                provider: ["Codex", "Claude", "Gemini"][i % 3].into(),
                icon: ["◎", "✳", "✦"][i % 3],
                quota: "5h".into(),
                requests: 42,
                limits: vec![("5h".into(), Some(30.0)), ("Weekly".into(), Some(15.0))],
                resets: vec![
                    Some(chrono::Utc::now().timestamp() + 4980),
                    Some(chrono::Utc::now().timestamp() + 93600),
                ],
                credits: None,
                recent_until: Some(Instant::now() + Duration::from_secs(10)),
                models: None,
                model_selected: 0,
            })
            .collect()
    }

    fn image_accounts(
        width: u16,
        height: u16,
        connection: &str,
        ms: u64,
        pack: &Pack,
        accounts: &[SceneAccount],
        all_limits: bool,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    pack,
                    &accounts.iter().collect::<Vec<_>>(),
                    connection,
                    ms,
                    0,
                    0,
                    false,
                    None,
                    all_limits,
                    true,
                    &sample_system(),
                    None,
                    &Dashboard::default(),
                )
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    #[ignore = "writes a local animated contact sheet for motion review"]
    fn write_cat_motion_preview() {
        let pack = Pack::parse(SOURCE).unwrap();
        let account = example_account();
        let mut clips = Vec::new();
        for (name, activity) in [
            ("Quiet / slow blink", CatActivity::Watch),
            ("Settling into sleep", CatActivity::Sleep),
            ("Grounded roll", CatActivity::Roll),
            ("Anticipation / jump / landing", CatActivity::Jump),
            ("Relaxed tail", CatActivity::Wag),
            ("Playing", CatActivity::Play),
            ("Grooming", CatActivity::Groom),
            ("Stretching", CatActivity::Stretch),
        ] {
            let epoch = (0..200)
                .find(|epoch| cat_activity(0, 'h', epoch * 18000 + 2400) == activity)
                .unwrap();
            let frames = (0..100)
                .map(|frame| {
                    let ms = epoch * 18000 + 900 + frame * 50;
                    let mut canvas = Canvas::new(40, 36, &pack);
                    canvas.cafe_cat(0, Some(&account), 10, 30, ms, cat_motion(0, 'h', ms));
                    canvas
                        .pixels
                        .iter()
                        .enumerate()
                        .filter(|(_, color)| **color != pack.color('b'))
                        .map(|(i, color)| {
                            let color = match color {
                                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                                _ => "#ffffff".into(),
                            };
                            json!([i % 40, i / 40, color])
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            clips.push(json!({"name":name, "frames":frames}));
        }
        let mut html = String::from(
            r#"<!doctype html><meta charset=utf-8><title>iogw cat motion</title><style>body{background:#14100e;color:#e9dac5;font:14px monospace;padding:24px}main{display:grid;grid-template-columns:repeat(4,260px);gap:16px}figure{margin:0;background:#241b16;padding:10px}canvas{width:240px;height:216px;image-rendering:pixelated}button{margin-bottom:20px}</style><h1>iogw · articulated cat motion</h1><button>Pause</button><main></main><script>const clips="#,
        );
        html.push_str(&serde_json::to_string(&clips).unwrap());
        html.push_str(r#";const main=document.querySelector('main');for(const clip of clips){const fig=document.createElement('figure');const label=document.createElement('figcaption');label.textContent=clip.name;const c=document.createElement('canvas');c.width=240;c.height=216;fig.append(label,c);main.append(fig);clip.ctx=c.getContext('2d');clip.ctx.scale(6,6)}let tick=0,paused=false;document.querySelector('button').onclick=(event)=>{paused=!paused;event.target.textContent=paused?'Resume':'Pause'};function draw(){for(const clip of clips){const ctx=clip.ctx;ctx.fillStyle='#241b16';ctx.fillRect(0,0,40,36);ctx.fillStyle='#a86b3b';ctx.fillRect(0,30,40,1);for(const [x,y,color]of clip.frames[tick%clip.frames.length]){ctx.fillStyle=color;ctx.fillRect(x,y,1,1)}}}draw();setInterval(()=>{if(!paused){tick++;draw()}},50);</script>"#);
        fs::create_dir_all("target/iogw-previews").unwrap();
        fs::write("target/iogw-previews/cat-motion.html", html).unwrap();
    }

    #[test]
    #[ignore = "writes local HTML terminal previews for visual review"]
    fn write_visual_previews() {
        let directory = Path::new("target/iogw-previews");
        fs::create_dir_all(directory).unwrap();
        for (name, width, height, state, count, theme) in [
            ("cafe", 180, 42, "CONNECTED", 7, Theme::Cafe),
            ("cafe-motion", 180, 42, "CONNECTED", 18, Theme::Cafe),
            ("cafe-focused", 180, 42, "CONNECTED", 18, Theme::Cafe),
            ("cafe-large", 180, 56, "CONNECTED", 7, Theme::Cafe),
            ("compact-motion", 80, 24, "CONNECTED", 8, Theme::Cafe),
            ("compact-focused", 80, 24, "CONNECTED", 8, Theme::Cafe),
            ("compact", 80, 24, "CONNECTED", 4, Theme::Cafe),
            ("office", 180, 42, "CONNECTED", 25, Theme::Office),
            ("office-focused", 180, 42, "CONNECTED", 25, Theme::Office),
            ("office-compact", 80, 24, "CONNECTED", 4, Theme::Office),
            ("station", 180, 42, "CONNECTED", 7, Theme::Station),
            ("station-focused", 180, 42, "CONNECTED", 18, Theme::Station),
            ("station-motion", 180, 42, "CONNECTED", 7, Theme::Station),
            ("station-large", 240, 76, "CONNECTED", 7, Theme::Station),
            ("station-compact", 80, 24, "CONNECTED", 4, Theme::Station),
            ("station-empty", 100, 30, "CONNECTED", 0, Theme::Station),
            ("station-auth", 80, 24, "AUTH", 0, Theme::Station),
            ("station-tiny", 24, 10, "CONNECTING", 1, Theme::Station),
            (
                "station-all-limits",
                180,
                42,
                "CONNECTED",
                1,
                Theme::Station,
            ),
            ("empty", 100, 30, "CONNECTED", 0, Theme::Cafe),
            ("auth", 80, 24, "AUTH", 0, Theme::Office),
            ("tiny", 24, 10, "CONNECTING", 1, Theme::Cafe),
            ("long-email", 80, 24, "CONNECTED", 2, Theme::Cafe),
            ("name-fallback", 80, 24, "CONNECTED", 2, Theme::Cafe),
            ("all-limits", 80, 24, "CONNECTED", 1, Theme::Cafe),
            ("office-all-limits", 80, 24, "CONNECTED", 1, Theme::Office),
        ] {
            let ms = if name.contains("motion") { 16000 } else { 700 };
            let mut accounts = sample_accounts(count);
            if name == "long-email" {
                accounts[0].email = "a-rather-long-address@example.com".into();
            }
            if name == "name-fallback" {
                accounts[0].email.clear();
                accounts[0].name = "Personal account".into();
            }
            let all_limits = name.ends_with("all-limits");
            if all_limits {
                accounts[0].used = Some(95.5);
                accounts[0].quota = "Gen 5h".into();
                accounts[0].limits = vec![
                    ("Gen 5h".into(), Some(95.5)),
                    ("Gen Weekly".into(), Some(82.0)),
                    ("Review 5h".into(), Some(50.0)),
                    ("Review Weekly".into(), Some(22.0)),
                    ("GPT-5.4 5h".into(), Some(8.0)),
                ];
            }
            let pack = Pack::parse(theme_source(theme)).unwrap();
            let mut models_cat = SceneAccount::models_cat();
            models_cat.models = Some(vec![
                ("coding-fast".into(), true),
                ("daily-agent".into(), true),
                ("research".into(), false),
                ("long-context".into(), true),
                ("fallback".into(), true),
            ]);
            let actors = std::iter::once(&models_cat)
                .chain(accounts.iter())
                .collect::<Vec<_>>();
            let dashboard = Dashboard {
                requests: Some(19325),
                tokens: Some(1284000),
                errors: Some(29),
                active: count.saturating_sub(1),
                accounts: count,
                context: vec![
                    Some(30.0),
                    Some(4.0),
                    Some(72.0),
                    Some(95.0),
                    Some(35.0),
                    Some(60.0),
                    Some(5.0),
                    Some(18.0),
                ],
            };
            let mut system = sample_system();
            system.history = (0..120)
                .map(|i| {
                    [
                        Some((i * 7 % 85) as f64),
                        Some((i * 3 % 70) as f64),
                        Some(35.0 + (i * 2 % 35) as f64),
                    ]
                })
                .collect();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let layout = SceneLayout::for_accounts(
                Rect::new(0, 0, width, height),
                theme,
                &actors,
                all_limits,
            );
            let selected = if name.contains("focused") {
                6.min(layout.capacity - 1).min(actors.len() - 1)
            } else {
                usize::from(count > 0)
            };
            let cards = layout.focused_cards(
                &actors[..layout.capacity.min(actors.len())],
                selected,
                all_limits,
            );
            terminal
                .draw(|frame| {
                    render_scene(
                        frame, &pack, &actors, state, ms, 0, selected, false, None, all_limits, true,
                        &system, None, &dashboard, Some(&cards), None,
                    );
                    // Exercise the same responsive footer used by the running TUI.
                    if let Some(account) = accounts.get(selected.saturating_sub(1)) {
                        use super::super::{AccountRow, GatewayClient, TuiApp, draw_animated_overview_context};
                        let mut app = TuiApp::new(GatewayClient::new("http://127.0.0.1:1".into()).unwrap()).unwrap();
                        app.hidden_usage_accounts.clear();
                        app.hidden_usage_providers.clear();
                        app.data.accounts = vec![AccountRow {
                            provider:account.provider.to_ascii_lowercase(), provider_label:account.provider.clone(), key:"one".into(),
                            enabled:true, requests:19325, total_tokens:1284000, errors:29,
                            last_success_at:"2026-11-16T12:00:00Z".into(), ..Default::default()
                        }];
                        app.animation.scene = vec![SceneAccount { email:account.email.clone(), name:account.name.clone(), provider:account.provider.clone(), icon:account.icon, ..example_account() }];
                        app.animation.scene[0].limits = account.limits.clone();
                        app.animation.scene[0].resets = account.resets.clone();
                        app.animation.set_view(vec![0], 0, false, false, 0);
                        app.data.snapshot = json!({"generated_at":"2026-11-16T12:00:01Z", "quotas":{account.provider.to_ascii_lowercase():{"accounts":[{"key":"one","plan_type":"plus","rate_limit_reset_credits":{"available_count":1},"models":[{"id":"coding-fast"},{"id":"long-context"}]}]}}});
                        draw_animated_overview_context(frame, &app);
                    }
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut html = String::from("<!doctype html><meta charset=utf-8><style>body{background:#0c1015;margin:20px}pre{font:14px/16px 'DejaVu Sans Mono',monospace;white-space:pre}span{display:inline-block;height:16px;vertical-align:top}</style><pre>");
            let css = |color: Color| match color {
                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                _ => "#101d22".into(),
            };
            for y in 0..height {
                for x in 0..width {
                    let cell = &buffer[(x, y)];
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
            fs::write(directory.join(format!("{name}.html")), html).unwrap();
        }
    }

    #[test]
    fn pack_validation_rejects_bad_or_executable_content() {
        let mut value: Value = serde_json::from_slice(SOURCE).unwrap();
        for (key, bad) in [
            ("schema_version", json!(2)),
            ("id", json!("../evil")),
            ("script", json!("evil.js")),
        ] {
            let mut invalid = value.clone();
            invalid[key] = bad;
            assert!(Pack::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        value["sprites"]["cat"]["frames"][0][0] = json!("☃");
        assert!(Pack::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(Pack::parse(&vec![b' '; MAX_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn every_theme_size_and_connection_state_renders_safely() {
        for theme in Theme::ALL {
            for (w, h) in [(1, 1), (20, 8), (38, 18), (80, 24), (180, 42), (300, 90)] {
                for state in ["CONNECTING", "CONNECTED", "AUTH", "STALE"] {
                    for count in [0, 1, 200] {
                        let buffer = image_theme(w, h, state, 2300, count, theme);
                        let text = text(&buffer);
                        if w >= 80 && h >= 24 {
                            assert!(text.contains(theme.name()));
                            assert!(text.contains(state));
                            assert!(text.contains("t theme"));
                            if state == "AUTH" {
                                assert!(text.contains("Login required · [o] Log in"));
                                assert!(!text.contains("agent00@example.com"));
                            } else if count > 0 {
                                for field in ["agent00@example.com", "◎ Codex", "30%", "5h"] {
                                    assert!(text.contains(field), "{theme:?}: missing {field}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn text_is_control_safe_and_clipped_by_terminal_columns() {
        assert_eq!(
            clean("alice\n\x1b\t\u{202e}@example.com"),
            "alice@example.com"
        );
        assert_eq!(clipped("日本語alice", 5), "日本…");
        assert_eq!(clipped("日本語", 0), "");
        assert_eq!(clipped("alice", 1), "…");
        assert_eq!(clipped("a\u{301}b", 3), "a\u{301}b");
        assert_eq!(
            email_only("Alice Example <alice@example.com>"),
            Some("alice@example.com".into())
        );
        assert_eq!(email_only("Alice Example"), None);
    }

    #[test]
    fn account_identity_prefers_email_and_falls_back_to_name_in_bubbles_and_footer() {
        use super::super::{draw_animated_overview_context, AccountRow, GatewayClient, TuiApp};
        let mut app =
            TuiApp::new(GatewayClient::new("http://127.0.0.1:1".into()).unwrap()).unwrap();
        app.animation.pack = Some(Pack::parse(SOURCE).unwrap());
        app.data.accounts = vec![AccountRow {
            provider: "codex".into(),
            key: "opaque-id-not-an-email".into(),
            enabled: true,
            ..Default::default()
        }];
        for (label, email, account_email, expected) in [
            ("Private Name", "alice@example.com", "", "alice@example.com"),
            ("Private Name", "", "alias@example.com", "alias@example.com"),
            (" Café\n\t\u{202e} Work ", "", "", "Café Work"),
            (" \t\n ", "", "", "Account unavailable"),
        ] {
            app.data.accounts[0].label = label.into();
            app.data.snapshot = json!({"quotas":{"codex":{"accounts":[{
                "key":"opaque-id-not-an-email", "email":email, "account_email":account_email
            }]}}});
            app.animation.observe(&app.data);
            app.animation.set_view(vec![0], 0, false, false, 0);
            assert_eq!(app.animation.selected_identity(), expected);
            for theme in Theme::ALL {
                app.animation.theme = theme;
                app.animation.pack = Some(Pack::parse(theme_source(theme)).unwrap());
                for (width, height) in [(80, 24), (180, 42)] {
                    app.animation.resize(width, height);
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| {
                            app.animation.draw(frame, "CONNECTED");
                            draw_animated_overview_context(frame, &app);
                        })
                        .unwrap();
                    let screen = text(terminal.backend().buffer());
                    let rows = screen.lines().collect::<Vec<_>>();
                    assert!(
                        rows[..rows.len() - 4].join("\n").contains(expected),
                        "missing bubble: {theme:?} {width}x{height} {expected}"
                    );
                    assert!(
                        rows[rows.len() - 4..].join("\n").contains(expected),
                        "missing footer: {expected}"
                    );
                    assert!(!screen.contains("Private Name"));
                    assert!(!screen.contains("opaque-id-not-an-email"));
                    assert!(!screen.contains('\u{202e}'));
                }
            }
        }
        let mut account = example_account();
        account.email.clear();
        account.name = "A long account name without an email".into();
        assert!(
            account.bubble_width(false) >= Line::from(account.name.as_str()).width() as u16 + 2
        );
        account.email = "short@example.com".into();
        assert_eq!(account.identity(), "short@example.com");
    }

    #[test]
    fn compact_labels_remove_only_known_filler_and_preserve_meaning() {
        for (provider, full, compact) in [
            ("codex", "Code Gen 5h", "Gen 5h"),
            ("codex", "Code Review Weekly", "Review Weekly"),
            ("codex", "Code Gen Weekly (2)", "Gen Weekly (2)"),
            ("codex", "Code Interpreter 5h", "Code Interpreter 5h"),
            ("minimax", "5h window", "5h"),
            ("minimax", "Weekly window (2)", "Weekly (2)"),
            ("minimax", "MiniMax-M2.5 5h window", "MiniMax-M2.5 5h"),
            ("claude", "Sonnet Weekly", "Sonnet Weekly"),
            ("claude", "Weekly scoped", "Weekly scoped"),
            ("claude", "Extra usage", "Extra usage"),
            ("gemini", "Gemini Flash Lite", "Flash Lite"),
            ("gemini", "gemini-2.5-flash", "gemini-2.5-flash"),
            ("antigravity", "Claude Sonnet 4.5", "Claude Sonnet 4.5"),
            ("qwen", "Requests (2)", "Req (2)"),
            ("qwen", "Input Tokens", "In tokens"),
            ("qwen", "Output Tokens", "Out tokens"),
            ("grok", "Text requests", "Text req"),
            ("grok", "Text tokens", "Text tokens"),
            ("unknown", "Context window", "Context window"),
            ("unknown", "日本語 7d", "日本語 7d"),
        ] {
            assert_eq!(
                compact_quota_label(provider, full),
                compact,
                "{provider}: {full}"
            );
        }
        assert_eq!(usage_percent(75.0), "75%");
        assert_eq!(usage_percent(12.5), "12.5%");
        let mut account = example_account();
        account.limits = vec![("A very long model/version/period label".into(), Some(98.5))];
        let line = account.limit_line(0, 24, 8);
        assert!(line.ends_with("98.5% · — +8"));
        assert!(Line::from(line).width() <= 24);
        account.limits[0].1 = None;
        assert!(account.limit_line(0, 24, 0).ends_with('—'));
    }

    #[test]
    fn summaries_prefer_overview_buckets_but_never_hide_a_more_used_model() {
        use super::super::{quota_rows_from_provider, AccountRow};
        for (provider, snapshot, expected) in [
            (
                "codex",
                json!({"accounts": [{"key":"account", "code_generation": {
                "five_hour":{"used_percent":12.5}, "weekly":{"used_percent":31}
            }, "code_review":{"five_hour":{"used_percent":4}}}]}),
                vec![("Gen Weekly", 31.0), ("Gen 5h", 12.5), ("Review 5h", 4.0)],
            ),
            (
                "gemini",
                json!({"accounts": [{"key":"account", "models": [
                    {"model_id":"gemini-2.5-flash", "current":{"used_percent":55}},
                    {"model_id":"gemini-2.5-pro", "current":{"used_percent":25}}
                ]}]}),
                vec![("Flash", 55.0), ("Pro", 25.0)],
            ),
            (
                "codex",
                json!({"accounts": [{"key":"account", "code_generation": {
                    "five_hour":{"used_percent":30}
                }, "additional_rate_limits":[{"display_name":"Gen", "five_hour":{"used_percent":30}}]}]}),
                vec![("Gen 5h", 30.0), ("Gen 5h (2)", 30.0)],
            ),
            (
                "minimax",
                json!({"accounts": [{"key":"account", "current_window":{"used_percent":10},
                "weekly":{"used_percent":20}, "models":[
                    {"model_name":"M2.5", "current":{"used_percent":10}, "weekly":{"used_percent":20}},
                    {"model_name":"Video", "current":{"used_percent":95}, "weekly":{"used_percent":50}}
                ]}]}),
                vec![
                    ("Video 5h", 95.0),
                    ("Video Weekly", 50.0),
                    ("Weekly", 20.0),
                    ("5h", 10.0),
                ],
            ),
        ] {
            let data = GatewayData {
                accounts: vec![AccountRow {
                    provider: provider.into(),
                    key: "account".into(),
                    enabled: true,
                    ..Default::default()
                }],
                quotas: quota_rows_from_provider(provider, &snapshot),
                ..Default::default()
            };
            let original_labels = data
                .quotas
                .iter()
                .map(|quota| quota.limit_label.clone())
                .collect::<Vec<_>>();
            let mut animation = Animation::new();
            animation.pack = Some(Pack::parse(SOURCE).unwrap());
            animation.observe(&data);
            let expected = expected
                .into_iter()
                .map(|(name, used)| (name.to_string(), Some(used)))
                .collect::<Vec<_>>();
            assert_eq!(animation.scene[0].limits, expected, "{provider}");
            assert_eq!(animation.scene[0].used, expected[0].1);
            assert_eq!(
                data.quotas
                    .iter()
                    .map(|quota| quota.limit_label.clone())
                    .collect::<Vec<_>>(),
                original_labels
            );
        }
    }

    #[test]
    fn credits_use_the_matching_account_balance_without_changing_quota_usage() {
        use super::super::{quota_rows_from_provider, AccountRow};
        for (provider, credits, expected) in [
            (
                "codex",
                json!({"balance":"60482.4406550000"}),
                "60482.4406550000",
            ),
            ("codex", json!({"balance":"0"}), "0"),
            ("codex", json!({"unlimited":true}), "Unlimited"),
            (
                "codex",
                json!({"has_credits":false}),
                "No credits available",
            ),
            (
                "claude",
                json!({"balance":"12.50","currency":"USD"}),
                "12.50 USD",
            ),
            (
                "claude",
                json!({"enabled":false,"monthly_limit":"100","used":"10"}),
                "Not reported",
            ),
            ("claude", Value::Null, "Not reported"),
            (
                "compatible",
                json!({"balance":"12.50","currency":"USD"}),
                "12.50 USD",
            ),
        ] {
            let mut selected = if provider == "codex" {
                json!({"code_generation":{"five_hour":{"used_percent":30}}})
            } else {
                json!({"five_hour":{"used_percent":30}})
            };
            selected["key"] = json!("selected-account");
            selected["label"] = json!("Selected fixture");
            selected["credits"] = credits.clone();
            selected["rate_limit_reset_credits"] = json!({"available_count":5});
            let quota = json!({"accounts":[
                {"key":"other-account", "label":"Other fixture", "credits":{"balance":"999"}},
                selected
            ]});
            let data = GatewayData {
                accounts: vec![AccountRow {
                    provider: provider.into(),
                    key: "selected-account".into(),
                    label: "Selected fixture".into(),
                    enabled: true,
                    ..Default::default()
                }],
                quotas: quota_rows_from_provider(provider, &quota),
                // Account metadata precedes the quota object in snapshot values.
                snapshot: json!({"accounts":[{"provider":provider,"key":"selected-account"}],
                    "quotas":{provider:quota}}),
                ..Default::default()
            };
            let mut animation = Animation::new();
            animation.pack = Some(Pack::parse(SOURCE).unwrap());
            animation.observe(&data);
            let account = &animation.scene[0];
            assert_eq!(
                account.credits.as_deref(),
                Some(expected),
                "{provider}: {credits}"
            );
            assert_eq!(account.used, Some(30.0));
            assert_eq!(account.limits.len(), 1, "{provider}: {:?}", account.limits);
            assert_eq!(account.quota_rows(false), 1);
            assert_eq!(account.quota_rows(true), 2);
        }
    }

    #[test]
    fn all_limits_shows_both_account_credits_in_every_theme_and_compact_layout() {
        let mut accounts = sample_accounts(2);
        accounts[0].credits = Some("8.25".into());
        accounts[1].credits = Some("12.50 USD".into());
        // Many quota windows must not push the balance out of a short bubble.
        for account in &mut accounts {
            account
                .limits
                .extend((0..8).map(|i| (format!("Extra {i}"), Some(10.0))));
        }
        for theme in Theme::ALL {
            let pack = Pack::parse(theme_source(theme)).unwrap();
            for (width, height) in [(80, 24), (180, 42), (300, 90)] {
                let buffer =
                    image_accounts(width, height, "CONNECTED", 700, &pack, &accounts, true);
                let screen = text(&buffer);
                for expected in ["Credits 8.25", "Credits 12.50 USD"] {
                    assert!(
                        screen.contains(expected),
                        "{theme:?} {width}x{height}: missing {expected}\n{screen}"
                    );
                }
                assert!(screen.contains("+"), "quota overflow remains marked");
                let normal =
                    image_accounts(width, height, "CONNECTED", 700, &pack, &accounts, false);
                assert!(!text(&normal).contains("Credits"));
                let layout = SceneLayout::for_accounts(
                    buffer.area,
                    theme,
                    &accounts.iter().collect::<Vec<_>>(),
                    true,
                );
                for (index, account) in accounts.iter().enumerate() {
                    let (bubble, _) = speech_bubble(&layout, index, account, theme, 700, true);
                    assert!(bubble.x >= layout.body.x && bubble.right() <= layout.body.right());
                    assert!(bubble.y >= layout.body.y && bubble.bottom() <= layout.body.bottom());
                }
            }
            for (width, height) in [(1, 1), (20, 8), (38, 18)] {
                image_accounts(width, height, "CONNECTED", 700, &pack, &accounts, true);
            }
        }
    }

    #[test]
    fn bubbles_fit_content_and_put_provider_status_above_email() {
        let mut accounts = sample_accounts(3);
        accounts[1].email = "a-very-long-account-address-for-responsive-layout@example.com".into();
        accounts[2].email = "日本語-account@example.com".into();
        for theme in Theme::ALL {
            let pack = Pack::parse(theme_source(theme)).unwrap();
            for (width, height) in [(20, 8), (38, 18), (80, 24), (180, 42), (300, 90)] {
                for all_limits in [false, true] {
                    let layout = SceneLayout::for_accounts(
                        Rect::new(0, 0, width, height),
                        theme,
                        &accounts.iter().collect::<Vec<_>>(),
                        all_limits,
                    );
                    let buffer = image_accounts(
                        width,
                        height,
                        "CONNECTED",
                        700,
                        &pack,
                        &accounts,
                        all_limits,
                    );
                    for (index, account) in accounts.iter().take(layout.capacity).enumerate() {
                        let (card, tail) =
                            speech_bubble(&layout, index, account, theme, 700, all_limits);
                        assert!(card.x >= layout.body.x && card.right() <= layout.body.right());
                        assert!(card.y >= layout.body.y && card.bottom() <= layout.body.bottom());
                        assert!(card.width <= account.bubble_width(all_limits));
                        assert!(card.height <= 8);
                        assert!(tail >= card.x && tail < card.right());
                        for other in index + 1..accounts.len().min(layout.capacity) {
                            assert!(card
                                .intersection(
                                    speech_bubble(
                                        &layout,
                                        other,
                                        &accounts[other],
                                        theme,
                                        700,
                                        all_limits
                                    )
                                    .0
                                )
                                .is_empty());
                        }
                        if width >= 38 && height >= 18 {
                            let row = card.y + u16::from(card.height >= 5 && card.width >= 8);
                            let read = |y| {
                                (card.x..card.right())
                                    .map(|x| buffer[(x, y)].symbol())
                                    .collect::<String>()
                            };
                            assert!(
                                read(row).contains(account.provider.as_str()),
                                "{theme:?} {width}x{height}: {}",
                                read(row)
                            );
                            assert!(read(row + 1).contains('@') || read(row + 1).contains('…'));
                        }
                    }
                }
            }
            let single =
                SceneLayout::for_accounts(Rect::new(0, 0, 80, 24), theme, &[&accounts[0]], false);
            let (bubble, _) = speech_bubble(&single, 0, &accounts[0], theme, 700, false);
            assert_eq!(single.capacity, 1);
            assert!(bubble.width < single.cards[0].width);
            assert!((i32::from(bubble.x + bubble.width / 2) - 40).abs() <= 1);
        }
    }

    fn example_account() -> SceneAccount {
        SceneAccount {
            status: 'h',
            recent_until: None,
            used: Some(75.0),
            email: "alice@example.com".into(),
            name: "Alice Example".into(),
            provider: "Codex".into(),
            icon: "◎",
            quota: "5h".into(),
            requests: 10,
            models: None,
            model_selected: 0,
            resets: vec![],
            credits: None,
            limits: (0..10)
                .map(|index| (format!("Window {index}"), Some(index as f64)))
                .collect(),
        }
    }

    #[test]
    fn speech_bubbles_are_above_their_actors_and_tails_follow_motion_without_collisions() {
        let account = example_account();
        for theme in Theme::ALL {
            for (width, height) in [(80, 24), (180, 42), (240, 76)] {
                let layout = SceneLayout::new(Rect::new(2, 3, width, height), theme);
                for ms in [0, 700, 10300, 12000, 15400] {
                    for index in 0..layout.capacity {
                        let (bubble, tail) =
                            speech_bubble(&layout, index, &account, theme, ms, false);
                        assert!(bubble.x >= layout.body.x && bubble.right() <= layout.body.right());
                        assert!(
                            bubble.y >= layout.body.y && bubble.bottom() < layout.body.bottom()
                        );
                        assert!(tail >= bubble.x && tail < bubble.right());
                        if theme.on_platform() {
                            let (_, bob, _) =
                                cat_motion_for_height(index, account.status, ms, height);
                            let head = layout.stage.y
                                + (cafe_counter(i32::from(layout.stage.height) * 2)
                                    - cat_head_height(index, account.status, ms)
                                    - bob) as u16
                                    / 2;
                            assert!(bubble.bottom() < head);
                        }
                        for other in index + 1..layout.capacity {
                            assert!(bubble
                                .intersection(
                                    speech_bubble(&layout, other, &account, theme, ms, false).0
                                )
                                .is_empty());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn summaries_are_capped_at_two_or_four_quota_rows_and_mark_overflow() {
        let pack = Pack::parse(SOURCE).unwrap();
        let account = example_account();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &pack,
                    &[&account],
                    "CONNECTED",
                    700,
                    0,
                    0,
                    false,
                    None,
                    true,
                    true,
                    &super::super::system_metrics::Snapshot::default(),
                    None,
                    &Dashboard::default(),
                )
            })
            .unwrap();
        let screen = text(terminal.backend().buffer());
        assert!(screen.contains("Window 0 0% · —"));
        assert!(screen.contains("Window 1 1% · — +8"));
        assert!(!screen.contains("Window 4"));
        assert_eq!(account.quota_rows(false), 2);
        assert_eq!(account.quota_rows(true), 4);
        assert_eq!(
            SceneLayout::for_accounts(Rect::new(0, 0, 80, 24), Theme::Cafe, &[&account], true)
                .cards[0]
                .height,
            6
        );
    }

    #[test]
    fn rain_is_deterministic_layered_and_clipped_behind_window_frames() {
        let pack = Pack::parse(SOURCE).unwrap();
        let draw = |ms| {
            let mut canvas = Canvas::new(70, 40, &pack);
            rainy_window(&mut canvas, 8, 4, 40, 30, ms);
            canvas.pixels
        };
        let first = draw(2300);
        assert_eq!(first, draw(2300));
        let second = draw(2450);
        assert_ne!(first, second);
        for y in 0..40 {
            for x in 0..70 {
                if first[y * 70 + x] != second[y * 70 + x] {
                    assert!(x > 8 && x < 47 && y > 4 && y < 33);
                    assert_ne!(x, 28, "rain painted over a vertical mullion");
                    assert!(
                        !(19..=20).contains(&y),
                        "rain painted over a horizontal mullion"
                    );
                }
            }
        }
        let distant = rain_drop(1, 42, 40, 30, 2300);
        let near = rain_drop(0, 42, 40, 30, 2300);
        assert!(near.length > distant.length);
        assert!(near.opacity > distant.opacity);
        assert_ne!(near.x, rain_drop(0, 42, 40, 30, 2350).x);
    }

    #[test]
    fn animated_selection_filters_sections_and_modals_share_the_overview_state() {
        use super::super::{AccountRow, GatewayClient, OverviewSection, TuiApp};
        let mut app =
            TuiApp::new(GatewayClient::new("http://127.0.0.1:1".into()).unwrap()).unwrap();
        app.hidden_usage_accounts.clear();
        app.hidden_usage_providers.clear();
        app.account_orders.clear();
        app.provider_order.clear();
        app.limit_display_mode = super::super::LimitDisplayMode::OnePerAccount;
        app.data.accounts = (0..3)
            .map(|index| AccountRow {
                provider: "codex".into(),
                key: format!("account-{index}"),
                label: format!("Private Name {index}"),
                account_id: format!("person{index}@example.com"),
                enabled: true,
                ..Default::default()
            })
            .collect();
        app.animation.pack = Some(Pack::parse(SOURCE).unwrap());
        app.animation.theme = Theme::Cafe;
        app.animation.active = true;
        app.animation.observe(&app.data);
        app.sync_animated_accounts();
        assert_eq!(app.animation.selected_identity(), "person0@example.com");
        app.move_selection(1);
        app.sync_animated_accounts();
        assert_eq!(app.animation.selected_identity(), "person1@example.com");
        app.hidden_usage_accounts.insert((
            "codex".into(),
            super::super::usage_account_filter_key(&app.data.accounts[0]),
        ));
        app.overview_account_selected = 0;
        app.clamp_selection();
        app.sync_animated_accounts();
        assert_eq!(app.animation.selected_identity(), "person1@example.com");
        assert_eq!(app.animation.order, vec![1, 2]);
        app.previous_overview_section();
        assert!(app.overview_section == OverviewSection::CustomModels);
        app.next_overview_section();
        assert!(app.overview_section == OverviewSection::AccountUsage);
        let mut terminal = Terminal::new(TestBackend::new(180, 42)).unwrap();
        terminal
            .draw(|frame| super::super::draw_tui(frame, &app))
            .unwrap();
        let screen = text(terminal.backend().buffer());
        assert!(!screen.contains("Private Name"));
        assert!(!screen.contains("person0@example.com"));
        app.open_selected_account_details();
        assert!(app.show_account_details);
        terminal
            .draw(|frame| super::super::draw_tui(frame, &app))
            .unwrap();
        assert!(text(terminal.backend().buffer()).contains("Account Details"));
        app.show_account_details = false;
        app.show_command_modal = true;
        terminal
            .draw(|frame| super::super::draw_tui(frame, &app))
            .unwrap();
        assert!(text(terminal.backend().buffer()).contains("Commands"));
    }

    #[test]
    fn placards_never_inherit_art_pixels_and_missing_usage_is_explicit() {
        for theme in Theme::ALL {
            let buffer = image_theme(80, 24, "CONNECTED", 700, 4, theme);
            let accounts = sample_accounts(4);
            let layout = SceneLayout::for_accounts(
                buffer.area,
                theme,
                &accounts.iter().collect::<Vec<_>>(),
                false,
            );
            for (index, account) in accounts.iter().take(layout.capacity).enumerate() {
                let (card, _) = speech_bubble(&layout, index, account, theme, 700, false);
                for y in card.y + 1..card.bottom() - 1 {
                    for x in card.x + 1..card.right() - 1 {
                        assert_ne!(buffer[(x, y)].symbol(), "▀");
                    }
                }
            }
            let mut animation = Animation::new();
            animation.pack = Some(Pack::parse(theme_source(theme)).unwrap());
            animation.theme = theme;
            let data = GatewayData {
                accounts: vec![super::super::AccountRow {
                    provider: "claude".into(),
                    label: "日本語の長い名前 alice@example.com".repeat(5),
                    key: "account".into(),
                    enabled: true,
                    ..Default::default()
                }],
                ..Default::default()
            };
            animation.observe(&data);
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| animation.draw(frame, "CONNECTED"))
                .unwrap();
            let text = text(terminal.backend().buffer());
            assert!(text.contains("Usage unavailable"));
            assert!(text.contains("✳ Claude"));
            assert!(!text.contains("0% used"));
        }
    }

    #[test]
    fn themes_use_different_arrangements_and_selection_survives_resizing() {
        assert_eq!(
            SceneLayout::new(Rect::new(0, 0, 180, 42), Theme::Cafe).capacity,
            12
        );
        assert_eq!(
            SceneLayout::new(Rect::new(0, 0, 180, 42), Theme::Office).capacity,
            24
        );
        assert_ne!(
            image_theme(80, 24, "CONNECTED", 700, 4, Theme::Cafe),
            image_theme(80, 24, "CONNECTED", 700, 4, Theme::Office)
        );
        let mut animation = Animation::new();
        animation.scene = (0..25)
            .map(|_| SceneAccount {
                status: 'h',
                recent_until: None,
                used: None,
                email: String::new(),
                name: String::new(),
                provider: "Codex".into(),
                icon: "◎",
                quota: String::new(),
                requests: 0,
                limits: vec![],
                resets: vec![],
                credits: None,
                models: None,
                model_selected: 0,
            })
            .collect();
        animation.theme = Theme::Cafe;
        animation.set_view((0..25).collect(), 0, false, false, 0);
        animation.resize(80, 24);
        animation.turn_page(1);
        assert_eq!((animation.selected, animation.page), (4, 1));
        animation.set_view((0..25).collect(), 7, false, false, 0);
        assert_eq!((animation.selected, animation.page), (8, 2));
        animation.theme = Theme::Office;
        animation.resize(180, 42);
        assert_eq!((animation.selected, animation.page), (8, 0));
        animation.turn_page(-1);
        assert_eq!((animation.selected, animation.page), (24, 1));
        animation.scene.truncate(1);
        animation.set_view(vec![0], 24, false, false, 0);
        animation.resize(1, 1);
        assert_eq!((animation.selected, animation.page), (1, 1));
    }

    #[test]
    fn animation_changes_pixels_and_connection_and_account_states_are_distinct() {
        assert_ne!(
            image(100, 30, "CONNECTED", 0, 3),
            image(100, 30, "CONNECTED", 700, 3)
        );
        assert_ne!(
            image(100, 30, "CONNECTED", 0, 0),
            image(100, 30, "AUTH", 0, 0)
        );
        assert_ne!(
            image(100, 30, "CONNECTED", 0, 1),
            image(100, 30, "CONNECTED", 0, 3)
        );
    }

    #[test]
    fn pause_freezes_animation_time() {
        let mut animation = Animation::new();
        animation.pause();
        let elapsed = animation.elapsed();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(animation.elapsed(), elapsed);
        animation.pause();
        assert!(!animation.paused);
    }

    #[test]
    fn telemetry_uses_real_account_state_and_only_observed_request_increases() {
        use super::super::{AccountRow, QuotaRow};
        let mut animation = Animation::new();
        animation.pack = Some(Pack::parse(SOURCE).unwrap());
        let mut data = GatewayData::default();
        for index in 0..4 {
            data.accounts.push(AccountRow {
                provider: "codex".into(),
                key: format!("account-{index}"),
                label: format!("Account {index}"),
                enabled: index != 2,
                cooldown: index == 1,
                requests: 50,
                attention_reasons: if index == 3 {
                    vec!["near quota".into()]
                } else {
                    Vec::new()
                },
                ..AccountRow::default()
            });
        }
        data.quotas.push(QuotaRow {
            provider: "codex".into(),
            account_key: "account-0".into(),
            used_percent_value: Some(75.0),
            limit_label: "5h window".into(),
            ..QuotaRow::default()
        });
        data.snapshot = json!({"dashboard": {"accounts": [{"key": "account-0", "email": "alice@example.com"}]}});
        data.quotas.push(QuotaRow {
            provider: "codex".into(),
            account_key: "account-0".into(),
            used_percent_value: Some(20.0),
            ..QuotaRow::default()
        });
        data.quotas.push(QuotaRow {
            provider: "codex".into(),
            account_key: "account-1".into(),
            used_percent_value: Some(f64::NAN),
            ..QuotaRow::default()
        });
        animation.observe(&data);
        assert_eq!(
            animation
                .scene
                .iter()
                .map(|account| account.status)
                .collect::<Vec<_>>(),
            vec!['h', 'c', 'd', 'a']
        );
        assert_eq!(animation.scene[0].usage(), "75%");
        assert_eq!(animation.scene[0].email, "alice@example.com");
        assert_eq!(animation.scene[0].quota, "5h");
        assert_eq!(animation.scene[0].provider, "Codex");
        assert_eq!(animation.scene[0].icon, "◎");
        assert_eq!(animation.scene[1].email, "");
        assert_eq!(animation.scene[1].usage(), "Usage unavailable");
        assert!(animation
            .scene
            .iter()
            .all(|account| account.recent_until.is_none()));
        data.accounts[0].requests += 1;
        animation.observe(&data);
        assert!(animation.scene[0].recent_until.is_some());
        assert!(animation.scene[1].recent_until.is_none());
        data.quotas[0].used_percent_value = Some(150.0);
        animation.observe(&data);
        assert_eq!(animation.scene[0].used, Some(100.0));
        animation.observe(&GatewayData::default());
        assert!(animation.scene.is_empty());
        assert!(animation.activity.is_empty());
        animation.exit();
        animation.observe(&data);
        assert!(
            animation.scene.is_empty(),
            "uninstalled/inactive plugins must not process telemetry"
        );
    }

    #[test]
    fn installation_replaces_only_the_pack_and_round_trips() {
        let dir =
            std::env::temp_dir().join(format!("iogw-plugin-test-{:016x}", rand::random::<u64>()));
        let target = dir.join("pixel-cafe.json");
        install(&target, SOURCE).unwrap();
        assert_eq!(Pack::load(&target).unwrap().version, "1.0.0");
        install(&target, SOURCE).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_file(target).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
