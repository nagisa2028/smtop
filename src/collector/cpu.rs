//! CPU / memory collector, reading `/proc` directly (no libraries).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Collector;
use crate::model::{CoreGroup, CpuSnapshot, History};

/// Cumulative jiffy counters from one `/proc/stat` cpu line.
#[derive(Clone, Copy, Default)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

impl CpuTimes {
    /// Parse the fields after the `cpu`/`cpuN` label. Any unparsable field
    /// rejects the whole line — silently dropping one would shift the
    /// position-dependent fields after it.
    fn parse(fields: &str) -> Option<Self> {
        let mut idle = 0;
        let mut total = 0;
        let mut count = 0;
        for (i, field) in fields.split_whitespace().enumerate() {
            let value: u64 = field.parse().ok()?;
            if i == 3 || i == 4 {
                idle += value;
            }
            total += value;
            count += 1;
        }
        if count < 4 {
            return None;
        }
        // user nice system idle iowait irq softirq steal ...
        Some(Self { idle, total })
    }

    fn usage_since(&self, prev: &Self) -> f32 {
        let dt = self.total.saturating_sub(prev.total);
        // Saturate: a counter quirk (e.g. steal rewinding on VM migration) must
        // not wrap into a huge busy delta.
        let di = self.idle.saturating_sub(prev.idle);
        if dt == 0 {
            0.0
        } else {
            (100.0 * dt.saturating_sub(di) as f64 / dt as f64) as f32
        }
    }
}

pub struct CpuCollector {
    model: String,
    prev_agg: CpuTimes,
    prev_cores: Vec<CpuTimes>,
    usage_hist: History,
    mem_hist: History,
    core_groups: Vec<CoreGroup>,
    primed: bool,
}

impl CpuCollector {
    pub fn new() -> Self {
        Self {
            model: read_model(),
            prev_agg: CpuTimes::default(),
            prev_cores: Vec::new(),
            usage_hist: History::new(),
            mem_hist: History::new(),
            core_groups: read_topology(),
            primed: false,
        }
    }
}

impl Collector for CpuCollector {
    type Out = CpuSnapshot;

    fn name(&self) -> &'static str {
        "cpu"
    }

    fn interval(&self) -> Duration {
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<CpuSnapshot> {
        let stat = fs::read_to_string("/proc/stat")?;
        let (agg, cores) = parse_stat(&stat);

        if self.prev_cores.len() != cores.len() {
            self.prev_cores = vec![CpuTimes::default(); cores.len()];
        }

        let usage = agg.usage_since(&self.prev_agg);
        let per_core: Vec<f32> = cores
            .iter()
            .zip(&self.prev_cores)
            .map(|(c, p)| c.usage_since(p))
            .collect();

        self.prev_agg = agg;
        self.prev_cores = cores;

        let mem = read_mem();
        let (mem_used, mem_total, swap_used, swap_total) =
            (mem.used, mem.total, mem.swap_used, mem.swap_total);
        let (load, tasks_running, tasks_total) = read_loadavg();
        let temp_c = read_cpu_temp();
        let freq_mhz = read_cpu_freq();
        let uptime_secs = fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| {
                s.split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
            })
            .map(|f| f as u64)
            .unwrap_or(0);

        // The first sample has no delta; skip pushing a misleading 0/100.
        if self.primed {
            self.usage_hist.push(usage as f64);
            let mem_pct = if mem_total > 0 {
                100.0 * mem_used as f64 / mem_total as f64
            } else {
                0.0
            };
            self.mem_hist.push(mem_pct);
        }
        self.primed = true;

        Ok(CpuSnapshot {
            model: self.model.clone(),
            per_core,
            usage,
            usage_hist: self.usage_hist.clone(),
            mem_used,
            mem_total,
            swap_used,
            swap_total,
            mem_hist: self.mem_hist.clone(),
            mem_available: mem.available,
            mem_cached: mem.cached,
            load,
            temp_c,
            freq_mhz,
            uptime_secs,
            tasks_total,
            tasks_running,
            core_groups: self.core_groups.clone(),
        })
    }
}

/// Parse the aggregate and per-CPU counters from `/proc/stat`. Per-CPU
/// entries are placed at their `cpuN` index, not their line position:
/// offline CPUs are omitted from `/proc/stat`, so positions would shift every
/// later CPU's usage onto the wrong label. Gaps stay zeroed.
fn parse_stat(stat: &str) -> (CpuTimes, Vec<CpuTimes>) {
    let mut agg = CpuTimes::default();
    let mut cores: Vec<CpuTimes> = Vec::new();
    for line in stat.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            break; // cpu lines are at the top of /proc/stat
        };
        if let Some(fields) = rest.strip_prefix(' ') {
            // aggregate "cpu  ..."
            if let Some(t) = CpuTimes::parse(fields) {
                agg = t;
            }
        } else if let Some((idx, fields)) = rest.split_once(' ')
            && let Ok(i) = idx.parse::<usize>()
            && let Some(t) = CpuTimes::parse(fields)
        {
            if cores.len() <= i {
                cores.resize(i + 1, CpuTimes::default());
            }
            cores[i] = t;
        }
    }
    (agg, cores)
}

/// Parse a kernel cpulist such as `0-3,8,10-11`.
fn parse_cpulist(s: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?);
                out.extend(a..=b);
            }
            None => out.push(part.parse().ok()?),
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn read_topology() -> Vec<CoreGroup> {
    read_topology_from(Path::new("/sys/devices/system/cpu"))
}

/// Group online logical CPUs by physical core via sysfs topology, ordered by
/// `(package, die, core_id, cpu)`.
///
/// SMT siblings come from `core_cpus_list` (`thread_siblings_list` on older
/// kernels) rather than `(package, core_id)`: `core_id` is only unique per die,
/// so on multi-die packages distinct cores would otherwise merge. Offline
/// CPUs (no topology directory, or `online` = 0) are left out. Falls back to
/// one-cpu-per-group if topology is unavailable.
fn read_topology_from(root: &Path) -> Vec<CoreGroup> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut cpus: Vec<usize> = Vec::new();
    if let Ok(rd) = fs::read_dir(root) {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(num) = name.strip_prefix("cpu")
                && let Ok(i) = num.parse::<usize>()
            {
                cpus.push(i);
            }
        }
    }
    cpus.sort_unstable();

    let read = |path: PathBuf| fs::read_to_string(path).ok();
    let online: Vec<usize> = cpus
        .iter()
        .copied()
        .filter(|cpu| {
            // cpu0 often has no `online` file; absent means online.
            read(root.join(format!("cpu{cpu}/online"))).is_none_or(|s| s.trim() != "0")
        })
        .collect();

    // Keyed for display order; each sibling set is emitted once (by its
    // first online member).
    let mut groups: BTreeMap<(i64, i64, i64, usize), CoreGroup> = BTreeMap::new();
    let mut seen: BTreeSet<Vec<usize>> = BTreeSet::new();
    let mut any_topology = false;
    for &cpu in &online {
        let topo = root.join(format!("cpu{cpu}/topology"));
        let num = |f: &str| -> Option<i64> { read(topo.join(f))?.trim().parse().ok() };
        // Online but no topology (e.g. some VMs/containers): its own core in
        // package 0; the sibling fallback below keeps it a single-cpu group.
        let pkg = num("physical_package_id");
        any_topology |= pkg.is_some();
        let pkg = pkg.unwrap_or(0);
        let siblings = read(topo.join("core_cpus_list"))
            .or_else(|| read(topo.join("thread_siblings_list")))
            .and_then(|s| parse_cpulist(&s))
            .map(|l| {
                let mut l: Vec<usize> = l.into_iter().filter(|c| online.contains(c)).collect();
                l.sort_unstable();
                l
            })
            .filter(|l| l.contains(&cpu))
            .unwrap_or_else(|| vec![cpu]);
        if !seen.insert(siblings.clone()) {
            continue;
        }
        let die = num("die_id").unwrap_or(0);
        let core = num("core_id").unwrap_or(cpu as i64);
        groups.insert(
            (pkg, die, core, siblings[0]),
            CoreGroup {
                package: pkg,
                cpus: siblings,
            },
        );
    }

    if !any_topology {
        return online
            .iter()
            .map(|&i| CoreGroup {
                package: 0,
                cpus: vec![i],
            })
            .collect();
    }
    groups.into_values().collect()
}

/// CPU package temperature in °C from hwmon (`coretemp`/`k10temp`/`zenpower`).
fn read_cpu_temp() -> Option<f32> {
    let dir = fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in dir.flatten() {
        let p = entry.path();
        let name = fs::read_to_string(p.join("name")).unwrap_or_default();
        match name.trim() {
            "coretemp" | "k10temp" | "zenpower" | "k8temp" => {
                // Prefer the package / Tctl-Tdie sensor; fall back to temp1.
                for i in 1..=8 {
                    let label =
                        fs::read_to_string(p.join(format!("temp{i}_label"))).unwrap_or_default();
                    let l = label.trim();
                    if (l.contains("Package") || l == "Tctl" || l == "Tdie")
                        && let Some(v) = read_milli_c(&p, i)
                    {
                        return Some(v);
                    }
                }
                if let Some(v) = read_milli_c(&p, 1) {
                    return Some(v);
                }
            }
            _ => {}
        }
    }
    None
}

fn read_milli_c(p: &std::path::Path, i: u32) -> Option<f32> {
    fs::read_to_string(p.join(format!("temp{i}_input")))
        .ok()?
        .trim()
        .parse::<f32>()
        .ok()
        .map(|m| m / 1000.0)
}

/// Average current core clock (MHz) from `/proc/cpuinfo`.
fn read_cpu_freq() -> Option<f32> {
    let info = fs::read_to_string("/proc/cpuinfo").ok()?;
    let (mut sum, mut n) = (0.0_f32, 0u32);
    for line in info.lines() {
        if line.starts_with("cpu MHz")
            && let Some((_, v)) = line.split_once(':')
            && let Ok(f) = v.trim().parse::<f32>()
        {
            sum += f;
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f32)
}

fn read_model() -> String {
    let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let name = info
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| "CPU".to_string());
    let cores = info.lines().filter(|l| l.starts_with("processor")).count();
    if cores > 0 {
        format!("{name}  ({cores}t)")
    } else {
        name
    }
}

struct MemInfo {
    used: u64,
    total: u64,
    available: u64,
    cached: u64,
    swap_used: u64,
    swap_total: u64,
}

fn read_mem() -> MemInfo {
    let info = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let get = |key: &str| -> u64 {
        info.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    };
    let total = get("MemTotal:");
    let available = get("MemAvailable:");
    let swap_total = get("SwapTotal:");
    let swap_free = get("SwapFree:");
    MemInfo {
        used: total.saturating_sub(available),
        total,
        available,
        cached: get("Cached:") + get("Buffers:") + get("SReclaimable:"),
        swap_used: swap_total.saturating_sub(swap_free),
        swap_total,
    }
}

/// Returns `(load[3], running_tasks, total_tasks)`.
/// `/proc/loadavg`: `<1m> <5m> <15m> <running>/<total> <lastpid>`.
fn read_loadavg() -> ([f32; 3], u32, u32) {
    let s = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let fields: Vec<&str> = s.split_whitespace().collect();
    let load = [
        fields.first().and_then(|v| v.parse().ok()).unwrap_or(0.0),
        fields.get(1).and_then(|v| v.parse().ok()).unwrap_or(0.0),
        fields.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.0),
    ];
    let (running, total) = fields
        .get(3)
        .and_then(|f| f.split_once('/'))
        .map(|(r, t)| (r.parse().unwrap_or(0), t.parse().unwrap_or(0)))
        .unwrap_or((0, 0));
    (load, running, total)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "smtop-cpu-{}-{}",
                std::process::id(),
                NEXT_TMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Fake `/sys/devices/system/cpu/cpuN`. `topo` = (package, die, core, siblings).
    fn fixture_cpu(root: &Path, cpu: usize, topo: Option<(i64, i64, i64, &str)>) {
        let dir = root.join(format!("cpu{cpu}"));
        fs::create_dir_all(&dir).unwrap();
        if let Some((pkg, die, core, sib)) = topo {
            let t = dir.join("topology");
            fs::create_dir_all(&t).unwrap();
            fs::write(t.join("physical_package_id"), format!("{pkg}\n")).unwrap();
            fs::write(t.join("die_id"), format!("{die}\n")).unwrap();
            fs::write(t.join("core_id"), format!("{core}\n")).unwrap();
            fs::write(t.join("core_cpus_list"), format!("{sib}\n")).unwrap();
        }
    }

    fn cpus_of(groups: &[CoreGroup]) -> Vec<Vec<usize>> {
        groups.iter().map(|g| g.cpus.clone()).collect()
    }

    #[test]
    fn stat_places_cpus_by_index_across_offline_gaps() {
        let stat = "cpu  30 0 0 60 0 0 0 0\n\
                    cpu0 10 0 0 20 0 0 0 0\n\
                    cpu3 20 0 0 40 0 0 0 0\n\
                    intr 1 2 3\n";
        let (agg, cores) = parse_stat(stat);
        assert_eq!(agg.total, 90);
        assert_eq!(cores.len(), 4);
        assert_eq!(cores[0].total, 30);
        assert_eq!(cores[1].total, 0); // offline gap stays zeroed
        assert_eq!(cores[3].total, 60);
    }

    #[test]
    fn cpulist_parses_ranges_and_singles() {
        assert_eq!(
            parse_cpulist("0-2,8,10-11\n"),
            Some(vec![0, 1, 2, 8, 10, 11])
        );
        assert_eq!(parse_cpulist("5"), Some(vec![5]));
        assert_eq!(parse_cpulist(""), None);
        assert_eq!(parse_cpulist("x"), None);
    }

    #[test]
    fn topology_groups_by_sibling_list_not_core_id() {
        // One package, two dies, both reusing core_id 0: must stay two cores.
        let d = TestDir::new();
        fixture_cpu(&d.0, 0, Some((0, 0, 0, "0,2")));
        fixture_cpu(&d.0, 1, Some((0, 1, 0, "1,3")));
        fixture_cpu(&d.0, 2, Some((0, 0, 0, "0,2")));
        fixture_cpu(&d.0, 3, Some((0, 1, 0, "1,3")));
        let g = read_topology_from(&d.0);
        assert_eq!(cpus_of(&g), vec![vec![0, 2], vec![1, 3]]);
    }

    #[test]
    fn topology_skips_offline_and_keeps_topologyless_cpus_separate() {
        // cpu1 offline (sibling of cpu0); cpu2 online without topology must
        // not merge with the real core_id 2 on cpu3.
        let d = TestDir::new();
        fixture_cpu(&d.0, 0, Some((0, 0, 0, "0")));
        fixture_cpu(&d.0, 1, None);
        fs::write(d.0.join("cpu1/online"), "0\n").unwrap();
        fixture_cpu(&d.0, 2, None);
        fixture_cpu(&d.0, 3, Some((0, 0, 2, "3")));
        let g = read_topology_from(&d.0);
        assert_eq!(cpus_of(&g), vec![vec![0], vec![2], vec![3]]);
    }

    #[test]
    fn cpu_times_parse_idle_and_total() {
        // user nice system idle iowait irq softirq steal
        let t = CpuTimes::parse("10 0 5 80 5 0 0 0").unwrap();
        assert_eq!(t.idle, 85); // idle + iowait
        assert_eq!(t.total, 100); // sum of all
        assert!(CpuTimes::parse("1 2 3").is_none()); // need >= 4 fields
        assert!(CpuTimes::parse("10 0 x 80 5").is_none()); // bad field shifts positions
    }

    #[test]
    fn cpu_usage_delta() {
        let a = CpuTimes::parse("10 0 5 80 5 0 0 0").unwrap(); // idle 85, total 100
        let b = CpuTimes::parse("20 0 10 80 10 0 0 0").unwrap(); // idle 90, total 120
        // busy delta = (120-100) - (90-85) = 20 - 5 = 15 over 20 -> 75%
        assert!((b.usage_since(&a) - 75.0).abs() < 0.01);
        // no progress -> 0%
        assert_eq!(a.usage_since(&a), 0.0);
    }
}
