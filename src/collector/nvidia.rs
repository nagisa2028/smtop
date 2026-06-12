//! NVIDIA GPU collector via NVML (`nvml-wrapper`).
//!
//! NVML is the only stable interface the proprietary/open NVIDIA driver exposes
//! for these metrics (sysfs does not carry them). `nvml-wrapper` dlopens
//! `libnvidia-ml` at runtime, so this builds and runs fine on machines without
//! the driver — `init` simply fails and we publish an empty list.

use std::collections::HashMap;
use std::time::Duration;

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor};

use super::Collector;
use crate::model::{Fan, GpuSnapshot, GpuVendor, History};

#[derive(Default)]
struct Hist {
    util: History,
    vram: History,
}

pub struct NvidiaCollector {
    nvml: Option<Nvml>,
    hist: HashMap<u32, Hist>,
}

impl NvidiaCollector {
    pub fn new() -> Self {
        // A missing driver/library is expected on AMD-only hosts; not an error.
        let nvml = Nvml::init().ok();
        Self {
            nvml,
            hist: HashMap::new(),
        }
    }
}

impl Collector for NvidiaCollector {
    type Out = Vec<GpuSnapshot>;

    fn name(&self) -> &'static str {
        "nvidia"
    }

    fn interval(&self) -> Duration {
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
        let Some(nvml) = self.nvml.as_ref() else {
            return Ok(Vec::new());
        };
        let count = nvml.device_count().unwrap_or(0);
        let mut out = Vec::with_capacity(count as usize);

        for i in 0..count {
            let Ok(dev) = nvml.device_by_index(i) else {
                continue;
            };

            let name = dev.name().unwrap_or_else(|_| format!("NVIDIA GPU {i}"));
            let busy_pct = dev.utilization_rates().map(|u| u.gpu as f32).unwrap_or(0.0);
            let (mem_used, mem_total) = dev
                .memory_info()
                .map(|m| (m.used, m.total))
                .unwrap_or((0, 0));
            let temp_c = dev
                .temperature(TemperatureSensor::Gpu)
                .ok()
                .map(|t| t as f32);
            let power_w = dev.power_usage().ok().map(|mw| mw as f32 / 1000.0);
            let sclk_mhz = dev.clock_info(Clock::Graphics).ok();
            let mclk_mhz = dev.clock_info(Clock::Memory).ok();
            let fan = dev.fan_speed(0).ok().map(|p| Fan::Pct(p as f32));
            // NVML reports PCIe throughput in KB/s over a ~20ms window.
            let pcie_rx_bps = dev
                .pcie_throughput(PcieUtilCounter::Receive)
                .ok()
                .map(|kb| kb as f64 * 1024.0);
            let pcie_tx_bps = dev
                .pcie_throughput(PcieUtilCounter::Send)
                .ok()
                .map(|kb| kb as f64 * 1024.0);
            let pcie_width = dev.current_pcie_link_width().ok().map(|w| w as u16);

            let h = self.hist.entry(i).or_default();
            h.util.push(busy_pct as f64);
            let vram_pct = if mem_total > 0 {
                100.0 * mem_used as f64 / mem_total as f64
            } else {
                0.0
            };
            h.vram.push(vram_pct);

            out.push(GpuSnapshot {
                vendor: GpuVendor::Nvidia,
                index: i as usize,
                name,
                busy_pct,
                util_hist: h.util.clone(),
                mem_used,
                mem_total,
                gtt: None,
                vram_hist: h.vram.clone(),
                temp_c,
                power_w,
                sclk_mhz,
                mclk_mhz,
                fan,
                pcie_rx_bps,
                pcie_tx_bps,
                pcie_width,
                suspended: false,
                note: None,
            });
        }
        Ok(out)
    }
}
