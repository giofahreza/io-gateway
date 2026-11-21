//! macOS local-machine telemetry using the operating system's bundled tools.
//! All functions are called from the sampler's blocking worker, never the TUI
//! drawing thread. Missing or changing Apple counters remain unavailable.

use std::process::Command;

fn output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) fn cpu_percent() -> Option<f64> {
    output("/usr/bin/top", &["-l", "1", "-n", "0"]).and_then(|text| parse_cpu_percent(&text))
}

fn parse_cpu_percent(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("CPU usage:"))?;
    let value_before = |word: &str| {
        line.split_whitespace()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|parts| parts[1].trim_end_matches(',') == word)
            .and_then(|parts| parts[0].strip_suffix('%'))
            .and_then(|value| value.parse::<f64>().ok())
    };
    let user = value_before("user")?;
    let system = value_before("sys")?;
    let busy = user + system;
    busy.is_finite()
        .then_some(busy)
        .filter(|value| (0.0..=100.0).contains(value))
}

pub(super) fn memory_bytes() -> Option<(u64, u64)> {
    let total = output("/usr/sbin/sysctl", &["-n", "hw.memsize"])?
        .trim()
        .parse::<u64>()
        .ok()?;
    let stats = output("/usr/bin/vm_stat", &[])?;
    let page_size = stats.lines().next()?.split_whitespace().find_map(|value| {
        value
            .trim_matches(|ch: char| !ch.is_ascii_digit())
            .parse()
            .ok()
    })?;
    let free = page_count(&stats, "Pages free")?;
    let speculative = page_count(&stats, "Pages speculative").unwrap_or_default();
    let purgeable = page_count(&stats, "Pages purgeable").unwrap_or_default();
    let reclaimable = free
        .saturating_add(speculative)
        .saturating_add(purgeable)
        .checked_mul(page_size)?;
    Some((total.saturating_sub(reclaimable), total))
}

fn page_count(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (label, value) = line.split_once(':')?;
        (label.trim() == name)
            .then(|| value.trim().trim_end_matches('.').parse::<u64>().ok())
            .flatten()
    })
}

pub(super) fn default_interface() -> Option<String> {
    let text = output("/sbin/route", &["-n", "get", "default"])?;
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? == "interface:" {
            Some(parts.next()?.to_string())
        } else {
            None
        }
    })
}

pub(super) fn network_bytes(interface: &str) -> Option<(u64, u64)> {
    if interface.is_empty() || interface == "lo0" {
        return None;
    }
    let text = output("/usr/sbin/netstat", &["-ib"])?;
    parse_network_bytes(&text, interface)
}

fn parse_network_bytes(text: &str, interface: &str) -> Option<(u64, u64)> {
    text.lines().find_map(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.first()? != &interface || fields.len() < 10 {
            return None;
        }
        Some((fields[6].parse().ok()?, fields[9].parse().ok()?))
    })
}

pub(super) fn root_disk() -> Option<(u64, u64)> {
    let text = output("/bin/df", &["-kP", "/"])?;
    text.lines().skip(1).find_map(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let total = fields.get(1)?.parse::<u64>().ok()?.checked_mul(1024)?;
        let used = fields.get(2)?.parse::<u64>().ok()?.checked_mul(1024)?;
        (total > 0 && used <= total).then_some((used, total))
    })
}

pub(super) fn disk_counters() -> Option<(u64, u64)> {
    let text = output(
        "/usr/sbin/ioreg",
        &[
            "-r",
            "-d",
            "1",
            "-c",
            "IOBlockStorageDriver",
            "-k",
            "Statistics",
        ],
    )?;
    parse_disk_counters(&text)
}

fn parse_disk_counters(text: &str) -> Option<(u64, u64)> {
    let mut totals: Option<(u64, u64)> = None;
    for line in text.lines().filter(|line| line.contains("\"Statistics\"")) {
        let read = property_u64(line, "Bytes (Read)")?;
        let written = property_u64(line, "Bytes (Write)")?;
        let (old_read, old_written) = totals.unwrap_or_default();
        totals = Some((
            old_read.checked_add(read)?,
            old_written.checked_add(written)?,
        ));
    }
    totals
}

pub(super) fn gpu() -> (Option<f64>, Option<(u64, u64)>) {
    let Some(text) = output(
        "/usr/sbin/ioreg",
        &[
            "-r",
            "-d",
            "1",
            "-c",
            "IOAccelerator",
            "-k",
            "PerformanceStatistics",
        ],
    ) else {
        return (None, None);
    };
    (parse_gpu_percent(&text), None)
}

fn parse_gpu_percent(text: &str) -> Option<f64> {
    text.lines()
        .filter(|line| line.contains("\"PerformanceStatistics\""))
        .filter_map(|line| {
            property_u64(line, "Device Utilization %")
                .or_else(|| property_u64(line, "Renderer Utilization %"))
                .or_else(|| property_u64(line, "Tiler Utilization %"))
        })
        .filter(|value| *value <= 100)
        .map(|value| value as f64)
        .max_by(|a, b| a.total_cmp(b))
}

fn property_u64(line: &str, name: &str) -> Option<u64> {
    let marker = format!("\"{name}\"=");
    let value = line.split_once(&marker)?.1.trim_start();
    if let Some(value) = value.strip_prefix("0x") {
        u64::from_str_radix(value.split(|ch: char| !ch.is_ascii_hexdigit()).next()?, 16).ok()
    } else {
        value
            .split(|ch: char| !ch.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_top_cpu_usage() {
        let cpu = parse_cpu_percent("CPU usage: 11.11% user, 16.66% sys, 72.22% idle").unwrap();
        assert!((cpu - 27.77).abs() < 0.000_001);
        assert_eq!(parse_cpu_percent("not a top sample"), None);
    }

    #[test]
    fn parses_vm_stat_memory() {
        let stats = "Mach Virtual Memory Statistics: (page size: 16384 bytes)\nPages free: 10.\nPages speculative: 2.\nPages purgeable: 3.";
        assert_eq!(page_count(stats, "Pages free"), Some(10));
        assert_eq!(page_count(stats, "Pages purgeable"), Some(3));
    }

    #[test]
    fn parses_network_rows() {
        let text = "Name Mtu Network Address Ipkts Ierrs Ibytes Opkts Oerrs Obytes Coll\nen0 1500 <Link#1> 00:00 10 0 100 20 0 200 0\nen0 1500 192.0.2 192.0.2.1 10 - 100 20 - 200 -";
        assert_eq!(parse_network_bytes(text, "en0"), Some((100, 200)));
    }

    #[test]
    fn parses_ioreg_disk_and_gpu_statistics() {
        let text = r#"
          "Statistics" = {"Bytes (Read)"=100,"Bytes (Write)"=200}
          "Statistics" = {"Bytes (Read)"=300,"Bytes (Write)"=400}
          "PerformanceStatistics" = {"Device Utilization %"=43}
        "#;
        assert_eq!(parse_disk_counters(text), Some((400, 600)));
        assert_eq!(parse_gpu_percent(text), Some(43.0));
    }

    #[test]
    fn parses_df_capacity() {
        let text =
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1 1000 250 750 25% /\n";
        let fields = text
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>();
        assert_eq!(fields[1].parse::<u64>().unwrap() * 1024, 1_024_000);
        assert_eq!(fields[2].parse::<u64>().unwrap() * 1024, 256_000);
    }
}
