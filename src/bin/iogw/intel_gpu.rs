//! Intel GPU counters without a runtime library or privileged subprocess.
//! Prefer system-wide i915 PMU counters. When permission is denied, DRM fdinfo
//! covers readable clients only; callers must keep that coverage label visible.
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{self, Read},
    os::fd::FromRawFd,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct IntelGpu {
    device: Option<String>,
    probed: Option<Instant>,
    pmu: Option<Vec<Counter>>,
    clients: HashMap<u64, Client>,
    sampled: Option<Instant>,
}

pub(super) struct Sample {
    pub usage: Option<f64>,
    pub system_wide: bool,
}

impl IntelGpu {
    pub fn sample(&mut self) -> Option<Sample> {
        let now = Instant::now();
        if self
            .probed
            .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(30))
            && self.pmu.is_none()
        {
            self.probed = Some(now);
            let device = intel_device(Path::new("/sys/class/drm"));
            if device != self.device {
                self.clients.clear();
                self.sampled = None;
            }
            self.device = device;
            if let Some(device) = &self.device {
                self.pmu = open_pmu(device).ok();
            }
        }
        let device = self.device.as_ref()?;
        if let Some(counters) = &mut self.pmu {
            let values = counters
                .iter_mut()
                .map(Counter::sample)
                .collect::<io::Result<Vec<_>>>();
            match values {
                Ok(values) => {
                    return Some(Sample {
                        usage: values.into_iter().flatten().reduce(f64::max),
                        system_wide: true,
                    })
                }
                Err(_) => self.pmu = None,
            }
        }
        let clients = read_clients(Path::new("/proc"), device);
        let sampled = Instant::now();
        let usage = self
            .sampled
            .and_then(|at| client_usage(&self.clients, &clients, sampled.duration_since(at)));
        self.clients = clients;
        self.sampled = Some(sampled);
        Some(Sample {
            usage,
            system_wide: false,
        })
    }
}

fn intel_device(root: &Path) -> Option<String> {
    let mut cards = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect::<Vec<_>>();
    cards.sort();
    cards.into_iter().find_map(|card| {
        let name = card.file_name()?.to_str()?;
        if !name
            .strip_prefix("card")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        let device = card.join("device");
        if fs::read_to_string(device.join("vendor")).ok()?.trim() != "0x8086" {
            return None;
        }
        let driver = fs::read_link(device.join("driver")).ok()?;
        if !matches!(driver.file_name()?.to_str()?, "i915" | "xe") {
            return None;
        }
        fs::canonicalize(device)
            .ok()?
            .file_name()?
            .to_str()
            .map(str::to_owned)
    })
}

// Linux perf_event_attr version 0 (64 bytes). The kernel ABI accepts this size;
// no sampling buffers, tracing, process inspection or control ioctls are used.
#[repr(C)]
#[derive(Default)]
struct PerfAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

struct Counter {
    file: File,
    previous: Option<[u64; 3]>,
}

impl Counter {
    fn sample(&mut self) -> io::Result<Option<f64>> {
        let mut bytes = [0u8; 24];
        self.file.read_exact(&mut bytes)?;
        let current = std::array::from_fn(|i| {
            u64::from_ne_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
        });
        let usage = self.previous.and_then(|old| pmu_usage(old, current));
        self.previous = Some(current);
        Ok(usage)
    }
}

fn pmu_usage(old: [u64; 3], new: [u64; 3]) -> Option<f64> {
    let busy = new[0].checked_sub(old[0])?;
    let enabled = new[1].checked_sub(old[1])?;
    let running = new[2].checked_sub(old[2])?;
    // count * enabled/running / enabled also accounts for multiplexed counters.
    (running > 0 && enabled >= running)
        .then(|| (busy as f64 / running as f64 * 100.0).clamp(0.0, 100.0))
}

fn open_pmu(device: &str) -> io::Result<Vec<Counter>> {
    let root = Path::new("/sys/bus/event_source/devices");
    let suffix = device.replace([':', '.'], "_");
    let specific = root.join(format!("i915_{suffix}"));
    let path = if specific.exists() {
        specific
    } else {
        root.join("i915")
    };
    // The unsuffixed PMU is the legacy single-device i915 interface. Do not
    // confuse it with an unrelated xe GPU on a mixed-driver machine.
    let driver = Path::new("/sys/bus/pci/devices")
        .join(device)
        .join("driver");
    if fs::read_link(driver)?.file_name().and_then(|v| v.to_str()) != Some("i915") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "No matching Intel PMU",
        ));
    }
    let kind = fs::read_to_string(path.join("type"))?
        .trim()
        .parse::<u32>()
        .map_err(io::Error::other)?;
    let cpumask = fs::read_to_string(path.join("cpumask"))?;
    let cpu = cpumask
        .trim()
        .split([',', '-'])
        .next()
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|cpu| *cpu >= 0)
        .ok_or_else(|| io::Error::other("Invalid PMU CPU mask"))?;
    let mut events = fs::read_dir(path.join("events"))?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with("-busy"))
        .map(|e| e.path())
        .collect::<Vec<_>>();
    events.sort();
    let mut counters = Vec::new();
    for event in events.into_iter().take(64) {
        let raw = fs::read_to_string(event)?;
        let config = raw
            .trim()
            .strip_prefix("config=0x")
            .and_then(|s| u64::from_str_radix(s, 16).ok())
            .ok_or_else(|| io::Error::other("Invalid PMU event"))?;
        let attr = PerfAttr {
            kind,
            size: std::mem::size_of::<PerfAttr>() as u32,
            config,
            read_format: 3,
            ..Default::default()
        };
        // SAFETY: attr is a live, correctly sized perf_event_attr v0. This opens
        // a read-only hardware counter on a kernel-advertised CPU, not a process.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr as *const PerfAttr,
                -1i32,
                cpu,
                -1i32,
                8u64,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful syscall returned a new owned descriptor.
        counters.push(Counter {
            file: unsafe { File::from_raw_fd(fd as i32) },
            previous: None,
        });
    }
    if counters.is_empty() {
        return Err(io::Error::other("No Intel engine counters"));
    }
    Ok(counters)
}

#[derive(Clone, Debug, Default)]
struct Client {
    engines: BTreeMap<String, (u64, u64)>, // cumulative busy nanoseconds, capacity
}

fn parse_client(text: &str, device: &str) -> Option<(u64, Client)> {
    let mut id = None;
    let mut driver = false;
    let mut matched_device = false;
    let mut engines = BTreeMap::new();
    let mut capacity = HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "drm-driver" => driver = matches!(value, "i915" | "xe"),
            "drm-client-id" => id = value.parse::<u64>().ok(),
            "drm-pdev" => matched_device = value == device,
            _ => {
                if let Some(name) = key.strip_prefix("drm-engine-capacity-") {
                    if let Ok(n) = value.parse::<u64>() {
                        if n > 0 {
                            capacity.insert(name.to_string(), n);
                        }
                    }
                } else if let Some(name) = key.strip_prefix("drm-engine-") {
                    if let Some(ns) = value
                        .strip_suffix(" ns")
                        .and_then(|s| s.trim().parse::<u64>().ok())
                    {
                        engines.insert(name.to_string(), (ns, 1));
                    }
                }
            }
        }
    }
    if !driver || !matched_device || engines.is_empty() {
        return None;
    }
    for (name, (_, n)) in &mut engines {
        *n = capacity.get(name).copied().unwrap_or(1);
    }
    Some((id?, Client { engines }))
}

fn read_clients(root: &Path, device: &str) -> HashMap<u64, Client> {
    let mut clients = HashMap::new();
    let Ok(processes) = fs::read_dir(root) else {
        return clients;
    };
    let started = Instant::now();
    // No command lines, environments, process memory or ordinary files are read.
    // Bound traversal so a busy host never monopolizes the metrics worker.
    let mut inspected = 0;
    for process in processes.flatten() {
        if started.elapsed() > Duration::from_millis(200) || inspected >= 32768 {
            break;
        }
        if !process
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let Ok(fds) = fs::read_dir(process.path().join("fdinfo")) else {
            continue;
        };
        for fd in fds.flatten() {
            inspected += 1;
            if inspected >= 32768 || started.elapsed() > Duration::from_millis(200) {
                break;
            }
            // A DRM client can be duplicated across FDs and forked processes.
            // Deduplicate globally by device-scoped drm-client-id, never by PID.
            if let Ok(text) = fs::read_to_string(fd.path()) {
                if let Some((id, client)) = parse_client(&text, device) {
                    clients.entry(id).or_insert(client);
                }
            }
        }
    }
    clients
}

fn client_usage(
    old: &HashMap<u64, Client>,
    new: &HashMap<u64, Client>,
    elapsed: Duration,
) -> Option<f64> {
    if elapsed.is_zero() {
        return None;
    }
    let mut engines = HashMap::<&str, (u64, u64)>::new();
    for (id, current) in new {
        let Some(previous) = old.get(id) else {
            continue;
        };
        // A backwards counter means this client's clock was reset. Wait for a
        // new baseline instead of mixing old and new engine epochs.
        if current.engines.iter().any(|(name, (busy, capacity))| {
            previous
                .engines
                .get(name)
                .is_some_and(|(before, old_capacity)| busy < before || capacity != old_capacity)
        }) {
            continue;
        }
        for (name, (busy, capacity)) in &current.engines {
            let Some((before, previous_capacity)) = previous.engines.get(name) else {
                continue;
            };
            if previous_capacity != capacity {
                continue;
            }
            let Some(delta) = busy.checked_sub(*before) else {
                continue;
            };
            let entry = engines.entry(name).or_insert((0, *capacity));
            entry.0 = entry.0.checked_add(delta)?;
            entry.1 = entry.1.max(*capacity);
        }
    }
    engines
        .values()
        .map(|(busy, capacity)| {
            (*busy as f64 / elapsed.as_nanos() as f64 / *capacity as f64 * 100.0).clamp(0.0, 100.0)
        })
        .reduce(f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "0000:00:02.0";
    fn client(id: u64, render: u64, video: u64) -> String {
        format!("drm-driver:\ti915\ndrm-client-id:\t{id}\ndrm-pdev:\t{DEVICE}\ndrm-engine-render:\t{render} ns\ndrm-engine-video:\t{video} ns\ndrm-engine-capacity-video:\t2\n")
    }
    #[test]
    fn pmu_abi_and_multiplexed_counter_deltas_are_correct() {
        assert_eq!(std::mem::size_of::<PerfAttr>(), 64);
        assert_eq!(pmu_usage([0, 0, 0], [25, 200, 100]), Some(25.0));
        assert_eq!(pmu_usage([100, 200, 200], [99, 300, 300]), None);
        assert_eq!(pmu_usage([0, 0, 0], [1, 2, 0]), None);
        assert_eq!(pmu_usage([0, 0, 0], [0, 2, 2]), Some(0.0));
    }
    #[test]
    fn client_counters_preserve_capacity_and_reject_unrelated_devices_and_units() {
        let (_, data) = parse_client(&client(5, 10, 20), DEVICE).unwrap();
        assert_eq!(data.engines["video"], (20, 2));
        assert!(parse_client(&client(5, 10, 20), "0000:01:00.0").is_none());
        assert!(parse_client(&client(5, 10, 20).replace(" ns", " ms"), DEVICE).is_none());
        assert!(parse_client(&client(5, 10, 20).replace("i915", "amdgpu"), DEVICE).is_none());
        assert!(parse_client(
            &client(5, 10, 20).replace("drm-client-id", "missing"),
            DEVICE
        )
        .is_none());
        assert!(parse_client(&client(5, 10, 20).replace("i915", "xe"), DEVICE).is_some());
    }
    #[test]
    fn client_deltas_dont_invent_usage_for_new_exited_or_reset_clients() {
        let map = |id, render, video| {
            HashMap::from([parse_client(&client(id, render, video), DEVICE).unwrap()])
        };
        let old = map(5, 1_000_000_000, 0);
        let mut new = map(5, 1_200_000_000, 1_000_000_000);
        new.extend(map(9, 99_000_000_000, 0));
        assert_eq!(client_usage(&old, &new, Duration::from_secs(1)), Some(50.0));
        assert_eq!(
            client_usage(&old, &map(9, 99_000_000_000, 0), Duration::from_secs(1)),
            None
        );
        assert_eq!(
            client_usage(&old, &HashMap::new(), Duration::from_secs(1)),
            None
        );
        assert_eq!(client_usage(&old, &map(5, 0, 0), Duration::ZERO), None);
        assert_eq!(
            client_usage(&old, &map(5, 0, 0), Duration::from_secs(1)),
            None
        );
        assert_eq!(client_usage(&old, &old, Duration::from_secs(1)), Some(0.0));
    }
    #[test]
    fn duplicate_and_forked_drm_handles_count_once() {
        let root = std::env::temp_dir().join(format!(
            "iogw-intel-fdinfo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for pid in [100, 200] {
            let fds = root.join(pid.to_string()).join("fdinfo");
            fs::create_dir_all(&fds).unwrap();
            fs::write(fds.join("3"), client(5, 500, 700)).unwrap();
            fs::write(fds.join("4"), client(5, 500, 700)).unwrap();
            fs::write(fds.join("9"), client(9, 100, 200)).unwrap();
            fs::write(
                fds.join("10"),
                client(5, 999, 999).replace(DEVICE, "0000:01:00.0"),
            )
            .unwrap();
        }
        let clients = read_clients(&root, DEVICE);
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[&5].engines["render"].0, 500);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "read-only probe of real Intel counters on this Linux machine"]
    fn live_intel_samples() {
        let mut gpu = IntelGpu::default();
        for _ in 0..4 {
            let sample = gpu.sample().expect("Intel GPU detected");
            println!(
                "Intel GPU: {:?}% ({})",
                sample.usage,
                if sample.system_wide {
                    "PMU / system"
                } else {
                    "fdinfo / readable clients"
                }
            );
            std::thread::sleep(Duration::from_secs(1));
        }
        assert!(
            gpu.sample().unwrap().usage.is_some(),
            "Readable Intel counters must produce a rate"
        );
    }
}
