//! Per-process GPU usage (P2): VRAM + utilization per PID.
//!
//! NVIDIA via NVML (sees all processes, no root needed). AMD via amdgpu
//! `/proc/<pid>/fdinfo` (own processes only unless root/CAP_SYS_PTRACE),
//! de-duplicated by `drm-client-id`, with utilization from `drm-engine-*` ns
//! deltas.

use std::collections::{HashMap, HashSet};
use std::fs;
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
    /// pid -> previous summed amdgpu engine ns (for the utilization delta).
    amd_prev: HashMap<i32, u64>,
    /// drm-pdev (e.g. "0000:05:00.0") -> amd GPU index.
    amd_pdev_idx: HashMap<String, usize>,
    last: Option<Instant>,
}

impl GpuProcCollector {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "nvidia")]
            nvml: Nvml::init().ok(),
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

        let mut out: HashMap<i32, GpuProcUse> = HashMap::new();

        let mut amd_cur: HashMap<i32, u64> = HashMap::new();
        collect_amdgpu(&self.amd_pdev_idx, &self.amd_prev, dt, &mut out, &mut amd_cur);
        self.amd_prev = amd_cur;

        #[cfg(feature = "nvidia")]
        if let Some(nvml) = self.nvml.as_ref() {
            collect_nvidia(nvml, &mut out);
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
    let mut engine_ns = 0;
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
            "drm-engine-gfx" | "drm-engine-compute" => engine_ns += parse_leading_u64(v),
            _ => {}
        }
    }
    if driver != "amdgpu" {
        return None;
    }
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
    v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0)
}

fn collect_amdgpu(
    pdev_idx: &HashMap<String, usize>,
    prev: &HashMap<i32, u64>,
    dt: Option<f64>,
    out: &mut HashMap<i32, GpuProcUse>,
    cur: &mut HashMap<i32, u64>,
) {
    let Ok(procs) = fs::read_dir("/proc") else {
        return;
    };
    for entry in procs.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        let mut seen: HashSet<u64> = HashSet::new();
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
            let fdi = format!("/proc/{pid}/fdinfo/{}", fd.file_name().to_string_lossy());
            let Ok(content) = fs::read_to_string(&fdi) else {
                continue;
            };
            let Some(info) = parse_fdinfo(&content) else {
                continue;
            };
            if !seen.insert(info.client_id) {
                continue; // same GPU client via another fd
            }
            vram += info.vram;
            engine += info.engine_ns;
            if let Some(&idx) = pdev_idx.get(&info.pdev)
                && !indices.contains(&idx) {
                    indices.push(idx);
                }
        }
        if seen.is_empty() {
            continue;
        }
        cur.insert(pid, engine);
        let util = match (prev.get(&pid), dt) {
            (Some(&pe), Some(dt)) if dt > 0.0 => {
                (engine.saturating_sub(pe) as f64 / (dt * 1e9) * 100.0) as f32
            }
            _ => 0.0,
        };
        indices.sort_unstable();
        let e = out.entry(pid).or_default();
        e.vram += vram;
        e.util_pct += util;
        for i in &indices {
            add_gpu_label(e, &format!("A{i}"));
        }
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

#[cfg(feature = "nvidia")]
fn collect_nvidia(nvml: &Nvml, out: &mut HashMap<i32, GpuProcUse>) {
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
        for p in procs {
            let mem = match p.used_gpu_memory {
                UsedGpuMemory::Used(b) => b,
                UsedGpuMemory::Unavailable => 0,
            };
            let e = out.entry(p.pid as i32).or_default();
            e.vram += mem;
            add_gpu_label(e, &label);
        }

        // Utilization (best-effort: unsupported / no samples -> skipped).
        if let Ok(samples) = dev.process_utilization_stats(None) {
            let mut latest: HashMap<u32, (u64, u32)> = HashMap::new();
            for s in samples {
                let newer = latest.get(&s.pid).map(|(t, _)| s.timestamp >= *t).unwrap_or(true);
                if newer {
                    latest.insert(s.pid, (s.timestamp, s.sm_util));
                }
            }
            for (pid, (_, sm)) in latest {
                out.entry(pid as i32).or_default().util_pct += sm as f32;
            }
        }
    }
}
