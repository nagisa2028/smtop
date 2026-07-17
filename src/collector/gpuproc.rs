//! Per-process GPU usage (P2): VRAM + utilization per PID.
//!
//! NVIDIA via NVML (sees all processes, no root needed). AMD via amdgpu
//! `/proc/<pid>/fdinfo` (own processes only unless root/CAP_SYS_PTRACE),
//! de-duplicated by `drm-pdev` + `drm-client-id`, with utilization from
//! `drm-engine-*` ns deltas.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use super::Collector;
use crate::model::GpuProcUse;

#[cfg(feature = "nvidia")]
use nvml_wrapper::Nvml;
#[cfg(feature = "nvidia")]
use nvml_wrapper::enums::device::UsedGpuMemory;

pub struct GpuProcCollector {
    #[cfg(feature = "nvidia")]
    nvml: Option<Nvml>,
    /// device index -> last seen NVML sample timestamp (µs), so each tick only
    /// fetches utilization samples newer than the previous one.
    #[cfg(feature = "nvidia")]
    nv_last_ts: HashMap<u32, u64>,
    /// (pid, starttime) -> previous summed amdgpu engine ns (for the
    /// utilization delta); starttime keeps a recycled PID from inheriting the
    /// dead process's counter as its baseline.
    amd_prev: HashMap<(i32, u64), u64>,
    /// drm-pdev (e.g. "0000:05:00.0") -> amd GPU index.
    amd_pdev_idx: HashMap<String, usize>,
    last: Option<Instant>,
}

impl GpuProcCollector {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "nvidia")]
            nvml: Nvml::init().ok(),
            #[cfg(feature = "nvidia")]
            nv_last_ts: HashMap::new(),
            amd_prev: HashMap::new(),
            amd_pdev_idx: amd_pdev_index(),
            last: None,
        }
    }
}

impl Collector for GpuProcCollector {
    type Out = HashMap<i32, GpuProcUse>;

    fn name(&self) -> &'static str {
        "gpuproc"
    }

    fn interval(&self) -> Duration {
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<Self::Out> {
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64());
        self.last = Some(now);

        // Public map is keyed by PID only. If a PID is recycled between this
        // tick and the UI's join, the new process can briefly show the prior
        // process's GPU/VRAM until the next tick overwrites it — an accepted
        // ~1-sample window (delta tracking below keys on (pid, starttime), so
        // only the display join is affected).
        let mut out: HashMap<i32, GpuProcUse> = HashMap::new();

        let mut amd_cur: HashMap<(i32, u64), u64> = HashMap::new();
        collect_amdgpu(
            &self.amd_pdev_idx,
            &self.amd_prev,
            dt,
            &mut out,
            &mut amd_cur,
        );
        self.amd_prev = amd_cur;

        #[cfg(feature = "nvidia")]
        if let Some(nvml) = self.nvml.as_ref() {
            collect_nvidia(nvml, &mut self.nv_last_ts, &mut out);
        }

        Ok(out)
    }
}

/// Map each amdgpu card's PCI address to its enumeration index (matching the
/// AMD device collector's ordering).
fn amd_pdev_index() -> HashMap<String, usize> {
    let mut cards: Vec<(String, String)> = Vec::new();
    if let Ok(rd) = fs::read_dir("/sys/class/drm") {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let is_card = name
                .strip_prefix("card")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if !is_card {
                continue;
            }
            let dev = e.path().join("device");
            let uevent = fs::read_to_string(dev.join("uevent")).unwrap_or_default();
            if !uevent.lines().any(|l| l == "DRIVER=amdgpu") {
                continue;
            }
            let pdev = fs::canonicalize(&dev)
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                .unwrap_or_default();
            cards.push((name.into_owned(), pdev));
        }
    }
    cards.sort();
    cards
        .into_iter()
        .enumerate()
        .map(|(i, (_, pdev))| (pdev, i))
        .collect()
}

struct FdInfo {
    client_id: u64,
    pdev: String,
    vram: u64,
    engine_ns: u64,
}

/// Parse one `/proc/<pid>/fdinfo/<fd>` for an amdgpu DRM client.
fn parse_fdinfo(content: &str) -> Option<FdInfo> {
    let mut driver = "";
    let mut client_id = None;
    let mut pdev = String::new();
    let mut vram = 0;
    // Per drm-usage-stats: any `drm-engine-<name>` reports busy ns for that
    // engine, and an optional `drm-engine-capacity-<name>` gives how many
    // parallel units it has (default 1). Utilization normalizes each engine's
    // busy time by its capacity, so collect both and combine after the loop.
    let mut engines: HashMap<String, u64> = HashMap::new();
    let mut capacities: HashMap<String, u64> = HashMap::new();
    for line in content.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "drm-driver" => driver = v,
            "drm-client-id" => client_id = v.parse().ok(),
            "drm-pdev" => pdev = v.to_string(),
            "drm-total-vram" => vram = parse_size(v),
            // Check the capacity prefix first: `drm-engine-capacity-<name>`
            // also matches the `drm-engine-` prefix.
            k => {
                if let Some(name) = k.strip_prefix("drm-engine-capacity-") {
                    capacities.insert(name.to_string(), parse_leading_u64(v).max(1));
                } else if let Some(name) = k.strip_prefix("drm-engine-") {
                    engines.insert(name.to_string(), parse_leading_u64(v));
                }
            }
        }
    }
    if driver != "amdgpu" {
        return None;
    }
    // Sum capacity-normalized busy ns across every engine (gfx/compute/dec/enc/
    // sdma/…). Capacity is constant over time, so a single summed value keeps
    // the downstream ns-delta math unchanged. Integer division drops
    // ≤(capacity−1) ns per sample, negligible against `dt * 1e9`.
    let engine_ns = engines
        .iter()
        .map(|(name, &ns)| ns / capacities.get(name).copied().unwrap_or(1).max(1))
        .sum();
    Some(FdInfo {
        client_id: client_id?,
        pdev,
        vram,
        engine_ns,
    })
}

/// `"10396 KiB"` -> bytes.
fn parse_size(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let n: f64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let mult = match it.next().unwrap_or("B") {
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (n * mult) as u64
}

/// Leading integer of e.g. `"649699 ns"`.
fn parse_leading_u64(v: &str) -> u64 {
    v.split_whitespace()
        .next()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0)
}

fn collect_amdgpu(
    pdev_idx: &HashMap<String, usize>,
    prev: &HashMap<(i32, u64), u64>,
    dt: Option<f64>,
    out: &mut HashMap<i32, GpuProcUse>,
    cur: &mut HashMap<(i32, u64), u64>,
) {
    collect_amdgpu_at(Path::new("/proc"), pdev_idx, prev, dt, out, cur);
}

fn collect_amdgpu_at(
    proc_root: &Path,
    pdev_idx: &HashMap<String, usize>,
    prev: &HashMap<(i32, u64), u64>,
    dt: Option<f64>,
    out: &mut HashMap<i32, GpuProcUse>,
    cur: &mut HashMap<(i32, u64), u64>,
) {
    let Ok(procs) = fs::read_dir(proc_root) else {
        return;
    };
    for entry in procs.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        let proc_dir = proc_root.join(pid.to_string());
        let Ok(fds) = fs::read_dir(proc_dir.join("fd")) else {
            continue;
        };
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        let mut vram = 0u64;
        let mut engine = 0u64;
        let mut indices: Vec<usize> = Vec::new();
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            if !target.to_string_lossy().starts_with("/dev/dri/") {
                continue;
            }
            let fdi = proc_dir.join("fdinfo").join(fd.file_name());
            let Ok(content) = fs::read_to_string(fdi) else {
                continue;
            };
            let Some(info) = parse_fdinfo(&content) else {
                continue;
            };
            if !seen.insert(fdinfo_client_key(&info)) {
                continue; // same GPU client via another fd
            }
            vram += info.vram;
            engine += info.engine_ns;
            if let Some(&idx) = pdev_idx.get(&info.pdev)
                && !indices.contains(&idx)
            {
                indices.push(idx);
            }
        }
        if seen.is_empty() {
            continue;
        }
        let key = (pid, super::proc::read_starttime_at(proc_root, pid));
        cur.insert(key, engine);
        let util = amd_util(prev.get(&key).copied(), engine, dt);
        indices.sort_unstable();
        merge_gpu_use(out, pid, vram, util, "");
        let e = out.entry(pid).or_default();
        for i in &indices {
            add_gpu_label(e, &format!("A{i}"));
        }
    }
}

fn merge_gpu_use(
    out: &mut HashMap<i32, GpuProcUse>,
    pid: i32,
    vram: u64,
    util_pct: f32,
    label: &str,
) {
    let usage = out.entry(pid).or_default();
    usage.vram += vram;
    usage.util_pct += util_pct;
    if !label.is_empty() {
        add_gpu_label(usage, label);
    }
}

fn amd_util(prev_engine_ns: Option<u64>, engine_ns: u64, dt: Option<f64>) -> f32 {
    match (prev_engine_ns, dt) {
        (Some(prev), Some(dt)) if dt > 0.0 => {
            (engine_ns.saturating_sub(prev) as f64 / (dt * 1e9) * 100.0) as f32
        }
        _ => 0.0,
    }
}

/// Append a GPU token (e.g. "N0", "A1") to a process's comma-separated label,
/// skipping duplicates.
fn add_gpu_label(e: &mut GpuProcUse, token: &str) {
    if e.label.split(',').any(|t| t == token) {
        return;
    }
    if !e.label.is_empty() {
        e.label.push(',');
    }
    e.label.push_str(token);
}

fn fdinfo_client_key(info: &FdInfo) -> (String, u64) {
    (info.pdev.clone(), info.client_id)
}

#[cfg(feature = "nvidia")]
fn collect_nvidia(
    nvml: &Nvml,
    last_ts: &mut HashMap<u32, u64>,
    out: &mut HashMap<i32, GpuProcUse>,
) {
    let count = nvml.device_count().unwrap_or(0);
    for i in 0..count {
        let Ok(dev) = nvml.device_by_index(i) else {
            continue;
        };
        let label = format!("N{i}");

        let mut procs = Vec::new();
        if let Ok(p) = dev.running_compute_processes() {
            procs.extend(p);
        }
        if let Ok(p) = dev.running_graphics_processes() {
            procs.extend(p);
        }
        let mut mem_by_pid: HashMap<u32, u64> = HashMap::new();
        for p in procs {
            let mem = match p.used_gpu_memory {
                UsedGpuMemory::Used(b) => b,
                UsedGpuMemory::Unavailable => 0,
            };
            remember_pid_vram(&mut mem_by_pid, p.pid, mem);
        }
        for (pid, mem) in mem_by_pid {
            merge_gpu_use(out, pid as i32, mem, 0.0, &label);
        }

        // Utilization (best-effort: unsupported / no new samples -> skipped).
        // Only fetch samples newer than the previous tick's, instead of the
        // driver's whole sample buffer every time.
        if let Ok(samples) = dev.process_utilization_stats(last_ts.get(&i).copied()) {
            let dev_ts = last_ts.entry(i).or_insert(0);
            let latest = latest_process_samples(
                samples.into_iter().map(|s| (s.pid, s.timestamp, s.sm_util)),
                dev_ts,
            );
            for (pid, (_, sm)) in latest {
                merge_gpu_use(out, pid as i32, 0, sm as f32, &label);
                // Tag the GPU even when the pid wasn't in the VRAM process
                // lists, so the GPU column isn't blank for a row that sorts
                // high on GPU%.
            }
        }
    }
}

#[cfg(feature = "nvidia")]
fn latest_process_samples(
    samples: impl IntoIterator<Item = (u32, u64, u32)>,
    last_timestamp: &mut u64,
) -> HashMap<u32, (u64, u32)> {
    let mut latest = HashMap::new();
    for (pid, timestamp, sm_util) in samples {
        *last_timestamp = (*last_timestamp).max(timestamp);
        if latest.get(&pid).is_none_or(|(seen, _)| timestamp >= *seen) {
            latest.insert(pid, (timestamp, sm_util));
        }
    }
    latest
}

#[cfg(feature = "nvidia")]
fn remember_pid_vram(mem_by_pid: &mut HashMap<u32, u64>, pid: u32, mem: u64) {
    mem_by_pid
        .entry(pid)
        .and_modify(|m| *m = (*m).max(mem))
        .or_insert(mem);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "smtop-gpuproc-{}-{}",
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

    #[test]
    fn amd_fdinfo_dedup_key_includes_pci_device() {
        let a = FdInfo {
            client_id: 7,
            pdev: "0000:05:00.0".into(),
            vram: 0,
            engine_ns: 0,
        };
        let b = FdInfo {
            client_id: 7,
            pdev: "0000:06:00.0".into(),
            vram: 0,
            engine_ns: 0,
        };

        assert_ne!(fdinfo_client_key(&a), fdinfo_client_key(&b));
    }

    #[test]
    fn amd_fdinfo_sums_all_engines_normalized_by_capacity() {
        let content = "\
drm-driver:\tamdgpu
drm-client-id:\t42
drm-pdev:\t0000:05:00.0
drm-total-vram:\t10240 KiB
drm-engine-gfx:\t1000 ns
drm-engine-compute:\t2000 ns
drm-engine-dec:\t800 ns
drm-engine-capacity-dec:\t2
";
        let info = parse_fdinfo(content).expect("amdgpu fdinfo");
        // gfx(1000) + compute(2000) + dec(800/2=400) = 3400
        assert_eq!(info.engine_ns, 3400);
        assert_eq!(info.client_id, 42);
        assert_eq!(info.vram, 10240 * 1024);
    }

    #[test]
    fn amd_fdinfo_rejects_other_drivers_and_missing_client() {
        assert!(parse_fdinfo("drm-driver: i915\ndrm-client-id: 1").is_none());
        assert!(parse_fdinfo("drm-driver: amdgpu\ndrm-pdev: 0000:01:00.0").is_none());
    }

    #[test]
    fn amd_size_util_and_labels_cover_boundaries() {
        assert_eq!(parse_size("2 MiB"), 2 * 1024 * 1024);
        assert_eq!(parse_size("1 GiB"), 1024 * 1024 * 1024);
        assert_eq!(amd_util(None, 1_000_000_000, Some(1.0)), 0.0);
        assert_eq!(amd_util(Some(1_000), 500_001_000, Some(1.0)), 50.0);
        assert_eq!(amd_util(Some(1_000), 500, Some(1.0)), 0.0);

        let mut usage = GpuProcUse::default();
        add_gpu_label(&mut usage, "A0");
        add_gpu_label(&mut usage, "A0");
        add_gpu_label(&mut usage, "N1");
        assert_eq!(usage.label, "A0,N1");
    }

    #[cfg(unix)]
    #[test]
    fn amd_collection_deduplicates_fds_and_aggregates_multiple_gpus() {
        use std::os::unix::fs::symlink;

        let tmp = TestDir::new();
        let proc_dir = tmp.0.join("42");
        fs::create_dir_all(proc_dir.join("fd")).unwrap();
        fs::create_dir_all(proc_dir.join("fdinfo")).unwrap();
        fs::write(
            proc_dir.join("stat"),
            "42 (gpu worker) S 1 2 3 4 5 6 7 8 9 10 0 0 13 14 15 16 17 18 999",
        )
        .unwrap();

        let info0 = "drm-driver: amdgpu\ndrm-client-id: 7\ndrm-pdev: 0000:05:00.0\ndrm-total-vram: 10 MiB\ndrm-engine-gfx: 1000000000 ns\n";
        let info1 = "drm-driver: amdgpu\ndrm-client-id: 7\ndrm-pdev: 0000:06:00.0\ndrm-total-vram: 20 MiB\ndrm-engine-gfx: 2000000000 ns\n";
        for (fd, info) in [("3", info0), ("4", info0), ("5", info1)] {
            symlink("/dev/dri/renderD128", proc_dir.join("fd").join(fd)).unwrap();
            fs::write(proc_dir.join("fdinfo").join(fd), info).unwrap();
        }

        let pdev = HashMap::from([
            ("0000:05:00.0".to_string(), 0),
            ("0000:06:00.0".to_string(), 1),
        ]);
        let prev = HashMap::from([((42, 999), 2_000_000_000)]);
        let mut out = HashMap::new();
        let mut cur = HashMap::new();
        collect_amdgpu_at(&tmp.0, &pdev, &prev, Some(1.0), &mut out, &mut cur);

        let usage = out.get(&42).unwrap();
        assert_eq!(usage.vram, 30 * 1024 * 1024);
        assert_eq!(usage.util_pct, 100.0);
        assert_eq!(usage.label, "A0,A1");
        assert_eq!(cur.get(&(42, 999)), Some(&3_000_000_000));
    }

    #[test]
    fn vendor_results_merge_for_the_same_pid_without_duplicate_labels() {
        let mut out = HashMap::new();
        merge_gpu_use(&mut out, 42, 10, 20.0, "A0");
        merge_gpu_use(&mut out, 42, 30, 40.0, "N0");
        merge_gpu_use(&mut out, 42, 0, 5.0, "N0");
        assert_eq!(
            out.get(&42),
            Some(&GpuProcUse {
                vram: 40,
                util_pct: 65.0,
                label: "A0,N0".into(),
            })
        );
    }

    #[cfg(feature = "nvidia")]
    #[test]
    fn nvidia_proc_vram_keeps_largest_duplicate_sample() {
        let mut by_pid = HashMap::new();
        remember_pid_vram(&mut by_pid, 42, 1024);
        remember_pid_vram(&mut by_pid, 42, 512);
        remember_pid_vram(&mut by_pid, 42, 2048);

        assert_eq!(by_pid.get(&42), Some(&2048));
        assert_eq!(by_pid.len(), 1);
    }

    #[cfg(feature = "nvidia")]
    #[test]
    fn nvidia_proc_util_keeps_latest_sample_and_advances_timestamp() {
        let mut timestamp = 100;
        let latest = latest_process_samples(
            [(7, 120, 30), (7, 110, 99), (8, 130, 40), (7, 140, 50)],
            &mut timestamp,
        );
        assert_eq!(latest.get(&7), Some(&(140, 50)));
        assert_eq!(latest.get(&8), Some(&(130, 40)));
        assert_eq!(timestamp, 140);
    }
}
