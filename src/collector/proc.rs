//! Per-process collector (P1): pid / command / CPU% / RSS / state from `/proc`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use super::Collector;
use crate::model::ProcInfo;

/// System page size for RSS (statm reports pages). Read from the kernel —
/// not all systems use 4 KiB (e.g. 16K/64K page ARM64 kernels). rustix caches
/// the auxv lookup, so calling this per pid is cheap.
fn page_size() -> u64 {
    rustix::param::page_size() as u64
}

/// Per-pid carry-over needed for rate/usage deltas.
#[derive(Clone, Copy, Default)]
struct Prev {
    jiffies: u64,
    read_bytes: u64,
    write_bytes: u64,
}

pub struct ProcessCollector {
    /// Keyed by `(pid, starttime)` so a recycled PID doesn't inherit the dead
    /// process's counters as its baseline.
    prev: HashMap<(i32, u64), Prev>,
    /// Previous total CPU jiffies (all cores) for the % denominator.
    prev_total: u64,
    ncpu: f64,
    last: Option<Instant>,
}

impl ProcessCollector {
    pub fn new() -> Self {
        Self {
            prev: HashMap::new(),
            prev_total: 0,
            ncpu: count_cpus().max(1) as f64,
            last: None,
        }
    }
}

impl Collector for ProcessCollector {
    type Out = Vec<ProcInfo>;

    fn name(&self) -> &'static str {
        "proc"
    }

    fn interval(&self) -> Duration {
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<Vec<ProcInfo>> {
        let total = read_total_jiffies();
        let total_delta = total.saturating_sub(self.prev_total);
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64());

        let mut out = Vec::new();
        let mut cur: HashMap<(i32, u64), Prev> = HashMap::new();

        for entry in fs::read_dir("/proc")?.flatten() {
            let fname = entry.file_name();
            let Some(pid) = fname.to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let Some((comm, state, jiffies, starttime)) = read_proc_stat(pid) else {
                continue;
            };
            let io = read_proc_io(pid);
            let io_ok = io.is_some();
            let (read_bytes, write_bytes) = io.unwrap_or((0, 0));
            let prev = self.prev.get(&(pid, starttime)).copied();
            cur.insert(
                (pid, starttime),
                Prev {
                    jiffies,
                    read_bytes,
                    write_bytes,
                },
            );

            let (cpu_pct, disk_read_bps, disk_write_bps) = process_rates(
                prev,
                jiffies,
                read_bytes,
                write_bytes,
                total_delta,
                self.ncpu,
                dt,
            );

            let name = read_cmdline(pid).unwrap_or(comm);
            out.push(ProcInfo {
                pid,
                name,
                cpu_pct,
                rss: read_rss(pid),
                state,
                disk_read_bps,
                disk_write_bps,
                io_ok,
            });
        }

        replace_previous(&mut self.prev, cur);
        self.prev_total = total;
        self.last = Some(now);
        Ok(out)
    }
}

fn replace_previous(previous: &mut HashMap<(i32, u64), Prev>, current: HashMap<(i32, u64), Prev>) {
    *previous = current;
}

/// Calculate rates for one process observation. Keeping this independent of
/// procfs makes first samples, PID reuse and counter resets deterministic to
/// test (PID reuse is represented by `prev == None`).
fn process_rates(
    prev: Option<Prev>,
    jiffies: u64,
    read_bytes: u64,
    write_bytes: u64,
    total_delta: u64,
    ncpu: f64,
    dt: Option<f64>,
) -> (f32, f64, f64) {
    // CPU% normalized so one fully-busy core reads ~100%.
    let cpu_pct = match prev {
        Some(p) if total_delta > 0 => {
            let dj = jiffies.saturating_sub(p.jiffies);
            (dj as f64 / total_delta as f64 * ncpu * 100.0) as f32
        }
        _ => 0.0,
    };
    let (disk_read_bps, disk_write_bps) = match (prev, dt) {
        (Some(p), Some(dt)) if dt > 0.0 => (
            read_bytes.saturating_sub(p.read_bytes) as f64 / dt,
            write_bytes.saturating_sub(p.write_bytes) as f64 / dt,
        ),
        _ => (0.0, 0.0),
    };
    (cpu_pct, disk_read_bps, disk_write_bps)
}

/// Disk bytes read/written from `/proc/<pid>/io` (read_bytes, write_bytes).
/// Returns `None` when inaccessible (needs ownership or CAP_SYS_PTRACE), so the
/// caller can distinguish "unknown" from "zero".
fn read_proc_io(pid: i32) -> Option<(u64, u64)> {
    let content = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut read = 0;
    let mut write = 0;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            read = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            write = v.trim().parse().unwrap_or(0);
        }
    }
    Some((read, write))
}

fn count_cpus() -> usize {
    fs::read_to_string("/proc/stat")
        .map(|s| {
            s.lines()
                .filter(|l| {
                    l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit)
                })
                .count()
        })
        .unwrap_or(1)
}

/// Sum of all fields on the aggregate `cpu` line of `/proc/stat`.
fn read_total_jiffies() -> u64 {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    stat.lines()
        .next()
        .and_then(|l| l.strip_prefix("cpu "))
        .map(|rest| {
            rest.split_whitespace()
                .filter_map(|v| v.parse::<u64>().ok())
                .sum()
        })
        .unwrap_or(0)
}

/// Returns `(comm, state, utime+stime jiffies, starttime)` from
/// `/proc/<pid>/stat`. The command (field 2) is parenthesized and may contain
/// spaces/parens, so we split on the last `)` before tokenizing the remaining
/// fields. `starttime` (boot-relative ticks) identifies this incarnation of
/// the PID.
fn read_proc_stat(pid: i32) -> Option<(String, char, u64, u64)> {
    read_proc_stat_at(Path::new("/proc"), pid)
}

fn read_proc_stat_at(root: &Path, pid: i32) -> Option<(String, char, u64, u64)> {
    let s = fs::read_to_string(root.join(pid.to_string()).join("stat")).ok()?;
    parse_proc_stat(&s)
}

fn parse_proc_stat(s: &str) -> Option<(String, char, u64, u64)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest = s.get(close + 1..)?;
    let mut f = rest.split_whitespace();
    // After ')': index 0 = state (field 3) … 11 = utime (14), 12 = stime (15),
    // 19 = starttime (22).
    let state = f.next().and_then(|s| s.chars().next()).unwrap_or('?');
    let utime: u64 = f.nth(10).and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let starttime: u64 = f.nth(6).and_then(|v| v.parse().ok()).unwrap_or(0);
    Some((comm, state, utime + stime, starttime))
}

pub(super) fn read_starttime_at(root: &Path, pid: i32) -> u64 {
    read_proc_stat_at(root, pid)
        .map(|(_, _, _, st)| st)
        .unwrap_or(0)
}

/// Resident set size in bytes from `/proc/<pid>/statm` (field 2 = pages).
fn read_rss(pid: i32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map(|pages| pages * page_size())
        .unwrap_or(0)
}

/// Full command line (NUL-separated args joined with spaces), if available.
fn read_cmdline(pid: i32) -> Option<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    let s = String::from_utf8_lossy(&raw)
        .replace('\0', " ")
        .trim()
        .to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_stream_parser_handles_spaces_and_parens() {
        let stat = "42 (worker ) name) R 1 2 3 4 5 6 7 8 9 10 101 202 13 14 15 16 17 18 999";
        let (name, state, jiffies, starttime) = parse_proc_stat(stat).unwrap();
        assert_eq!(name, "worker ) name");
        assert_eq!(state, 'R');
        assert_eq!(jiffies, 303);
        assert_eq!(starttime, 999);
    }

    #[test]
    fn process_rates_cover_first_sample_delta_and_counter_reset() {
        assert_eq!(
            process_rates(None, 50, 1_000, 2_000, 100, 4.0, Some(1.0)),
            (0.0, 0.0, 0.0)
        );

        let prev = Prev {
            jiffies: 50,
            read_bytes: 1_000,
            write_bytes: 2_000,
        };
        let (cpu, read, write) = process_rates(Some(prev), 75, 3_000, 5_000, 100, 4.0, Some(2.0));
        assert!((cpu - 100.0).abs() < f32::EPSILON);
        assert_eq!(read, 1_000.0);
        assert_eq!(write, 1_500.0);

        // Kernel counters can reset; saturating subtraction must not spike.
        assert_eq!(
            process_rates(Some(prev), 10, 100, 200, 100, 4.0, Some(1.0)),
            (0.0, 0.0, 0.0)
        );
        // A recycled PID has a different starttime and therefore no baseline.
        assert_eq!(
            process_rates(None, 10_000, 10_000, 10_000, 100, 4.0, Some(1.0)),
            (0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn disappeared_and_recycled_pids_drop_old_baselines() {
        let mut previous =
            HashMap::from([((10, 100), Prev::default()), ((20, 200), Prev::default())]);
        replace_previous(
            &mut previous,
            HashMap::from([((20, 200), Prev::default()), ((10, 999), Prev::default())]),
        );
        assert!(!previous.contains_key(&(10, 100)));
        assert!(previous.contains_key(&(10, 999)));
        assert!(previous.contains_key(&(20, 200)));
    }
}
