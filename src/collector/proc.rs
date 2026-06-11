//! Per-process collector (P1): pid / command / CPU% / RSS / state from `/proc`.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use super::Collector;
use crate::model::ProcInfo;

/// Assumed page size for RSS (statm reports pages). 4 KiB on essentially all
/// Linux x86_64 systems.
const PAGE_SIZE: u64 = 4096;

pub struct ProcessCollector {
    /// pid -> previous (utime + stime) jiffies.
    prev: HashMap<i32, u64>,
    /// Previous total CPU jiffies (all cores) for the % denominator.
    prev_total: u64,
    ncpu: f64,
}

impl ProcessCollector {
    pub fn new() -> Self {
        Self {
            prev: HashMap::new(),
            prev_total: 0,
            ncpu: count_cpus().max(1) as f64,
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

        let mut out = Vec::new();
        let mut cur: HashMap<i32, u64> = HashMap::new();

        for entry in fs::read_dir("/proc")?.flatten() {
            let fname = entry.file_name();
            let Some(pid) = fname.to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let Some((comm, state, jiffies)) = read_proc_stat(pid) else {
                continue;
            };
            cur.insert(pid, jiffies);

            // CPU% normalized so one fully-busy core reads ~100%.
            let cpu_pct = match self.prev.get(&pid) {
                Some(&prev_j) if total_delta > 0 => {
                    let dj = jiffies.saturating_sub(prev_j);
                    (dj as f64 / total_delta as f64 * self.ncpu * 100.0) as f32
                }
                _ => 0.0,
            };

            let name = read_cmdline(pid).unwrap_or(comm);
            out.push(ProcInfo {
                pid,
                name,
                cpu_pct,
                rss: read_rss(pid),
                state,
            });
        }

        self.prev = cur;
        self.prev_total = total;
        Ok(out)
    }
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
        .map(|rest| rest.split_whitespace().filter_map(|v| v.parse::<u64>().ok()).sum())
        .unwrap_or(0)
}

/// Returns `(comm, state, utime+stime jiffies)` from `/proc/<pid>/stat`.
/// The command (field 2) is parenthesized and may contain spaces/parens, so we
/// split on the last `)` before tokenizing the remaining fields.
fn read_proc_stat(pid: i32) -> Option<(String, char, u64)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest = s.get(close + 1..)?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After ')': index 0 = state (field 3) … index 11 = utime (14), 12 = stime (15).
    let state = f.first().and_then(|s| s.chars().next()).unwrap_or('?');
    let utime: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    Some((comm, state, utime + stime))
}

/// Resident set size in bytes from `/proc/<pid>/statm` (field 2 = pages).
fn read_rss(pid: i32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .map(|pages| pages * PAGE_SIZE)
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
