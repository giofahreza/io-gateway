//! Optional local-machine telemetry. No reads or subprocesses on the TUI thread.
//! Missing counters stay unknown; rate counters require two successful samples.
#[cfg(target_os = "linux")]
use std::fs;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};

#[cfg(target_os = "linux")]
#[path = "intel_gpu.rs"]
mod intel_gpu;
#[cfg(target_os = "macos")]
#[path = "macos_metrics.rs"]
mod macos_metrics;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum GpuSource {
    #[default]
    Unknown,
    Drm,
    Nvidia,
    IntelPmu,
    IntelClients,
    MacIoreg,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Snapshot {
    pub cpu: Option<f64>,
    pub memory: Option<(u64, u64)>,
    pub disk: Option<(u64, u64)>,
    pub disk_io: Option<(f64, f64)>,
    pub network: Option<(f64, f64)>,
    pub interface: String,
    pub gpu: Option<f64>,
    pub gpu_source: GpuSource,
    pub vram: Option<(u64, u64)>,
    pub sampled_at: Option<Instant>,
    /// One point per sampled second, oldest first; None is an honest gap.
    pub history: VecDeque<[Option<f64>; 3]>,
}

#[derive(Default)]
pub(super) struct Monitor {
    task: Option<JoinHandle<()>>,
    rx: Option<watch::Receiver<Snapshot>>,
    pub snapshot: Snapshot,
}

impl Monitor {
    pub fn start(&mut self) {
        if self.task.is_some() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (tx, rx) = watch::channel(Snapshot::default());
        self.rx = Some(rx);
        self.task = Some(runtime.spawn(async move {
            let mut sampler = Sampler::default();
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut nvidia_available = true;
            loop {
                interval.tick().await;
                let Ok((next, mut snapshot)) = tokio::task::spawn_blocking(move || {
                    let snapshot = sampler.sample();
                    (sampler, snapshot)
                })
                .await
                else {
                    break;
                };
                sampler = next;
                if snapshot.gpu.is_none() && nvidia_available {
                    match nvidia().await {
                        Ok((gpu, vram)) => {
                            // Retain standalone NVIDIA VRAM when utilization is
                            // unsupported, but never mix it with Intel counters.
                            if gpu.is_some() || snapshot.gpu_source == GpuSource::Unknown {
                                snapshot.gpu = gpu;
                                snapshot.vram = vram;
                                if gpu.is_some() || vram.is_some() {
                                    snapshot.gpu_source = GpuSource::Nvidia;
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            nvidia_available = false
                        }
                        Err(_) => {}
                    }
                }
                if tx.send(snapshot).is_err() {
                    break;
                }
            }
        }));
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &mut self.rx {
            if rx.has_changed().unwrap_or(false) {
                let mut next = rx.borrow_and_update().clone();
                if next.sampled_at != self.snapshot.sampled_at {
                    next.history = std::mem::take(&mut self.snapshot.history);
                    if let (Some(old), Some(new)) = (self.snapshot.sampled_at, next.sampled_at) {
                        let missing = new
                            .saturating_duration_since(old)
                            .as_secs()
                            .saturating_sub(1)
                            .min(120);
                        next.history
                            .extend(std::iter::repeat_n([None; 3], missing as usize));
                    }
                    next.history.push_back([
                        next.cpu,
                        next.gpu,
                        next.memory
                            .filter(|(_, total)| *total > 0)
                            .map(|(used, total)| used as f64 * 100.0 / total as f64),
                    ]);
                    while next.history.len() > 120 {
                        next.history.pop_front();
                    }
                } else {
                    next.history = std::mem::take(&mut self.snapshot.history);
                }
                self.snapshot = next;
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.rx = None;
        self.snapshot = Snapshot::default();
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct Sampler {
    cpu: Option<(u64, u64)>,
    network: Option<(String, u64, u64, Instant)>,
    disk_io: Option<(u64, u64, Instant)>,
    #[cfg(target_os = "linux")]
    intel: intel_gpu::IntelGpu,
}

impl Sampler {
    fn sample(&mut self) -> Snapshot {
        #[cfg(target_os = "linux")]
        {
            let now = Instant::now();
            let current_cpu = fs::read_to_string("/proc/stat")
                .ok()
                .and_then(|s| cpu_ticks(&s));
            let cpu = self
                .cpu
                .zip(current_cpu)
                .and_then(|(old, new)| cpu_percent(old, new));
            self.cpu = current_cpu;
            let memory = fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|s| memory_bytes(&s));
            let interface = default_interface().unwrap_or_default();
            let current_net = fs::read_to_string("/proc/net/dev")
                .ok()
                .and_then(|s| network_bytes(&s, &interface));
            let network = self.network.as_ref().zip(current_net).and_then(
                |((old_interface, rx, tx, at), (new_rx, new_tx))| {
                    (old_interface == &interface)
                        .then(|| rates((*rx, *tx), (new_rx, new_tx), now.duration_since(*at)))
                        .flatten()
                },
            );
            self.network = current_net.map(|(rx, tx)| (interface.clone(), rx, tx, now));
            let current_disk = disk_counters();
            let disk_io = self
                .disk_io
                .zip(current_disk)
                .and_then(|((r, w, at), current)| rates((r, w), current, now.duration_since(at)));
            self.disk_io = current_disk.map(|(r, w)| (r, w, now));
            let (mut gpu, vram) = drm_gpu();
            let mut gpu_source = if gpu.is_some() {
                GpuSource::Drm
            } else {
                GpuSource::Unknown
            };
            if gpu.is_none() {
                if let Some(sample) = self.intel.sample() {
                    gpu = sample.usage;
                    gpu_source = if sample.system_wide {
                        GpuSource::IntelPmu
                    } else {
                        GpuSource::IntelClients
                    };
                }
            }
            Snapshot {
                history: VecDeque::new(),
                cpu,
                memory,
                disk: root_disk(),
                disk_io,
                network,
                interface,
                gpu,
                gpu_source,
                vram,
                sampled_at: Some(now),
            }
        }
        #[cfg(target_os = "macos")]
        {
            let now = Instant::now();
            let cpu = macos_metrics::cpu_percent();
            let memory = macos_metrics::memory_bytes();
            let interface = macos_metrics::default_interface().unwrap_or_default();
            let current_net = macos_metrics::network_bytes(&interface);
            let network = self.network.as_ref().zip(current_net).and_then(
                |((old_interface, rx, tx, at), (new_rx, new_tx))| {
                    (old_interface == &interface)
                        .then(|| rates((*rx, *tx), (new_rx, new_tx), now.duration_since(*at)))
                        .flatten()
                },
            );
            self.network = current_net.map(|(rx, tx)| (interface.clone(), rx, tx, now));
            let current_disk = macos_metrics::disk_counters();
            let disk_io = self
                .disk_io
                .zip(current_disk)
                .and_then(|((r, w, at), current)| rates((r, w), current, now.duration_since(at)));
            self.disk_io = current_disk.map(|(r, w)| (r, w, now));
            let (gpu, vram) = macos_metrics::gpu();
            Snapshot {
                history: VecDeque::new(),
                cpu,
                memory,
                disk: macos_metrics::root_disk(),
                disk_io,
                network,
                interface,
                gpu,
                gpu_source: gpu.map_or(GpuSource::Unknown, |_| GpuSource::MacIoreg),
                vram,
                sampled_at: Some(now),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Snapshot {
                sampled_at: Some(Instant::now()),
                ..Snapshot::default()
            }
        }
    }
}

fn cpu_ticks(text: &str) -> Option<(u64, u64)> {
    let mut words = text.lines().next()?.split_whitespace();
    if words.next()? != "cpu" {
        return None;
    }
    // guest/guest_nice are already included in user/nice; do not double count.
    let values = words
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if values.len() < 4 {
        return None;
    }
    Some((
        values
            .iter()
            .try_fold(0u64, |sum, value| sum.checked_add(*value))?,
        values[3].checked_add(*values.get(4).unwrap_or(&0))?,
    ))
}

fn cpu_percent(old: (u64, u64), new: (u64, u64)) -> Option<f64> {
    let total = new.0.checked_sub(old.0)?;
    let idle = new.1.checked_sub(old.1)?;
    (total > 0 && idle <= total).then(|| (total - idle) as f64 * 100.0 / total as f64)
}

fn memory_bytes(text: &str) -> Option<(u64, u64)> {
    let read = |key| {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name == key)
                .then(|| {
                    value
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()?
                        .checked_mul(1024)
                })
                .flatten()
        })
    };
    let total = read("MemTotal")?;
    let available = read("MemAvailable")?;
    (total > 0 && available <= total).then_some((total - available, total))
}

fn network_bytes(text: &str, interface: &str) -> Option<(u64, u64)> {
    if interface.is_empty() || interface == "lo" {
        return None;
    }
    text.lines().find_map(|line| {
        let (name, counters) = line.split_once(':')?;
        if name.trim() != interface {
            return None;
        }
        let values = counters.split_whitespace().collect::<Vec<_>>();
        Some((values.first()?.parse().ok()?, values.get(8)?.parse().ok()?))
    })
}

fn rates(old: (u64, u64), new: (u64, u64), elapsed: Duration) -> Option<(f64, f64)> {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return None;
    }
    Some((
        new.0.checked_sub(old.0)? as f64 / seconds,
        new.1.checked_sub(old.1)? as f64 / seconds,
    ))
}

#[cfg(target_os = "linux")]
fn default_interface() -> Option<String> {
    if let Ok(routes) = fs::read_to_string("/proc/net/route") {
        if let Some(interface) = routes.lines().skip(1).find_map(|line| {
            let parts = line.split_whitespace().collect::<Vec<_>>();
            let flags = u32::from_str_radix(parts.get(3)?, 16).ok()?;
            (parts.get(1)? == &"00000000" && flags & 3 == 3).then(|| parts[0].to_string())
        }) {
            return Some(interface);
        }
    }
    // IPv6-only machines may not have an IPv4 default route.
    if let Ok(routes) = fs::read_to_string("/proc/net/ipv6_route") {
        if let Some(interface) = routes.lines().find_map(|line| {
            let parts = line.split_whitespace().collect::<Vec<_>>();
            (parts.first()? == &"00000000000000000000000000000000"
                && parts.get(1)? == &"00"
                && *parts.last()? != "lo")
                .then(|| parts.last().unwrap().to_string())
        }) {
            return Some(interface);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn root_disk() -> Option<(u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // Constant NUL-terminated path, valid output pointer; inspect only on success.
    if unsafe { libc::statvfs(c"/".as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let total = (stat.f_blocks as u64).checked_mul(stat.f_frsize as u64)?;
    let free = (stat.f_bfree as u64).checked_mul(stat.f_frsize as u64)?;
    (total > 0 && free <= total).then_some((total - free, total))
}

#[cfg(target_os = "linux")]
fn disk_counters() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/diskstats").ok()?;
    let mut totals: Option<(u64, u64)> = None;
    for line in text.lines() {
        let values = line.split_whitespace().collect::<Vec<_>>();
        let Some(name) = values.get(2) else {
            continue;
        };
        // Real whole block devices only: avoid partitions, dm/md and loop
        // devices double-counting the same I/O. Linux sectors are 512 bytes.
        if !std::path::Path::new("/sys/block")
            .join(name)
            .join("device")
            .exists()
        {
            continue;
        }
        let read = values.get(5)?.parse::<u64>().ok()?.checked_mul(512)?;
        let written = values.get(9)?.parse::<u64>().ok()?.checked_mul(512)?;
        let (old_read, old_written) = totals.unwrap_or_default();
        totals = Some((
            old_read.checked_add(read)?,
            old_written.checked_add(written)?,
        ));
    }
    totals
}

#[cfg(target_os = "linux")]
fn drm_gpu() -> (Option<f64>, Option<(u64, u64)>) {
    let Ok(cards) = fs::read_dir("/sys/class/drm") else {
        return (None, None);
    };
    let mut cards = cards
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    cards.sort();
    for card in cards {
        let name = card.file_name().unwrap_or_default().to_string_lossy();
        if !name
            .strip_prefix("card")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        let read = |name| {
            fs::read_to_string(card.join("device").join(name))
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        };
        if let Some(busy) = read("gpu_busy_percent").filter(|busy| *busy <= 100) {
            let vram = read("mem_info_vram_used")
                .zip(read("mem_info_vram_total"))
                .filter(|(used, total)| *total > 0 && used <= total);
            return (Some(busy as f64), vram);
        }
    }
    (None, None)
}

async fn nvidia() -> std::io::Result<(Option<f64>, Option<(u64, u64)>)> {
    let mut command = tokio::process::Command::new("nvidia-smi");
    command
        .args([
            "--query-gpu=utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let output = tokio::time::timeout(Duration::from_millis(750), command.output())
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "GPU sample timeout"))??;
    if !output.status.success() {
        return Err(std::io::Error::other("GPU sample unavailable"));
    }
    Ok(parse_nvidia(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_nvidia(text: &str) -> (Option<f64>, Option<(u64, u64)>) {
    let fields = text
        .lines()
        .next()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>();
    let gpu = fields
        .first()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value));
    let vram = fields
        .get(1)
        .and_then(|value| value.parse::<u64>().ok())
        .zip(fields.get(2).and_then(|value| value.parse::<u64>().ok()))
        .and_then(|(used, total)| {
            Some((
                used.checked_mul(1024 * 1024)?,
                total.checked_mul(1024 * 1024)?,
            ))
        })
        .filter(|(used, total)| *total > 0 && used <= total);
    (gpu, vram)
}

pub(super) fn bytes(value: f64) -> String {
    if value >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}G", value / (1024.0 * 1024.0 * 1024.0))
    } else if value >= 1024.0 * 1024.0 {
        format!("{:.1}M", value / (1024.0 * 1024.0))
    } else if value >= 1024.0 {
        format!("{:.1}K", value / 1024.0)
    } else {
        format!("{value:.0}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn monitor_samples_off_thread_and_stops_cleanly() {
        let mut monitor = Monitor::default();
        monitor.start();
        monitor.start(); // Starting twice must not add another worker.
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                monitor.poll();
                if monitor.snapshot.cpu.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("CPU rate arrives after two samples");
        assert!(monitor.snapshot.memory.is_some());
        assert!(monitor.snapshot.disk.is_some());
        monitor.stop();
        assert!(monitor.task.is_none());
        assert!(monitor.snapshot.sampled_at.is_none());
    }
    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn monitor_samples_macos_hardware() {
        let mut monitor = Monitor::default();
        monitor.start();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                monitor.poll();
                if monitor.snapshot.cpu.is_some()
                    && monitor.snapshot.memory.is_some()
                    && monitor.snapshot.disk.is_some()
                    && monitor.snapshot.disk_io.is_some()
                    && monitor.snapshot.network.is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("macOS metrics arrive after two samples");
        monitor.stop();
        assert!(monitor.snapshot.sampled_at.is_none());
    }
    #[test]
    fn cpu_ignores_guest_and_rates_never_fabricate_resets() {
        assert_eq!(cpu_ticks("cpu 10 2 3 40 5 1 2 3 100 100"), Some((66, 45)));
        assert_eq!(cpu_percent((100, 50), (200, 75)), Some(75.0));
        assert_eq!(cpu_percent((100, 50), (90, 55)), None);
        assert_eq!(
            rates((100, 200), (300, 500), Duration::from_secs(2)),
            Some((100.0, 150.0))
        );
        assert_eq!(rates((100, 200), (0, 500), Duration::from_secs(2)), None);
    }
    #[test]
    fn memory_uses_available_and_network_selects_one_interface() {
        assert_eq!(
            memory_bytes("MemTotal: 1000 kB\nMemAvailable: 250 kB\nMemFree: 10 kB"),
            Some((750 * 1024, 1000 * 1024))
        );
        assert_eq!(memory_bytes("MemTotal: 1000 kB"), None);
        let text = "lo: 999 0 0 0 0 0 0 0 999 0\neth0: 10 0 0 0 0 0 0 0 20 0\nveth0: 30 0 0 0 0 0 0 0 40 0";
        assert_eq!(network_bytes(text, "eth0"), Some((10, 20)));
        assert_eq!(network_bytes(text, "lo"), None);
    }
    #[test]
    fn unsupported_gpu_stays_unknown_and_vram_is_separate() {
        assert_eq!(parse_nvidia("[N/A], [Not Supported], 8192"), (None, None));
        assert_eq!(parse_nvidia("24, 1024, 8192\n90, 2000, 9000").0, Some(24.0));
        assert_eq!(parse_nvidia("NaN, 1, 2").0, None);
        assert_eq!(parse_nvidia("110, 100, 50"), (None, None));
    }
    #[test]
    fn histories_are_bounded_sampled_once_and_preserve_unknowns_and_gaps() {
        let (tx, rx) = watch::channel(Snapshot::default());
        let mut monitor = Monitor::default();
        monitor.rx = Some(rx);
        let start = Instant::now();
        for second in 0..200 {
            tx.send(Snapshot {
                sampled_at: Some(start + Duration::from_secs(second)),
                cpu: Some(42.0),
                gpu: None,
                memory: Some((8, 16)),
                ..Default::default()
            })
            .unwrap();
            monitor.poll();
            for _ in 0..20 {
                monitor.poll();
            }
            assert_eq!(
                monitor.snapshot.history.len(),
                (second as usize + 1).min(120)
            );
            assert_eq!(
                monitor.snapshot.history.back(),
                Some(&[Some(42.0), None, Some(50.0)])
            );
        }
        // Even a re-published timestamp must not produce another chart point.
        tx.send(monitor.snapshot.clone()).unwrap();
        monitor.poll();
        assert_eq!(monitor.snapshot.history.len(), 120);
        tx.send(Snapshot {
            sampled_at: Some(start + Duration::from_secs(203)),
            cpu: Some(0.0),
            ..Default::default()
        })
        .unwrap();
        monitor.poll();
        assert_eq!(monitor.snapshot.history.len(), 120);
        for index in 116..119 {
            assert_eq!(monitor.snapshot.history[index], [None; 3]);
        }
        assert_eq!(monitor.snapshot.history[119], [Some(0.0), None, None]);
        monitor.stop();
        assert!(monitor.snapshot.history.is_empty());
    }
}
