//! Optional Panda3D GPU renderer. Rust owns all account state and input.
//! Pipe IO runs off the UI thread; queues, image sizes and wait times are bounded.
use super::sixel::{self, Canvas, Pixels};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Clear, Paragraph},
    Frame,
};
use serde::Serialize;
use std::{
    cell::{Cell as Shared, RefCell},
    collections::VecDeque,
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(super) const PAGE_SIZE: usize = 7;
/// Frames render at this multiple of the terminal's half-block pixel grid and are
/// averaged down when drawn, which sharpens edges and thin geometry.
const SUPERSAMPLE: u32 = 3;
const MAX_WIDTH: u32 = 640 * SUPERSAMPLE;
const MAX_HEIGHT: u32 = 400 * SUPERSAMPLE;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const FRAME_TIMEOUT: Duration = Duration::from_secs(4);
const HELPER: &str = include_str!("scene3d/panda3d_helper.py");
const MESH: &[u8] = include_bytes!("scene3d/cafe.mesh");
/// A soft dusk blue; keep in step with `BACKGROUND` in the helper.
const BACKGROUND: Color = Color::Rgb(38, 43, 58);
/// Aimed a little toward the camera so the whole floor, near corner included, fits.
const TARGET: [f32; 3] = [0., 1.3, 2.];
const DEFAULT_YAW: f32 = 0.22;
/// Camera height above the target as a fraction of its distance.
const DEFAULT_TILT: f32 = 0.5;
/// Radians of orbit per dragged column, and tilt per dragged row.
const DRAG_YAW: f32 = 0.02;
const DRAG_TILT: f32 = 0.05;

/// Set when the terminal was cleared behind the scene's back (a prompt left and
/// re-entered the alternate screen), so a sixel image must be sent again.
static WIPED: AtomicBool = AtomicBool::new(false);

pub(super) fn screen_wiped() {
    WIPED.store(true, Ordering::Relaxed);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Models,
    Account { provider: String, key: String },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct Avatar {
    #[serde(skip_serializing)]
    pub target: Target,
    /// Label rows shown above the actor (the same data as the animated UI's
    /// bubble); `None` uses the label's default colour.
    #[serde(skip_serializing)]
    pub lines: Vec<(String, Option<Color>)>,
    pub position: [f32; 3],
    pub color: [f32; 3],
    pub status: [f32; 3],
    pub selected: bool,
    pub models: bool,
    /// Facing angle about the vertical axis; 0 faces the camera's default side (+z).
    pub heading: f32,
    /// Leg-swing phase in radians and gait amplitude (0 standing, 1 walking).
    pub gait: f32,
    pub stride: f32,
}

/// Patrol routes (x, z) through the 20 x 20 café, one territory per cat. Loops
/// close on themselves; two-point routes walk back and forth. None crosses
/// furniture, and any two are at least `MIN_GAP` apart so cats never overlap.
const ROUTES: [&[[f32; 2]]; 7] = [
    &[[-4.2, -0.2], [4.2, -0.2], [4.2, 7.], [-4.2, 7.]],
    &[[-2.4, 1.6], [2.4, 1.6], [2.4, 5.2], [-2.4, 5.2]],
    &[[-8.6, -1.8], [-8.6, 7.2]],
    &[[8.6, -1.8], [8.6, 7.2]],
    &[[-4.8, 8.9], [4.8, 8.9]],
    &[[4.4, -8.], [7.4, -8.], [7.4, -5.5], [4.4, -5.5]],
    &[[-4.7, -7.2], [-4.7, -3.]],
];
#[cfg(test)]
const MIN_GAP: f32 = 1.6;
/// The custom-models robot minds the bar instead of patrolling.
const BARISTA: [f32; 2] = [-2.6, -7.3];
const WALK_SPEED: f32 = 0.8;
/// Distance covered by one full four-paw step cycle, so paws do not skate.
const STRIDE: f32 = 0.65;

pub(super) struct Walk {
    pub position: [f32; 2],
    pub heading: f32,
    pub gait: f32,
    pub stride: f32,
}

/// Where an actor is at `time`: a pure function of the clock, so a paused scene
/// freezes and every render of the same instant agrees with picking and labels.
pub(super) fn walk(slot: usize, time: f32) -> Walk {
    if slot == 0 {
        return Walk {
            position: BARISTA,
            heading: 0.,
            gait: 0.,
            stride: 0.,
        };
    }
    let route = ROUTES[slot % ROUTES.len()];
    let leg = |i: usize| {
        let (a, b) = (route[i], route[(i + 1) % route.len()]);
        (a, b, (b[0] - a[0]).hypot(b[1] - a[1]))
    };
    let heading = |i: usize| {
        let (a, b, _) = leg(i);
        (b[0] - a[0]).atan2(b[1] - a[1])
    };
    let pause = |i: usize| 2. + ((slot + i) % 3) as f32;
    let cycle: f32 = (0..route.len())
        .map(|i| leg(i).2 / WALK_SPEED + pause(i))
        .sum();
    let mut t = (time + slot as f32 * 3.7).rem_euclid(cycle);
    for i in 0..route.len() {
        let (a, b, length) = leg(i);
        let duration = length / WALK_SPEED;
        if t < duration {
            // Ease in and out so cats set off and stop instead of gliding.
            let u = t / duration;
            let eased = u * u * (3. - 2. * u);
            return Walk {
                position: [a[0] + (b[0] - a[0]) * eased, a[1] + (b[1] - a[1]) * eased],
                heading: heading(i),
                gait: eased * length * std::f32::consts::TAU / STRIDE,
                stride: (6. * u * (1. - u)).min(1.),
            };
        }
        t -= duration;
        if t < pause(i) {
            // Stand and turn toward the next leg.
            let from = heading(i);
            let delta = (heading((i + 1) % route.len()) - from + std::f32::consts::PI)
                .rem_euclid(std::f32::consts::TAU)
                - std::f32::consts::PI;
            let u = (t / 0.5).min(1.);
            return Walk {
                position: b,
                heading: from + delta * u * u * (3. - 2. * u),
                gait: 0.,
                stride: 0.,
            };
        }
        t -= pause(i);
    }
    Walk {
        position: route[0],
        heading: heading(0),
        gait: 0.,
        stride: 0.,
    }
}

pub(super) struct View {
    pub time: f32,
    pub selected: usize,
    pub total: usize,
    pub avatars: Vec<Avatar>,
    pub bubbles: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct Request {
    width: u32,
    height: u32,
    time: f32,
    camera: [f32; 3],
    objects: Vec<Avatar>,
    #[serde(skip_serializing)]
    page: usize,
    #[serde(skip_serializing)]
    pages: usize,
    #[serde(skip_serializing)]
    bubbles: bool,
}

impl Request {
    fn same_visuals(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.camera == other.camera
            && self.objects.len() == other.objects.len()
            && self.objects.iter().zip(&other.objects).all(|(a, b)| {
                a.position == b.position
                    && a.color == b.color
                    && a.status == b.status
                    && a.selected == b.selected
                    && a.models == b.models
                    && a.heading == b.heading
                    && a.gait == b.gait
                    && a.stride == b.stride
            })
    }
}

#[derive(Clone)]
struct Image {
    request: Request,
    pixels: Arc<Vec<u8>>,
}

enum Reply {
    Ready(String),
    Image(Image),
    Failed(String),
}

struct Process {
    child: Child,
    tx: Option<SyncSender<Request>>,
    rx: Receiver<Reply>,
    worker: Option<JoinHandle<()>>,
    logger: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
}

impl Process {
    fn spawn(mut command: Command) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not start Python: {error}"))?;
        let mut input = child.stdin.take().ok_or("missing renderer stdin")?;
        let mut output = child.stdout.take().ok_or("missing renderer stdout")?;
        let mut errors = child.stderr.take().ok_or("missing renderer stderr")?;
        let (tx, requests) = mpsc::sync_channel::<Request>(1);
        let (replies, rx) = mpsc::sync_channel(1);
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let mut process = Self {
            child,
            tx: Some(tx),
            rx,
            worker: None,
            logger: None,
            stderr: stderr.clone(),
        };
        process.logger = Some(
            thread::Builder::new()
                .name("iogw-3d-log".into())
                .spawn(move || {
                    let mut buffer = [0; 1024];
                    while let Ok(count) = errors.read(&mut buffer) {
                        if count == 0 {
                            break;
                        }
                        let mut tail = stderr.lock().unwrap_or_else(|error| error.into_inner());
                        tail.extend(&buffer[..count]);
                        while tail.len() > 8192 {
                            tail.pop_front();
                        }
                    }
                })
                .map_err(|error| error.to_string())?,
        );
        process.worker = Some(
            thread::Builder::new()
                .name("iogw-3d-pipe".into())
                .spawn(move || {
                    let result = (|| -> Result<(), String> {
                        input.write_all(b"SCN1").map_err(io_error)?;
                        input
                            .write_all(&(MESH.len() as u32).to_le_bytes())
                            .map_err(io_error)?;
                        input.write_all(MESH).map_err(io_error)?;
                        input.flush().map_err(io_error)?;
                        let renderer = read_ready(&mut output)?;
                        replies
                            .try_send(Reply::Ready(renderer))
                            .map_err(|error| error.to_string())?;
                        while let Ok(request) = requests.recv() {
                            let json =
                                serde_json::to_vec(&request).map_err(|error| error.to_string())?;
                            input
                                .write_all(&(json.len() as u32).to_le_bytes())
                                .map_err(io_error)?;
                            input.write_all(&json).map_err(io_error)?;
                            input.flush().map_err(io_error)?;
                            let pixels = read_image(&mut output, &request)?;
                            replies
                                .try_send(Reply::Image(Image {
                                    request,
                                    pixels: Arc::new(pixels),
                                }))
                                .map_err(|error| error.to_string())?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        let _ = replies.try_send(Reply::Failed(error));
                    }
                })
                .map_err(|error| error.to_string())?,
        );
        Ok(process)
    }

    fn error(&self, message: &str) -> String {
        let tail = self
            .stderr
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let bytes: Vec<_> = tail.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let detail = text
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        let detail: String = detail
            .chars()
            .filter(|ch| !ch.is_control())
            .take(220)
            .collect();
        format!(
            "3D unavailable: {message}{}{}; install Panda3D 1.10.16 and set IOGW_PANDA3D_PYTHON",
            if detail.is_empty() { "" } else { " · " },
            detail
        )
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Close the command queue before joining a worker waiting for a request.
        self.tx.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(logger) = self.logger.take() {
            let _ = logger.join();
        }
    }
}

fn io_error(error: std::io::Error) -> String {
    format!("renderer pipe: {error}")
}

fn read_u32(reader: &mut impl Read) -> Result<u32, String> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes).map_err(io_error)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_ready(reader: &mut impl Read) -> Result<String, String> {
    let mut magic = [0; 4];
    reader.read_exact(&mut magic).map_err(io_error)?;
    if &magic != b"P3D1" {
        return Err("invalid renderer greeting".into());
    }
    let length = read_u32(reader)?;
    if length > 4096 {
        return Err("renderer greeting exceeds limit".into());
    }
    let mut bytes = vec![0; length as usize];
    reader.read_exact(&mut bytes).map_err(io_error)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if value["gpu"] != true {
        return Err("hardware GPU rendering is unavailable".into());
    }
    Ok(value["renderer"]
        .as_str()
        .unwrap_or("GPU")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(160)
        .collect())
}

fn read_image(reader: &mut impl Read, request: &Request) -> Result<Vec<u8>, String> {
    let mut magic = [0; 4];
    reader.read_exact(&mut magic).map_err(io_error)?;
    let width = read_u32(reader)?;
    let height = read_u32(reader)?;
    let length = read_u32(reader)?;
    if &magic != b"RGB1"
        || width != request.width
        || height != request.height
        || width == 0
        || height == 0
        || width > MAX_WIDTH
        || height > MAX_HEIGHT
        || length != width * height * 3
    {
        return Err("invalid renderer image dimensions".into());
    }
    let mut pixels = vec![0; length as usize];
    reader.read_exact(&mut pixels).map_err(io_error)?;
    Ok(pixels)
}

pub(super) struct Scene {
    process: Process,
    ready: bool,
    renderer: String,
    pending: Option<Instant>,
    image: Option<Image>,
    presented: RefCell<Option<(Rect, Image)>>,
    area: Rect,
    yaw: f32,
    tilt: f32,
    zoom: f32,
    last_request: Instant,
    /// `IOGW_3D_SIXEL=1`: draw real pixels with sixel instead of coloured half-blocks.
    sixel: bool,
    /// The terminal's cell size in pixels, while sixel is requested and usable.
    cell: Option<(u32, u32)>,
    /// Where sixel images go, checked once a second: a tmux pane can move or hide.
    route: Result<Option<sixel::Route>, &'static str>,
    route_checked: Instant,
    /// When the whole image was last repainted (tmux may repaint over passthrough).
    repainted: Shared<Instant>,
    canvas: RefCell<Option<Canvas>>,
}

impl Scene {
    pub fn new() -> Result<Self, String> {
        let python = std::env::var_os("IOGW_PANDA3D_PYTHON")
            .or_else(|| {
                let data = if cfg!(windows) {
                    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
                } else {
                    std::env::var_os("XDG_DATA_HOME")
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                        .or_else(|| {
                            std::env::var_os("HOME")
                                .map(|home| PathBuf::from(home).join(".local/share"))
                        })
                }?;
                let executable = data.join("iogw/panda3d").join(if cfg!(windows) {
                    "Scripts/python.exe"
                } else {
                    "bin/python"
                });
                executable.is_file().then(|| executable.into_os_string())
            })
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "python".into()
                } else {
                    "python3".into()
                }
            });
        let sixel = std::env::var_os("IOGW_3D_SIXEL").is_some_and(|value| value == "1");
        let mut command = Command::new(python);
        command.args(["-u", "-c", HELPER]);
        // The trusted helper receives visual parameters only, never session cookies
        // or gateway credentials, and does not need the admin key environment.
        command.env_remove("IOGW_ADMIN_API_KEY");
        command.env("PYTHONDONTWRITEBYTECODE", "1");
        Ok(Self {
            process: Process::spawn(command).map_err(|error| {
                format!(
                "3D unavailable: {error}; set IOGW_PANDA3D_PYTHON to a Python with Panda3D 1.10.16"
            )
            })?,
            ready: false,
            renderer: String::new(),
            pending: Some(Instant::now()),
            image: None,
            presented: RefCell::new(None),
            area: Rect::default(),
            yaw: DEFAULT_YAW,
            tilt: DEFAULT_TILT,
            zoom: 1.,
            last_request: Instant::now() - Duration::from_millis(50),
            sixel,
            cell: None,
            route: if sixel {
                sixel::route()
            } else {
                Ok(Some(sixel::Route::Direct))
            },
            route_checked: Instant::now(),
            repainted: Shared::new(Instant::now()),
            canvas: RefCell::new(None),
        })
    }

    pub fn camera(&mut self, orbit: f32, zoom: f32) {
        (self.yaw, self.tilt) = orbited(self.yaw, self.tilt, orbit, 0.);
        self.zoom = (self.zoom * zoom).clamp(0.3, 1.5);
    }

    /// Click-and-drag by a number of terminal cells: horizontal orbits, vertical tilts.
    pub fn drag(&mut self, columns: i32, rows: i32) {
        (self.yaw, self.tilt) = orbited(
            self.yaw,
            self.tilt,
            -columns as f32 * DRAG_YAW,
            rows as f32 * DRAG_TILT,
        );
    }

    pub fn reset_camera(&mut self) {
        self.yaw = DEFAULT_YAW;
        self.tilt = DEFAULT_TILT;
        self.zoom = 1.;
    }

    pub fn update(&mut self, terminal: Rect, view: View) -> Result<(), String> {
        self.area = scene_area(terminal);
        self.cell = self.sixel.then(|| cell_pixels(self.area)).flatten();
        if self.cell.is_some() && self.route_checked.elapsed() >= Duration::from_secs(1) {
            self.route = sixel::route();
            self.route_checked = Instant::now();
        }
        if self.route.is_err() {
            self.cell = None;
        }
        loop {
            match self.process.rx.try_recv() {
                Ok(Reply::Ready(renderer)) => {
                    self.ready = true;
                    self.renderer = renderer;
                    self.pending = None;
                }
                Ok(Reply::Image(image)) => {
                    self.image = Some(image);
                    self.pending = None;
                }
                Ok(Reply::Failed(error)) => return Err(self.process.error(&error)),
                Err(TryRecvError::Disconnected) => {
                    return Err(self.process.error("renderer stopped"))
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if self.pending.is_some_and(|at| {
            at.elapsed()
                > if self.ready {
                    FRAME_TIMEOUT
                } else {
                    START_TIMEOUT
                }
        }) {
            return Err(self.process.error("renderer timed out"));
        }
        if self.ready && self.pending.is_none() && self.area.width > 0 && self.area.height > 0 {
            let (width, height) = match self.cell {
                Some(cell) => pixel_dimensions(self.area, cell),
                None => dimensions(self.area),
            };
            let aspect = width as f32 / height as f32;
            // The default view frames the whole 20 x 20 floor.
            let distance = (24. * (1.6 / aspect).max(1.) * self.zoom).min(96.);
            let camera = [
                TARGET[0] + distance * self.yaw.sin(),
                TARGET[1] + distance * self.tilt,
                TARGET[2] + distance * self.yaw.cos(),
            ];
            let request = Request {
                width,
                height,
                time: view.time,
                camera,
                objects: view.avatars,
                page: view.selected / PAGE_SIZE + 1,
                pages: view.total.max(1).div_ceil(PAGE_SIZE),
                bubbles: view.bubbles,
            };
            // Text changes need no GPU work. Picking still uses the identities
            // corresponding to the labels actually displayed with this image.
            if let Some(image) = &mut self.image {
                if image.request.same_visuals(&request) {
                    image.request = request;
                    return Ok(());
                }
            }
            // Mouse motion can wake the event loop much faster than drawing.
            if self.last_request.elapsed() >= Duration::from_millis(50) {
                self.process
                    .tx
                    .as_ref()
                    .ok_or("renderer stopped")?
                    .try_send(request)
                    .map_err(|error| self.process.error(&error.to_string()))?;
                self.pending = Some(Instant::now());
                self.last_request = Instant::now();
            }
        }
        Ok(())
    }

    /// After ratatui has drawn a frame: which scene cells it filled with text.
    pub fn note_coverage(&self, buffer: &Buffer) {
        if let Some(canvas) = self.canvas.borrow_mut().as_mut() {
            canvas.note_coverage(buffer);
        }
    }

    /// Writes the sixel image for the frame just drawn; a no-op without sixel.
    pub fn present(&self, out: &mut impl Write) -> std::io::Result<()> {
        let presented = self.presented.borrow();
        let mut canvas = self.canvas.borrow_mut();
        let (Some((_, image)), Some(canvas)) = (presented.as_ref(), canvas.as_mut()) else {
            return Ok(());
        };
        let Ok(Some(route)) = self.route else {
            // Inside tmux but not on screen: nothing may be drawn. Repaint when shown.
            canvas.refresh();
            return Ok(());
        };
        if matches!(route, sixel::Route::Tmux { .. })
            && self.repainted.get().elapsed() >= Duration::from_secs(2)
        {
            // tmux repaints its window for reasons the app never hears about.
            canvas.refresh();
            self.repainted.set(Instant::now());
        }
        canvas.present(
            &Pixels {
                rgb: &image.pixels,
                width: image.request.width,
                height: image.request.height,
            },
            route,
            out,
        )
    }

    /// Another screen is showing, so the terminal no longer holds the image.
    pub fn suspend(&self) {
        *self.canvas.borrow_mut() = None;
    }

    pub fn pick(&self, column: u16, row: u16) -> Option<Target> {
        let presented = self.presented.borrow();
        let (area, image) = presented.as_ref()?;
        image.pick(*area, column, row)
    }

    pub fn draw(&self, frame: &mut Frame<'_>, connection: &str, paused: bool, metrics: &str) {
        let terminal = frame.area();
        let area = scene_area(terminal);
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(BACKGROUND)),
            terminal,
        );
        if let Some(image) = &self.image {
            // Retain the exact camera, viewport and identities painted here.
            // A newer worker reply must not change picking before a redraw.
            *self.presented.borrow_mut() = Some((area, image.clone()));
            match self.cell {
                // Real pixels: ratatui leaves the scene's cells to the sixel image, but
                // only for an image rendered for exactly this area and cell size.
                Some(cell) if self.sixel => {
                    let size = pixel_dimensions(area, cell);
                    let mut canvas = self.canvas.borrow_mut();
                    if size != (image.request.width, image.request.height) {
                        *canvas = None;
                    } else {
                        if !canvas.as_ref().is_some_and(|c| c.fits(area, cell, size)) {
                            *canvas = Some(Canvas::new(area, cell, size));
                        }
                        let canvas = canvas.as_mut().expect("created above");
                        if WIPED.swap(false, Ordering::Relaxed) {
                            canvas.invalidate();
                        }
                        canvas.mark(frame.buffer_mut(), BACKGROUND);
                    }
                }
                _ => {
                    for row in 0..area.height {
                        for column in 0..area.width {
                            let (column, row) = (u32::from(column), u32::from(row));
                            let x = span(column, u32::from(area.width), image.request.width);
                            let (rows, height) = (u32::from(area.height) * 2, image.request.height);
                            frame.buffer_mut()[(area.x + column as u16, area.y + row as u16)]
                                .set_symbol("▀")
                                .set_fg(image.average(x, span(row * 2, rows, height)))
                                .set_bg(image.average(x, span(row * 2 + 1, rows, height)));
                        }
                    }
                }
            }
            for (label, index) in image.labels(area) {
                let avatar = &image.request.objects[index];
                let base = if avatar.selected {
                    Color::Yellow
                } else {
                    Color::White
                };
                frame.render_widget(Clear, label);
                frame.render_widget(
                    Paragraph::new(
                        avatar
                            .lines
                            .iter()
                            .map(|(text, color)| {
                                Line::styled(
                                    super::animated::clipped(text, label.width),
                                    Style::default().fg(color.unwrap_or(base)),
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                    .style(Style::default().bg(BACKGROUND)),
                    label,
                );
            }
        } else if area.height > 0 {
            frame.render_widget(
                Paragraph::new("Starting Panda3D GPU…  ·  F5/Esc returns")
                    .style(Style::default().fg(Color::White).bg(BACKGROUND)),
                area,
            );
        }
        let (page, pages) = self
            .image
            .as_ref()
            .map_or((1, 1), |image| (image.request.page, image.request.pages));
        let header = format!(
            " 3D Café / {connection} · page {page}/{pages}{} · {}{}",
            if paused { " · PAUSED" } else { "" },
            if self.ready {
                self.renderer.as_str()
            } else {
                "starting GPU"
            },
            match (self.sixel, self.cell, self.route) {
                (false, ..) => "".to_string(),
                (true, _, Err(reason)) => format!(" · half-block ({reason})"),
                (true, Some(_), _) => " · sixel".to_string(),
                (true, None, _) =>
                    " · half-block (sixel needs the terminal's pixel size)".to_string(),
            }
        );
        frame.render_widget(
            Paragraph::new(format!("{header}\n {metrics}"))
                .style(Style::default().fg(Color::White).bg(BACKGROUND)),
            Rect::new(
                terminal.x,
                terminal.y,
                terminal.width,
                terminal.height.min(2),
            ),
        );
        let footer = super::animated::footer_area(terminal);
        if footer.height > 0 {
            frame.render_widget(Paragraph::new(" h/l cats · j/k item · a quota · n add · e/d toggle · x delete · r/R refresh · ? keys · drag/[ ] orbit · +/- zoom · 0 reset · Space pause · b labels · F5 normal")
                .style(Style::default().fg(Color::Gray).bg(BACKGROUND)),
                Rect::new(footer.x, footer.bottom()-1, footer.width, 1));
        }
    }
}

fn scene_area(terminal: Rect) -> Rect {
    Rect::new(
        terminal.x,
        terminal.y + terminal.height.min(2),
        terminal.width,
        terminal.height.saturating_sub(6),
    )
}

/// The terminal's cell size in pixels, if it reports one and a frame covering the
/// scene at that size fits what the renderer can produce.
fn cell_pixels(area: Rect) -> Option<(u32, u32)> {
    let size = crossterm::terminal::window_size().ok()?;
    fit_cell(size.columns, size.rows, size.width, size.height, area)
}

fn fit_cell(columns: u16, rows: u16, width: u16, height: u16, area: Rect) -> Option<(u32, u32)> {
    if columns == 0 || rows == 0 || area.width == 0 || area.height == 0 {
        return None;
    }
    let (cw, ch) = (u32::from(width / columns), u32::from(height / rows));
    let fits = u32::from(area.width) * cw <= MAX_WIDTH && u32::from(area.height) * ch <= MAX_HEIGHT;
    (cw > 0 && ch > 0 && fits).then_some((cw, ch))
}

/// Render size for the pixels of `area`, one render pixel per screen pixel. The
/// renderer needs an even height; the last pixel row of the area repeats.
fn pixel_dimensions(area: Rect, (cw, ch): (u32, u32)) -> (u32, u32) {
    (
        u32::from(area.width) * cw,
        (u32::from(area.height) * ch).max(2) & !1,
    )
}

/// Orbit freely around the café; the tilt stays between a low and a high view.
fn orbited(yaw: f32, tilt: f32, orbit: f32, lift: f32) -> (f32, f32) {
    let pi = std::f32::consts::PI;
    (
        (yaw + orbit + pi).rem_euclid(2. * pi) - pi,
        (tilt + lift).clamp(0.12, 2.),
    )
}

/// Render size: the terminal's half-block pixel grid, capped, times `SUPERSAMPLE`.
fn dimensions(area: Rect) -> (u32, u32) {
    let width = u32::from(area.width).max(1);
    let height = (u32::from(area.height) * 2).max(2);
    let scale = ((MAX_WIDTH / SUPERSAMPLE) as f32 / width as f32)
        .min((MAX_HEIGHT / SUPERSAMPLE) as f32 / height as f32)
        .min(1.);
    (
        (width as f32 * scale).floor().max(1.) as u32 * SUPERSAMPLE,
        ((height as f32 * scale).floor().max(2.) as u32 / 2) * 2 * SUPERSAMPLE,
    )
}

/// The `index`-th of `count` equal slices of `size` pixels; never empty.
fn span(index: u32, count: u32, size: u32) -> (u32, u32) {
    let start = index * size / count;
    (start, ((index + 1) * size / count).clamp(start + 1, size))
}

impl Image {
    fn average(&self, x: (u32, u32), y: (u32, u32)) -> Color {
        let (mut sum, mut count) = ([0u32; 3], 0);
        for row in y.0..y.1 {
            for column in x.0..x.1 {
                let at = ((row * self.request.width + column) * 3) as usize;
                for (total, value) in sum.iter_mut().zip(&self.pixels[at..at + 3]) {
                    *total += u32::from(*value);
                }
                count += 1;
            }
        }
        Color::Rgb(
            (sum[0] / count) as u8,
            (sum[1] / count) as u8,
            (sum[2] / count) as u8,
        )
    }

    fn labels(&self, area: Rect) -> Vec<(Rect, usize)> {
        let mut labels: Vec<(Rect, usize)> = Vec::new();
        if !self.request.bubbles || area.width == 0 || area.height < 2 {
            return labels;
        }
        let width = (area.width / 3).clamp(12, 28).min(area.width);
        // Place the focused label first. Other labels move upward in bounded
        // steps instead of painting over a neighbor at oblique camera angles.
        let objects = &self.request.objects;
        let order = objects
            .iter()
            .enumerate()
            .filter(|(_, object)| object.selected)
            .chain(
                objects
                    .iter()
                    .enumerate()
                    .filter(|(_, object)| !object.selected),
            );
        for (index, avatar) in order {
            let point = [
                avatar.position[0],
                avatar.position[1] + 1.75,
                avatar.position[2],
            ];
            let Some((x, y)) = self.project(area, point) else {
                continue;
            };
            let left = (x - i32::from(width / 2))
                .clamp(i32::from(area.x), i32::from(area.right() - width))
                as u16;
            // The label sits above the head, growing upward with its rows.
            let height = (avatar.lines.len() as u16).clamp(1, area.height);
            let top = if avatar.selected {
                (y - i32::from(height)).clamp(i32::from(area.y), i32::from(area.bottom() - height))
            } else {
                y - i32::from(height)
            };
            for step in 0..=PAGE_SIZE {
                let row = top - step as i32 * (i32::from(height) + 1);
                if row < i32::from(area.y) {
                    break;
                }
                if row + i32::from(height) > i32::from(area.bottom()) {
                    continue;
                }
                let rect = Rect::new(left, row as u16, width, height);
                if labels.iter().all(|(other, _)| !rect.intersects(*other)) {
                    labels.push((rect, index));
                    break;
                }
            }
        }
        labels
    }

    fn basis(&self) -> ([f32; 3], [f32; 3], [f32; 3]) {
        let forward = normal(sub(TARGET, self.request.camera));
        let right = normal(cross(forward, [0., 1., 0.]));
        (forward, right, cross(right, forward))
    }

    fn project(&self, area: Rect, point: [f32; 3]) -> Option<(i32, i32)> {
        let (forward, right, up) = self.basis();
        let delta = sub(point, self.request.camera);
        let depth = dot(delta, forward);
        if depth <= 0. {
            return None;
        }
        let scale = 20f32.to_radians().tan();
        let aspect = self.request.width as f32 / self.request.height as f32;
        Some((
            i32::from(area.x)
                + ((dot(delta, right) / (depth * scale * aspect) + 1.) * 0.5 * area.width as f32)
                    as i32,
            i32::from(area.y)
                + ((1. - dot(delta, up) / (depth * scale)) * 0.5 * area.height as f32) as i32,
        ))
    }

    fn pick(&self, area: Rect, column: u16, row: u16) -> Option<Target> {
        if !area.contains((column, row).into()) || area.width == 0 || area.height == 0 {
            return None;
        }
        if let Some((_, index)) = self
            .labels(area)
            .iter()
            .find(|(rect, _)| rect.contains((column, row).into()))
        {
            return Some(self.request.objects[*index].target.clone());
        }
        let (forward, right, up) = self.basis();
        let scale = 20f32.to_radians().tan();
        let aspect = self.request.width as f32 / self.request.height as f32;
        let x =
            (2. * (f32::from(column - area.x) + 0.5) / f32::from(area.width) - 1.) * scale * aspect;
        let y = (1. - 2. * (f32::from(row - area.y) + 0.5) / f32::from(area.height)) * scale;
        let ray = normal([
            forward[0] + right[0] * x + up[0] * y,
            forward[1] + right[1] * x + up[1] * y,
            forward[2] + right[2] * x + up[2] * y,
        ]);
        self.request
            .objects
            .iter()
            .filter_map(|avatar| {
                let p = avatar.position;
                ray_box(
                    self.request.camera,
                    ray,
                    [p[0] - 0.6, p[1], p[2] - 0.6],
                    [p[0] + 0.6, p[1] + 1.5, p[2] + 0.6],
                )
                .map(|distance| (distance, &avatar.target))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, target)| target.clone())
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn normal(a: [f32; 3]) -> [f32; 3] {
    let length = dot(a, a).sqrt().max(f32::EPSILON);
    [a[0] / length, a[1] / length, a[2] / length]
}
fn ray_box(origin: [f32; 3], ray: [f32; 3], min: [f32; 3], max: [f32; 3]) -> Option<f32> {
    let (mut near, mut far) = (0f32, f32::INFINITY);
    for axis in 0..3 {
        if ray[axis].abs() < 1e-7 {
            if origin[axis] < min[axis] || origin[axis] > max[axis] {
                return None;
            }
        } else {
            let a = (min[axis] - origin[axis]) / ray[axis];
            let b = (max[axis] - origin[axis]) / ray[axis];
            near = near.max(a.min(b));
            far = far.min(a.max(b));
        }
    }
    (far >= near).then_some(near)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn request() -> Request {
        Request {
            width: 160,
            height: 100,
            time: 0.,
            camera: [0., 5.6, 14.],
            objects: vec![Avatar {
                target: Target::Account {
                    provider: "codex".into(),
                    key: "stable-key".into(),
                },
                lines: vec![("Demo".into(), None), ("25%".into(), None)],
                position: [0., 0.25, 4.2],
                color: [1.; 3],
                status: [0.; 3],
                selected: true,
                models: false,
                heading: 0.,
                gait: 0.,
                stride: 0.,
            }],
            page: 1,
            pages: 1,
            bubbles: true,
        }
    }

    #[test]
    fn protocol_rejects_unbounded_and_mismatched_images_before_allocation() {
        for (width, height, length) in [
            (640u32, 400u32, 768000u32),
            (160, 100, u32::MAX),
            (160, 0, 0),
        ] {
            let mut bytes = b"RGB1".to_vec();
            for value in [width, height, length] {
                bytes.extend(value.to_le_bytes());
            }
            assert!(read_image(&mut Cursor::new(bytes), &request()).is_err());
        }
        assert!(read_ready(&mut Cursor::new(b"P3D1\xff\xff\xff\xff")).is_err());
        let greeting = br#"{"gpu":false,"renderer":"llvmpipe"}"#;
        let mut bytes = b"P3D1".to_vec();
        bytes.extend((greeting.len() as u32).to_le_bytes());
        bytes.extend(greeting);
        assert!(read_ready(&mut Cursor::new(bytes)).is_err());
    }

    #[test]
    fn projection_picks_displayed_identity_after_resize_and_orbit() {
        for camera in [[0., 5.6, 14.], [4., 6., 14.], [-4., 6., 14.]] {
            let mut request = request();
            request.camera = camera;
            let image = Image {
                request,
                pixels: Arc::new(vec![]),
            };
            for area in [Rect::new(0, 2, 160, 50), Rect::new(3, 4, 80, 22)] {
                let (x, y) = image.project(area, [0., 1., 4.2]).unwrap();
                assert_eq!(
                    image.pick(area, x as u16, y as u16),
                    Some(Target::Account {
                        provider: "codex".into(),
                        key: "stable-key".into()
                    })
                );
                assert_eq!(image.pick(area, 0, 0), None);
            }
        }
    }

    #[test]
    fn parallel_rays_and_closest_intersection_are_well_defined() {
        assert_eq!(
            ray_box([0., 0., 2.], [0., 0., -1.], [-1.; 3], [1.; 3]),
            Some(1.)
        );
        assert_eq!(
            ray_box([2., 0., 2.], [0., 0., -1.], [-1.; 3], [1.; 3]),
            None
        );
        assert_eq!(ray_box([0., 0., 2.], [0., 0., 1.], [-1.; 3], [1.; 3]), None);
    }

    #[test]
    fn rendering_is_bounded_at_large_and_tiny_terminal_sizes() {
        for area in [
            Rect::new(0, 0, u16::MAX, u16::MAX),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 320, 98),
        ] {
            let (width, height) = dimensions(area);
            assert!((1..=MAX_WIDTH).contains(&width));
            assert!((2..=MAX_HEIGHT).contains(&height));
            assert_eq!(height % 2, 0);
        }
        assert_eq!(scene_area(Rect::new(0, 0, 1, 1)).height, 0);
        assert_eq!(
            dimensions(Rect::new(0, 0, 100, 20)),
            (100 * SUPERSAMPLE, 40 * SUPERSAMPLE)
        );
    }

    #[test]
    fn sixel_needs_a_reported_cell_size_and_a_frame_the_renderer_can_make() {
        let area = Rect::new(0, 2, 159, 63);
        // foot at 6 x 15 px inside tmux: the pixel size of a 159 x 68 pane.
        assert_eq!(fit_cell(159, 68, 954, 1020, area), Some((6, 15)));
        assert_eq!(pixel_dimensions(area, (6, 15)), (954, 944));
        // A terminal that does not report pixels, or a pane too big to render.
        assert_eq!(fit_cell(159, 68, 0, 0, area), None);
        assert_eq!(fit_cell(0, 0, 954, 1020, area), None);
        assert_eq!(fit_cell(159, 68, 159 * 14, 68 * 30, area), None);
        assert_eq!(fit_cell(159, 68, 954, 1020, Rect::new(0, 0, 0, 0)), None);
    }

    #[test]
    fn orbit_wraps_all_the_way_around_and_tilt_stays_in_range() {
        let pi = std::f32::consts::PI;
        let (yaw, tilt) = orbited(pi - 0.1, 0.5, 0.3, 0.);
        assert!((yaw - (-pi + 0.2)).abs() < 1e-5, "{yaw}");
        assert_eq!(tilt, 0.5);
        assert_eq!(orbited(0., 0.5, 0., 10.).1, 2.);
        assert_eq!(orbited(0., 0.5, 0., -10.).1, 0.12);
    }

    #[test]
    fn supersampled_pixels_average_into_each_half_block() {
        // One cell is two half-block pixels; each covers a 2 x 2 block of render pixels.
        let mut pixels = vec![0; 2 * 4 * 3];
        for (index, value) in [(0, 200), (1, 0), (2, 0), (3, 200)] {
            let (x, y) = (index % 2, index / 2);
            pixels[(y * 2 + x) * 3..][..3].fill(value);
        }
        let mut request = request();
        (request.width, request.height) = (2, 4);
        let image = Image {
            request,
            pixels: Arc::new(pixels),
        };
        let (x, rows) = (span(0, 1, 2), 2);
        assert_eq!(
            image.average(x, span(0, rows, 4)),
            Color::Rgb(100, 100, 100)
        );
        assert_eq!(image.average(x, span(1, rows, 4)), Color::Rgb(0, 0, 0));
        // A frame smaller than the area still yields a non-empty sample.
        assert_eq!(span(9, 10, 3), (2, 3));
    }

    #[test]
    fn patrol_territories_never_bring_two_cats_together() {
        let samples = |route: &[[f32; 2]]| {
            let mut points = Vec::new();
            for (i, a) in route.iter().enumerate() {
                let b = route[(i + 1) % route.len()];
                let steps = ((b[0] - a[0]).hypot(b[1] - a[1]) / 0.1).ceil() as usize;
                points.extend((0..=steps).map(|k| {
                    let t = k as f32 / steps as f32;
                    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
                }));
            }
            points
        };
        let mut territories: Vec<_> = ROUTES.iter().map(|route| samples(route)).collect();
        territories.push(vec![BARISTA]);
        for (i, a) in territories.iter().enumerate() {
            for (j, b) in territories.iter().enumerate().skip(i + 1) {
                let nearest = a
                    .iter()
                    .flat_map(|p| b.iter().map(move |q| (p[0] - q[0]).hypot(p[1] - q[1])))
                    .fold(f32::MAX, f32::min);
                assert!(
                    nearest >= MIN_GAP,
                    "territories {i} and {j} come within {nearest}"
                );
            }
        }
    }

    #[test]
    fn cats_patrol_the_floor_smoothly_and_the_barista_stays_put() {
        let mut moved = 0f32;
        for slot in 0..16 {
            let mut last = walk(slot, 0.);
            for step in 1..=2400 {
                let now = walk(slot, step as f32 * 0.05);
                let [x, z] = now.position;
                assert!(
                    x.abs() <= 10. && z.abs() <= 10.,
                    "slot {slot} left the floor"
                );
                assert!((0. ..=1.).contains(&now.stride) && now.gait.is_finite());
                // Peak eased speed is 1.5x the average; allow no jumps beyond that.
                let jump = (x - last.position[0]).hypot(z - last.position[1]);
                assert!(
                    jump <= WALK_SPEED * 1.5 * 0.05 + 1e-3,
                    "slot {slot} jumped {jump}"
                );
                if slot == 0 {
                    assert_eq!(now.position, BARISTA);
                } else if slot == 1 {
                    moved += jump;
                }
                last = now;
            }
        }
        assert!(moved > 30., "walking cat covered only {moved}");
        assert_eq!(walk(3, 12.5).position, walk(3, 12.5).position);
    }

    #[test]
    fn labels_do_not_overlap_and_click_the_same_displayed_account() {
        let mut request = request();
        let original = request.objects[0].clone();
        request.objects = (0..7)
            .map(|index| {
                let mut avatar = original.clone();
                avatar.position[0] = (index as f32 - 3.) * 1.3;
                avatar.target = Target::Account {
                    provider: "codex".into(),
                    key: format!("account-{index}"),
                };
                avatar.selected = index == 3;
                avatar
            })
            .collect();
        let image = Image {
            request,
            pixels: Arc::new(vec![]),
        };
        for area in [
            Rect::new(0, 2, 180, 36),
            Rect::new(0, 2, 80, 18),
            Rect::new(0, 2, 320, 98),
        ] {
            let labels = image.labels(area);
            assert!(!labels.is_empty());
            assert!(labels.iter().any(|(_, index)| *index == 3));
            for (i, (rect, index)) in labels.iter().enumerate() {
                assert_eq!(
                    image.pick(area, rect.x, rect.y),
                    Some(image.request.objects[*index].target.clone())
                );
                assert!(labels
                    .iter()
                    .skip(i + 1)
                    .all(|(other, _)| !rect.intersects(*other)));
            }
        }
    }

    #[test]
    fn text_updates_do_not_require_gpu_work_or_send_account_identities_to_python() {
        let original = request();
        let mut changed = original.clone();
        changed.objects[0].lines = vec![
            ("updated@example.com".into(), None),
            ("50%".into(), Some(Color::Red)),
        ];
        assert!(original.same_visuals(&changed));
        let json = serde_json::to_string(&changed).unwrap();
        for private in ["updated@example.com", "stable-key", "50%", "target"] {
            assert!(!json.contains(private));
        }
        changed.objects[0].selected = false;
        assert!(!original.same_visuals(&changed));
    }

    #[cfg(unix)]
    #[test]
    fn picking_keeps_the_last_painted_identity_until_the_next_draw() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let request = request();
        let image = Image {
            pixels: Arc::new(vec![0; (request.width * request.height * 3) as usize]),
            request,
        };
        let mut scene = Scene {
            process: Process::spawn(command).unwrap(),
            ready: true,
            renderer: "test GPU".into(),
            pending: None,
            image: Some(image.clone()),
            presented: RefCell::new(None),
            area: Rect::default(),
            yaw: DEFAULT_YAW,
            tilt: DEFAULT_TILT,
            zoom: 1.,
            last_request: Instant::now(),
            sixel: false,
            cell: None,
            route: Ok(Some(sixel::Route::Direct)),
            route_checked: Instant::now(),
            repainted: Shared::new(Instant::now()),
            canvas: RefCell::new(None),
        };
        let mut terminal = Terminal::new(TestBackend::new(180, 42)).unwrap();
        let area = scene_area(Rect::new(0, 0, 180, 42));
        let (x, y) = image.project(area, [0., 1., 4.2]).unwrap();
        assert_eq!(scene.pick(x as u16, y as u16), None);
        terminal
            .draw(|frame| scene.draw(frame, "test", true, ""))
            .unwrap();

        let original = image.request.objects[0].target.clone();
        let replacement = Target::Account {
            provider: "claude".into(),
            key: "next-frame-account".into(),
        };
        scene.image.as_mut().unwrap().request.objects[0].target = replacement.clone();
        scene.area = Rect::new(10, 10, 40, 8);
        assert_eq!(scene.pick(x as u16, y as u16), Some(original));
        terminal
            .draw(|frame| scene.draw(frame, "test", true, ""))
            .unwrap();
        assert_eq!(scene.pick(x as u16, y as u16), Some(replacement));
    }

    #[cfg(unix)]
    #[test]
    fn stalled_helper_is_killed_and_reaped_without_waiting_for_readiness() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let started = Instant::now();
        let process = Process::spawn(command).unwrap();
        drop(process);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
