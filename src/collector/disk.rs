//! Disk I/O collector from `/proc/diskstats` (physical devices only).

use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};

use super::Collector;
use crate::model::{DiskSnapshot, History};

const SECTOR_BYTES: u64 = 512;

#[derive(Default)]
struct Prev {
    read_sectors: u64,
    write_sectors: u64,
    reads_done: u64,
    writes_done: u64,
    io_ticks: u64,
    r_hist: History,
    w_hist: History,
}

pub struct DiskCollector {
    prev: HashMap<String, Prev>,
    last: Option<Instant>,
}

impl DiskCollector {
    pub fn new() -> Self {
        Self {
            prev: HashMap::new(),
            last: None,
        }
    }
}

impl Collector for DiskCollector {
    type Out = Vec<DiskSnapshot>;

    fn name(&self) -> &'static str {
        "disk"
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn sample(&mut self) -> anyhow::Result<Vec<DiskSnapshot>> {
        let content = fs::read_to_string("/proc/diskstats")?;
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64());
        self.last = Some(now);

        let mut out = Vec::new();
        for line in content.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            // major minor name reads merged sectors_read ... writes merged sectors_written
            if f.len() < 10 {
                continue;
            }
            let name = f[2];
            if !is_physical(name) {
                continue;
            }
            // /proc/diskstats fields (0-based): 3 reads_completed, 5 sectors_read,
            // 7 writes_completed, 9 sectors_written, 12 io_ticks (ms device busy).
            let reads_done: u64 = f[3].parse().unwrap_or(0);
            let read_sectors: u64 = f[5].parse().unwrap_or(0);
            let writes_done: u64 = f[7].parse().unwrap_or(0);
            let write_sectors: u64 = f[9].parse().unwrap_or(0);
            let io_ticks: u64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);

            let prev = self.prev.entry(name.to_string()).or_default();
            let (r_bps, w_bps, util_pct, r_iops, w_iops) = match dt {
                Some(dt) if dt > 0.0 => (
                    read_sectors.saturating_sub(prev.read_sectors) as f64 * SECTOR_BYTES as f64 / dt,
                    write_sectors.saturating_sub(prev.write_sectors) as f64 * SECTOR_BYTES as f64 / dt,
                    (io_ticks.saturating_sub(prev.io_ticks) as f64 / (dt * 1000.0) * 100.0).min(100.0) as f32,
                    reads_done.saturating_sub(prev.reads_done) as f64 / dt,
                    writes_done.saturating_sub(prev.writes_done) as f64 / dt,
                ),
                _ => (0.0, 0.0, 0.0, 0.0, 0.0),
            };
            prev.read_sectors = read_sectors;
            prev.write_sectors = write_sectors;
            prev.reads_done = reads_done;
            prev.writes_done = writes_done;
            prev.io_ticks = io_ticks;
            if dt.is_some() {
                prev.r_hist.push(r_bps);
                prev.w_hist.push(w_bps);
            }

            out.push(DiskSnapshot {
                dev: name.to_string(),
                r_bps,
                w_bps,
                r_hist: prev.r_hist.clone(),
                w_hist: prev.w_hist.clone(),
                util_pct,
                r_iops,
                w_iops,
            });
        }
        out.sort_by(|a, b| a.dev.cmp(&b.dev));
        Ok(out)
    }
}

/// Whole physical disks only — skip partitions and virtual/loop devices.
fn is_physical(name: &str) -> bool {
    if name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("dm-")
        || name.starts_with("sr")
        || name.starts_with("fd")
        || name.starts_with("zram")
    {
        return false;
    }
    // nvme0n1 = disk, nvme0n1p1 = partition
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        return !name.contains('p');
    }
    // sda = disk, sda1 = partition
    if name.starts_with("sd") || name.starts_with("vd") || name.starts_with("hd") {
        return !name.chars().last().is_some_and(|c| c.is_ascii_digit());
    }
    false
}
