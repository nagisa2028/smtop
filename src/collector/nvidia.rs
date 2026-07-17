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

#[derive(Default)]
struct NvidiaReadings {
    name: String,
    busy_pct: f32,
    mem_used: u64,
    mem_total: u64,
    temp_c: Option<f32>,
    power_w: Option<f32>,
    sclk_mhz: Option<u32>,
    mclk_mhz: Option<u32>,
    fan: Option<Fan>,
    pcie_rx_bps: Option<f64>,
    pcie_tx_bps: Option<f64>,
    pcie_width: Option<u16>,
    enc_pct: Option<f32>,
    dec_pct: Option<f32>,
}

trait NvidiaSource {
    fn device_count(&self) -> u32;
    fn readings(&self, index: u32) -> Option<NvidiaReadings>;
}

struct NvmlSource<'a>(&'a Nvml);

impl NvidiaSource for NvmlSource<'_> {
    fn device_count(&self) -> u32 {
        self.0.device_count().unwrap_or(0)
    }

    fn readings(&self, index: u32) -> Option<NvidiaReadings> {
        let dev = self.0.device_by_index(index).ok()?;
        let name = dev.name().unwrap_or_else(|_| format!("NVIDIA GPU {index}"));
        let busy_pct = dev.utilization_rates().map(|u| u.gpu as f32).unwrap_or(0.0);
        let (mem_used, mem_total) = dev
            .memory_info()
            .map(|m| (m.used, m.total))
            .unwrap_or((0, 0));
        Some(NvidiaReadings {
            name,
            busy_pct,
            mem_used,
            mem_total,
            temp_c: dev
                .temperature(TemperatureSensor::Gpu)
                .ok()
                .map(|t| t as f32),
            power_w: dev.power_usage().ok().map(|mw| mw as f32 / 1000.0),
            sclk_mhz: dev.clock_info(Clock::Graphics).ok(),
            mclk_mhz: dev.clock_info(Clock::Memory).ok(),
            fan: dev.fan_speed(0).ok().map(|p| Fan::Pct(p as f32)),
            pcie_rx_bps: dev
                .pcie_throughput(PcieUtilCounter::Receive)
                .ok()
                .map(|kb| kb as f64 * 1024.0),
            pcie_tx_bps: dev
                .pcie_throughput(PcieUtilCounter::Send)
                .ok()
                .map(|kb| kb as f64 * 1024.0),
            pcie_width: dev.current_pcie_link_width().ok().map(|w| w as u16),
            enc_pct: dev.encoder_utilization().ok().map(|u| u.utilization as f32),
            dec_pct: dev.decoder_utilization().ok().map(|u| u.utilization as f32),
        })
    }
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
        Ok(collect_from_source(&NvmlSource(nvml), &mut self.hist))
    }
}

fn collect_from_source(
    source: &impl NvidiaSource,
    histories: &mut HashMap<u32, Hist>,
) -> Vec<GpuSnapshot> {
    (0..source.device_count())
        .filter_map(|index| {
            source
                .readings(index)
                .map(|readings| snapshot_from_readings(index, histories, readings))
        })
        .collect()
}

fn snapshot_from_readings(
    index: u32,
    histories: &mut HashMap<u32, Hist>,
    readings: NvidiaReadings,
) -> GpuSnapshot {
    let h = histories.entry(index).or_default();
    h.util.push(readings.busy_pct as f64);
    let vram_pct = if readings.mem_total > 0 {
        100.0 * readings.mem_used as f64 / readings.mem_total as f64
    } else {
        0.0
    };
    h.vram.push(vram_pct);

    GpuSnapshot {
        vendor: GpuVendor::Nvidia,
        index: index as usize,
        name: readings.name,
        busy_pct: readings.busy_pct,
        util_hist: h.util.clone(),
        mem_used: readings.mem_used,
        mem_total: readings.mem_total,
        gtt: None,
        vram_hist: h.vram.clone(),
        temp_c: readings.temp_c,
        power_w: readings.power_w,
        sclk_mhz: readings.sclk_mhz,
        mclk_mhz: readings.mclk_mhz,
        fan: readings.fan,
        pcie_rx_bps: readings.pcie_rx_bps,
        pcie_tx_bps: readings.pcie_tx_bps,
        pcie_width: readings.pcie_width,
        enc_pct: readings.enc_pct,
        dec_pct: readings.dec_pct,
        suspended: false,
        note: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_snapshots_handle_partial_metrics_and_per_gpu_histories() {
        let mut histories = HashMap::new();
        let missing = snapshot_from_readings(
            0,
            &mut histories,
            NvidiaReadings {
                name: "fallback".into(),
                mem_used: 100,
                mem_total: 0,
                ..NvidiaReadings::default()
            },
        );
        assert_eq!(missing.index, 0);
        assert_eq!(missing.vram_hist.points().last().map(|p| p.1), Some(0.0));
        assert!(missing.temp_c.is_none());

        let full = snapshot_from_readings(
            1,
            &mut histories,
            NvidiaReadings {
                name: "RTX".into(),
                busy_pct: 75.0,
                mem_used: 3,
                mem_total: 4,
                temp_c: Some(60.0),
                power_w: Some(120.0),
                pcie_rx_bps: Some(1024.0),
                enc_pct: Some(10.0),
                ..NvidiaReadings::default()
            },
        );
        assert_eq!(full.util_hist.points().last().map(|p| p.1), Some(75.0));
        assert_eq!(full.vram_hist.points().last().map(|p| p.1), Some(75.0));
        assert_eq!(full.temp_c, Some(60.0));
        assert_eq!(full.pcie_rx_bps, Some(1024.0));
        assert_eq!(histories.len(), 2);
    }

    struct MockSource(Vec<Option<NvidiaReadings>>);

    impl NvidiaSource for MockSource {
        fn device_count(&self) -> u32 {
            self.0.len() as u32
        }

        fn readings(&self, index: u32) -> Option<NvidiaReadings> {
            self.0[index as usize].as_ref().map(|r| NvidiaReadings {
                name: r.name.clone(),
                busy_pct: r.busy_pct,
                mem_used: r.mem_used,
                mem_total: r.mem_total,
                ..NvidiaReadings::default()
            })
        }
    }

    #[test]
    fn mocked_nvml_source_skips_failed_devices_and_preserves_indices() {
        let source = MockSource(vec![
            Some(NvidiaReadings {
                name: "GPU0".into(),
                busy_pct: 10.0,
                mem_total: 100,
                ..NvidiaReadings::default()
            }),
            None,
            Some(NvidiaReadings {
                name: "GPU2".into(),
                busy_pct: 20.0,
                mem_total: 200,
                ..NvidiaReadings::default()
            }),
        ]);
        let snapshots = collect_from_source(&source, &mut HashMap::new());
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].index, 0);
        assert_eq!(snapshots[1].index, 2);
        assert_eq!(snapshots[1].name, "GPU2");
    }
}
