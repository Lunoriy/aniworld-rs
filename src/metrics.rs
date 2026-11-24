//! Linux resource sampling straight from procfs — no extra crate, no syscalls
//! that need privileges. The TUI uses this to show what the downloader is
//! actually costing the machine.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default)]
pub struct Metrics {
    pub cpu_percent: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub net_rx_rate: f64,
    pub net_tx_rate: f64,
    pub open_sockets: usize,
}

impl Metrics {
    pub fn mem_percent(&self) -> f32 {
        if self.mem_total == 0 {
            0.0
        } else {
            self.mem_used as f32 / self.mem_total as f32 * 100.0
        }
    }
}

struct CpuTimes {
    total: u64,
    idle: u64,
}

fn cpu_times() -> Option<CpuTimes> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut fields = line.split_whitespace();
    fields.next()?;
    let values: Vec<u64> = fields.filter_map(|v| v.parse().ok()).collect();
    if values.len() < 4 {
        return None;
    }
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    let total: u64 = values.iter().sum();
    Some(CpuTimes { total, idle })
}

fn memory() -> (u64, u64) {
    let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let mut total = 0;
    let mut available = 0;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(rest);
        }
    }
    (total.saturating_sub(available), total)
}

fn parse_kb(value: &str) -> u64 {
    value
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

fn network_bytes() -> (u64, u64) {
    let Ok(dev) = std::fs::read_to_string("/proc/net/dev") else {
        return (0, 0);
    };
    let mut rx = 0;
    let mut tx = 0;
    for line in dev.lines().skip(2) {
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 9 {
            continue;
        }
        rx += fields[0].parse::<u64>().unwrap_or(0);
        tx += fields[8].parse::<u64>().unwrap_or(0);
    }
    (rx, tx)
}

fn socket_count() -> usize {
    ["/proc/net/tcp", "/proc/net/tcp6", "/proc/net/udp"]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .map(|contents| contents.lines().count().saturating_sub(1))
        .sum()
}

pub struct Sampler {
    previous_cpu: Option<CpuTimes>,
    previous_net: Option<(u64, u64)>,
    previous_at: Instant,
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            previous_cpu: cpu_times(),
            previous_net: Some(network_bytes()),
            previous_at: Instant::now(),
        }
    }

    pub fn sample(&mut self) -> Metrics {
        let now = Instant::now();
        let elapsed = now.duration_since(self.previous_at).as_secs_f64().max(0.001);

        let cpu_percent = match (cpu_times(), &self.previous_cpu) {
            (Some(current), Some(previous)) => {
                let total = current.total.saturating_sub(previous.total);
                let idle = current.idle.saturating_sub(previous.idle);
                if total == 0 {
                    0.0
                } else {
                    ((total.saturating_sub(idle)) as f32 / total as f32) * 100.0
                }
            }
            _ => 0.0,
        };
        self.previous_cpu = cpu_times();

        let net = network_bytes();
        let (rx_rate, tx_rate) = match &self.previous_net {
            Some((rx, tx)) => (
                net.0.saturating_sub(*rx) as f64 / elapsed,
                net.1.saturating_sub(*tx) as f64 / elapsed,
            ),
            None => (0.0, 0.0),
        };
        self.previous_net = Some(net);
        self.previous_at = now;

        let (mem_used, mem_total) = memory();
        Metrics {
            cpu_percent,
            mem_used,
            mem_total,
            net_rx_rate: rx_rate,
            net_tx_rate: tx_rate,
            open_sockets: socket_count(),
        }
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the sampling thread and publish snapshots on a watch channel.
pub fn spawn(
    interval: Duration,
) -> tokio::sync::watch::Receiver<Metrics> {
    let (tx, rx) = tokio::sync::watch::channel(Metrics::default());
    std::thread::Builder::new()
        .name("aniworld-metrics".into())
        .spawn(move || {
            let mut sampler = Sampler::new();
            loop {
                std::thread::sleep(interval);
                if tx.send(sampler.sample()).is_err() {
                    break;
                }
            }
        })
        .ok();
    rx
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second.max(0.0) as u64))
}

pub fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "--:--".to_string();
    }
    let total = seconds as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}
