//! Shared data model: snapshot types published by collectors and read by the UI.
//!
//! Each collector owns its own history ring buffers and publishes a fresh,
//! history-bearing snapshot into `SharedState` via `ArcSwap` (lock-free). The UI
//! renders whatever is latest at its own frame rate, decoupled from collection.

use std::collections::VecDeque;

use arc_swap::ArcSwapOption;

/// Number of samples retained per time-series (~120 points is ~1 KB/series).
pub const HIST_CAP: usize = 120;

/// A fixed-capacity ring buffer of `f64` samples for time-series charts.
#[derive(Clone, Debug, Default)]
pub struct History {
    buf: VecDeque<f64>,
}

impl History {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(HIST_CAP),
        }
    }

    pub fn push(&mut self, v: f64) {
        if self.buf.len() == HIST_CAP {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    pub fn max(&self) -> f64 {
        self.buf.iter().copied().fold(0.0_f64, f64::max)
    }

    /// `(x, y)` points for a ratatui `Chart` over a fixed `0..HIST_CAP` x-axis.
    ///
    /// Points are right-aligned: the newest sample sits at the right edge
    /// (`x = HIST_CAP - 1`) and older samples extend left, leaving the left
    /// blank until the buffer fills. This keeps "newest on the right" stable
    /// instead of rescaling the whole graph while history accumulates.
    pub fn points(&self) -> Vec<(f64, f64)> {
        let offset = HIST_CAP - self.buf.len();
        self.buf
            .iter()
            .enumerate()
            .map(|(i, &v)| ((offset + i) as f64, v))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
}

/// Fan reading, normalized per vendor (NVIDIA reports %, AMD reports RPM).
#[derive(Clone, Copy, Debug)]
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

/// Lock-free shared state: one `ArcSwapOption` slot per collector source.
#[derive(Default)]
pub struct SharedState {
    pub cpu: ArcSwapOption<CpuSnapshot>,
    pub amd: ArcSwapOption<Vec<GpuSnapshot>>,
    pub nvidia: ArcSwapOption<Vec<GpuSnapshot>>,
    pub net: ArcSwapOption<Vec<NetSnapshot>>,
    pub disk: ArcSwapOption<Vec<DiskSnapshot>>,
    pub fs: ArcSwapOption<Vec<FsSnapshot>>,
}
