//! Shared data model: snapshot types published by collectors and read by the UI.
//!
//! Each collector owns its own history ring buffers and publishes a fresh,
//! history-bearing snapshot into `SharedState` via `ArcSwap` (lock-free). The UI
//! renders whatever is latest at its own frame rate, decoupled from collection.

use std::collections::HashMap;
use std::time::Instant;

use arc_swap::ArcSwapOption;

/// A snapshot plus when it was published, so the UI can flag a collector that
/// stopped updating (stale data) instead of presenting frozen values as live.
/// `Deref`s to the inner snapshot, so readers use it transparently.
pub struct Stamped<T> {
    pub at: Instant,
    data: T,
}

impl<T> Stamped<T> {
    pub fn new(data: T) -> Self {
        Self {
            at: Instant::now(),
            data,
        }
    }
}

impl<T> std::ops::Deref for Stamped<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.data
    }
}

/// Number of samples retained per time-series (~120 points is ~1 KB/series).
pub const HIST_CAP: usize = 120;

/// A fixed-capacity ring buffer of `f64` samples for time-series charts.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// Chart-ready points. Keeping the x coordinates here avoids rebuilding
    /// and allocating every series on every UI frame (the UI redraws more
    /// often than collectors publish).
    points: Vec<(f64, f64)>,
}

impl History {
    pub fn new() -> Self {
        Self {
            points: Vec::with_capacity(HIST_CAP),
        }
    }

    pub fn push(&mut self, v: f64) {
        if self.points.len() == HIST_CAP {
            // The x coordinates remain 0..HIST_CAP; only slide the samples.
            for i in 1..HIST_CAP {
                self.points[i - 1].1 = self.points[i].1;
            }
            self.points[HIST_CAP - 1].1 = v;
        } else {
            // Histories are right-aligned while filling, so existing points
            // move one column left before the newest point is appended.
            for (x, _) in &mut self.points {
                *x -= 1.0;
            }
            self.points.push(((HIST_CAP - 1) as f64, v));
        }
    }

    pub fn max(&self) -> f64 {
        self.points.iter().map(|&(_, y)| y).fold(0.0_f64, f64::max)
    }

    /// `(x, y)` points for a ratatui `Chart` over a fixed `0..HIST_CAP` x-axis.
    ///
    /// Points are right-aligned: the newest sample sits at the right edge
    /// (`x = HIST_CAP - 1`) and older samples extend left, leaving the left
    /// blank until the buffer fills. This keeps "newest on the right" stable
    /// instead of rescaling the whole graph while history accumulates.
    pub fn points(&self) -> &[(f64, f64)] {
        &self.points
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_right_aligned_and_bounded() {
        let mut history = History::new();
        history.push(10.0);
        history.push(20.0);
        assert_eq!(
            history.points(),
            &[((HIST_CAP - 2) as f64, 10.0), ((HIST_CAP - 1) as f64, 20.0),]
        );

        for value in 2..=HIST_CAP {
            history.push(value as f64);
        }
        assert_eq!(history.points().len(), HIST_CAP);
        assert_eq!(history.points().first(), Some(&(0.0, 20.0)));
        assert_eq!(
            history.points().last(),
            Some(&((HIST_CAP - 1) as f64, HIST_CAP as f64))
        );
        assert_eq!(history.max(), HIST_CAP as f64);
    }

    #[test]
    fn history_evicts_old_peak_and_clones_independently() {
        let mut history = History::new();
        assert_eq!(history.max(), 0.0);
        history.push(10_000.0);
        for value in 1..HIST_CAP {
            history.push(value as f64);
        }
        assert_eq!(history.max(), 10_000.0);

        let mut cloned = history.clone();
        history.push(HIST_CAP as f64); // evicts the old 10_000 peak
        assert_eq!(history.max(), HIST_CAP as f64);
        assert_eq!(history.points().first(), Some(&(0.0, 1.0)));

        cloned.push(50_000.0);
        assert_eq!(cloned.max(), 50_000.0);
        assert_eq!(history.max(), HIST_CAP as f64);
    }
}

// Vendor-neutral data model: without the `nvidia` feature the NVIDIA variants
// are simply never constructed, which is fine — not dead code to remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "nvidia"), allow(dead_code))]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
}

/// Fan reading, normalized per driver (NVIDIA usually reports %, DRM hwmon RPM).
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(feature = "nvidia"), allow(dead_code))]
pub enum Fan {
    Pct(f32),
    Rpm(u32),
}

#[derive(Clone, Debug, Default)]
pub struct CpuSnapshot {
    pub model: String,
    /// Per-core utilization, 0..=100.
    pub per_core: Vec<f32>,
    /// Aggregate utilization, 0..=100.
    pub usage: f32,
    pub usage_hist: History,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// Memory used percentage history, 0..=100.
    pub mem_hist: History,
    pub mem_available: u64,
    /// Cached + reclaimable + buffers (reclaimable page cache), bytes.
    pub mem_cached: u64,
    pub load: [f32; 3],
    /// Package temperature (°C) and average current clock (MHz), if available.
    pub temp_c: Option<f32>,
    pub freq_mhz: Option<f32>,
    pub uptime_secs: u64,
    pub tasks_total: u32,
    pub tasks_running: u32,
    /// CPU topology grouped by physical core, ordered by (socket, core).
    pub core_groups: Vec<CoreGroup>,
}

/// One physical core: its socket (package) id and the logical-CPU indices of
/// its threads (1 = no SMT, 2 = hyper-threaded, etc.).
#[derive(Clone, Debug)]
pub struct CoreGroup {
    pub package: i64,
    pub cpus: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct GpuSnapshot {
    pub vendor: GpuVendor,
    pub index: usize,
    pub name: String,
    /// Utilization, 0..=100.
    pub busy_pct: f32,
    pub util_hist: History,
    pub mem_used: u64,
    pub mem_total: u64,
    /// APU shared (GTT) memory used/total, when applicable.
    pub gtt: Option<(u64, u64)>,
    /// VRAM used percentage history, 0..=100.
    pub vram_hist: History,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    pub sclk_mhz: Option<u32>,
    pub mclk_mhz: Option<u32>,
    pub fan: Option<Fan>,
    /// PCIe throughput (bytes/s); NVIDIA via NVML.
    pub pcie_rx_bps: Option<f64>,
    pub pcie_tx_bps: Option<f64>,
    /// Negotiated PCIe link width (lanes).
    pub pcie_width: Option<u16>,
    /// Video encoder / decoder utilization (0..=100). NVIDIA only (NVENC/NVDEC
    /// via NVML); amdgpu doesn't expose VCN engine util through sysfs.
    pub enc_pct: Option<f32>,
    pub dec_pct: Option<f32>,
    /// AMD only: the GPU is runtime-suspended (D3cold), so SMU telemetry
    /// (utilization/temp/power/clock) is unavailable until it resumes.
    pub suspended: bool,
    /// Optional diagnostic note shown on the card (e.g. an unsupported
    /// gpu_metrics revision), so missing telemetry is explained, not silent.
    pub note: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NetSnapshot {
    pub iface: String,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_hist: History,
    pub tx_hist: History,
    pub up: bool,
    /// Link speed in Mbps, when reported.
    pub speed_mbps: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct DiskSnapshot {
    pub dev: String,
    pub r_bps: f64,
    pub w_bps: f64,
    pub r_hist: History,
    pub w_hist: History,
    /// Fraction of wall time the device was busy (0..=100).
    pub util_pct: f32,
    pub r_iops: f64,
    pub w_iops: f64,
}

#[derive(Clone, Debug)]
pub struct FsSnapshot {
    pub mount: String,
    pub used: u64,
    pub total: u64,
}

/// One process row for the Processes tab.
#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: i32,
    pub name: String,
    /// CPU usage, single-core normalized (100 = one full core).
    pub cpu_pct: f32,
    /// Resident set size in bytes.
    pub rss: u64,
    pub state: char,
    /// Disk read/write rate in bytes/s (actual block I/O; cached/buffered I/O
    /// reads as 0). Meaningful only when `io_ok`.
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    /// Whether `/proc/<pid>/io` was readable (false = permission denied, so the
    /// disk figures are unknown rather than zero).
    pub io_ok: bool,
}

/// Per-process GPU usage, aggregated across the GPUs a process touches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuProcUse {
    /// GPU-addressable memory used by the process (dedicated VRAM or shared
    /// system memory for an integrated GPU), in bytes.
    pub vram: u64,
    /// GPU utilization attributed to the process (percent; NVIDIA SM% or DRM
    /// engine busy). 0 when unavailable.
    pub util_pct: f32,
    /// Which GPU(s), e.g. "N0" (NVIDIA 0), "A0" (AMD 0), or "I0" (Intel 0).
    pub label: String,
}

/// Lock-free shared state: one `ArcSwapOption` slot per collector source,
/// each timestamped at publish.
#[derive(Default)]
pub struct SharedState {
    pub cpu: ArcSwapOption<Stamped<CpuSnapshot>>,
    pub amd: ArcSwapOption<Stamped<Vec<GpuSnapshot>>>,
    pub intel: ArcSwapOption<Stamped<Vec<GpuSnapshot>>>,
    pub nvidia: ArcSwapOption<Stamped<Vec<GpuSnapshot>>>,
    pub net: ArcSwapOption<Stamped<Vec<NetSnapshot>>>,
    pub disk: ArcSwapOption<Stamped<Vec<DiskSnapshot>>>,
    pub fs: ArcSwapOption<Stamped<Vec<FsSnapshot>>>,
    pub procs: ArcSwapOption<Stamped<Vec<ProcInfo>>>,
    /// pid -> aggregated GPU usage (NVML + DRM fdinfo).
    pub gpu_procs: ArcSwapOption<Stamped<HashMap<i32, GpuProcUse>>>,
}
