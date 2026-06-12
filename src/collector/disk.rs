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
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<Vec<DiskSnapshot>> {
        let content = fs::read_to_string("/proc/diskstats")?;
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64());
        self.last = Some(now);

        let mut out = Vec::new();
        for line in content.lines() {
            let Some(st) = parse_diskstat_line(line) else {
                continue;
            };
            if !is_physical(&st.name) {
                continue;
            }
            let (name, reads_done, read_sectors, writes_done, write_sectors, io_ticks) = (
                st.name.as_str(),
                st.reads,
                st.read_sectors,
                st.writes,
                st.write_sectors,
                st.io_ticks,
            );

            let prev = self.prev.entry(name.to_string()).or_default();
            let (r_bps, w_bps, util_pct, r_iops, w_iops) = match dt {
                Some(dt) if dt > 0.0 => (
                    read_sectors.saturating_sub(prev.read_sectors) as f64 * SECTOR_BYTES as f64
                        / dt,
                    write_sectors.saturating_sub(prev.write_sectors) as f64 * SECTOR_BYTES as f64
                        / dt,
                    (io_ticks.saturating_sub(prev.io_ticks) as f64 / (dt * 1000.0) * 100.0)
                        .min(100.0) as f32,
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
    if name.starts_with("nvme") {
        return !name.contains('p');
    }
    // mmcblk0 = disk; mmcblk0p1 / mmcblk0boot0 / mmcblk0rpmb are not.
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    // sda = disk, sda1 = partition
    if name.starts_with("sd") || name.starts_with("vd") || name.starts_with("hd") {
        return !name.chars().last().is_some_and(|c| c.is_ascii_digit());
    }
    false
}

/// Cumulative counters parsed from one `/proc/diskstats` line.
struct RawDiskStat {
    name: String,
    reads: u64,
    read_sectors: u64,
    writes: u64,
    write_sectors: u64,
    io_ticks: u64,
}

/// Pure parse of a `/proc/diskstats` line.
/// Fields (0-based): 2 name, 3 reads_completed, 5 sectors_read,
/// 7 writes_completed, 9 sectors_written, 12 io_ticks (ms device busy).
fn parse_diskstat_line(line: &str) -> Option<RawDiskStat> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 10 {
        return None;
    }
    Some(RawDiskStat {
        name: f[2].to_string(),
        reads: f[3].parse().ok()?,
        read_sectors: f[5].parse().ok()?,
        writes: f[7].parse().ok()?,
        write_sectors: f[9].parse().ok()?,
        io_ticks: f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diskstat_line_field_offsets() {
        // Real nvme line (20 fields incl. discard/flush).
        let line = "259 5 nvme0n1 7990935 0 2019919586 678655 211112 98 53123045 32217 0 151686 797684 1242 39 10199522464 86691 112 120";
        let s = parse_diskstat_line(line).unwrap();
        assert_eq!(s.name, "nvme0n1");
        assert_eq!(s.reads, 7990935);
        assert_eq!(s.read_sectors, 2019919586);
        assert_eq!(s.writes, 211112);
        assert_eq!(s.write_sectors, 53123045);
        assert_eq!(s.io_ticks, 151686);
    }

    #[test]
    fn diskstat_rejects_short_lines() {
        assert!(parse_diskstat_line("8 0 sda 1 2 3").is_none());
    }

    #[test]
    fn physical_disk_filter() {
        assert!(is_physical("nvme0n1"));
        assert!(is_physical("sda"));
        assert!(!is_physical("nvme0n1p1"));
        assert!(!is_physical("sda1"));
        assert!(!is_physical("loop0"));
        assert!(!is_physical("dm-0"));
        assert!(is_physical("mmcblk0"));
        assert!(!is_physical("mmcblk0p1"));
        assert!(!is_physical("mmcblk0boot0"));
        assert!(!is_physical("mmcblk0rpmb"));
    }
}
