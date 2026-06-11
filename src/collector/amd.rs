//! AMD GPU collector via the `amdgpu` DRM sysfs interface.
//!
//! Deliberately does NOT use ROCm SMI: reading sysfs directly covers consumer
//! Radeon cards and APUs that ROCm SMI refuses to enumerate (the reason btop
//! can't see them). Validated against a local Barcelo APU.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Collector;
use crate::model::{Fan, GpuSnapshot, GpuVendor, History};

#[derive(Default)]
struct Hist {
    util: History,
    vram: History,
}

pub struct AmdCollector {
    hist: HashMap<String, Hist>,
}

impl AmdCollector {
    pub fn new() -> Self {
        Self {
            hist: HashMap::new(),
        }
    }
}

impl Collector for AmdCollector {
    type Out = Vec<GpuSnapshot>;

    fn name(&self) -> &'static str {
        "amd"
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn sample(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
        let mut out = Vec::new();
        for (idx, dev) in enumerate_amdgpu_cards().into_iter().enumerate() {
            let key = dev.to_string_lossy().into_owned();

            // Prefer the binary gpu_metrics table: on newer discrete cards
            // (e.g. RDNA4 / Navi 48) the legacy gpu_busy_percent and hwmon
            // sensors return EBUSY, but gpu_metrics is populated. Fall back to
            // the legacy sysfs nodes (which APUs expose) when metrics are absent.
            let metrics = read_gpu_metrics(&dev);

            let mem_used = read_u64(dev.join("mem_info_vram_used")).unwrap_or(0);
            let mem_total = read_u64(dev.join("mem_info_vram_total")).unwrap_or(0);
            let gtt = match (
                read_u64(dev.join("mem_info_gtt_used")),
                read_u64(dev.join("mem_info_gtt_total")),
            ) {
                (Some(u), Some(t)) if t > 0 => Some((u, t)),
                _ => None,
            };

            let (hw_temp, hw_power, hw_fan) = read_hwmon(&dev);
            let m = metrics.as_ref();
            let busy_pct = m
                .map(|m| m.gfx_activity)
                .or_else(|| read_u64(dev.join("gpu_busy_percent")).map(|v| v as f32))
                .unwrap_or(0.0);
            let temp_c = m.and_then(|m| m.temp_c).or(hw_temp);
            let power_w = m.and_then(|m| m.power_w).or(hw_power);
            let fan = m
                .and_then(|m| m.fan_rpm)
                .map(Fan::Rpm)
                .or(hw_fan);
            let sclk_mhz = m
                .and_then(|m| m.sclk_mhz)
                .or_else(|| read_current_clock(dev.join("pp_dpm_sclk")));
            let mclk_mhz = m
                .and_then(|m| m.mclk_mhz)
                .or_else(|| read_current_clock(dev.join("pp_dpm_mclk")));
            let name = read_name(&dev);
            let suspended = fs::read_to_string(dev.join("power/runtime_status"))
                .map(|s| s.trim() == "suspended")
                .unwrap_or(false);

            let h = self.hist.entry(key).or_default();
            h.util.push(busy_pct as f64);
            let vram_pct = if mem_total > 0 {
                100.0 * mem_used as f64 / mem_total as f64
            } else {
                0.0
            };
            h.vram.push(vram_pct);

            out.push(GpuSnapshot {
                vendor: GpuVendor::Amd,
                index: idx,
                name,
                busy_pct,
                util_hist: h.util.clone(),
                mem_used,
                mem_total,
                gtt,
                vram_hist: h.vram.clone(),
                temp_c,
                power_w,
                sclk_mhz,
                mclk_mhz,
                fan,
                pcie_rx_bps: None,
                pcie_tx_bps: None,
                pcie_width: m.and_then(|m| m.pcie_width),
                suspended,
            });
        }
        Ok(out)
    }
}

/// Find `/sys/class/drm/cardN/device` dirs whose driver is `amdgpu`,
/// skipping connector entries like `card1-DP-1`.
fn enumerate_amdgpu_cards() -> Vec<PathBuf> {
    let mut cards = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return cards;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // ^card\d+$
        if let Some(num) = name.strip_prefix("card") {
            if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
        } else {
            continue;
        }
        let dev = entry.path().join("device");
        let uevent = fs::read_to_string(dev.join("uevent")).unwrap_or_default();
        if uevent.lines().any(|l| l == "DRIVER=amdgpu") {
            cards.push(dev);
        }
    }
    cards.sort();
    cards
}

fn read_u64(path: PathBuf) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_name(dev: &Path) -> String {
    let uevent = fs::read_to_string(dev.join("uevent")).unwrap_or_default();
    let pci_id = uevent
        .lines()
        .find_map(|l| l.strip_prefix("PCI_ID="))
        .unwrap_or("?");
    format!("AMD {pci_id}")
}

/// Returns `(temp_c, power_w, fan)` from the card's `amdgpu` hwmon node.
fn read_hwmon(dev: &Path) -> (Option<f32>, Option<f32>, Option<Fan>) {
    let Ok(entries) = fs::read_dir(dev.join("hwmon")) else {
        return (None, None, None);
    };
    for entry in entries.flatten() {
        let h = entry.path();
        let name = fs::read_to_string(h.join("name")).unwrap_or_default();
        if name.trim() != "amdgpu" {
            continue;
        }
        let temp_c = read_u64(h.join("temp1_input")).map(|m| m as f32 / 1000.0);
        let power_w = read_u64(h.join("power1_average")).map(|u| u as f32 / 1_000_000.0);
        let fan = read_u64(h.join("fan1_input")).map(|r| Fan::Rpm(r as u32));
        return (temp_c, power_w, fan);
    }
    (None, None, None)
}

/// Selected fields decoded from the binary `gpu_metrics` sysfs node.
struct GpuMetrics {
    gfx_activity: f32,
    temp_c: Option<f32>,
    power_w: Option<f32>,
    sclk_mhz: Option<u32>,
    mclk_mhz: Option<u32>,
    fan_rpm: Option<u32>,
    pcie_width: Option<u16>,
}

/// Decode the `gpu_metrics_v1_x` table (discrete GPUs). The leading
/// header + temperature + activity + power + clock + fan offsets are stable
/// across content revisions 1..=3; APU tables (format_revision 2) and unknown
/// revisions are rejected so the caller falls back to legacy sysfs nodes.
///
/// v1_3 layout (little-endian, offsets from start):
///   0  u16 structure_size, 2 u8 format_rev, 3 u8 content_rev
///   4  u16 temperature_edge, 6 hotspot, 8 mem, 10 vrgfx, 12 vrsoc, 14 vrmem
///   16 u16 average_gfx_activity, 18 umc, 20 mm
///   22 u16 average_socket_power
///   40 u16 average_gfxclk_frequency ... 44 average_uclk_frequency
///   54 u16 current_gfxclk, 58 current_uclk
///   72 u16 current_fan_speed
fn read_gpu_metrics(dev: &Path) -> Option<GpuMetrics> {
    // Must be read with a single read() syscall: read_to_end would issue a
    // second read that re-triggers the SMU and returns EBUSY, failing the whole
    // call. Read once into a fixed buffer instead.
    use std::io::Read;
    let mut f = fs::File::open(dev.join("gpu_metrics")).ok()?;
    let mut buf = [0u8; 256];
    let n = f.read(&mut buf).ok()?;
    let b = &buf[..n];
    if b.len() < 76 {
        return None;
    }
    let format_rev = b[2];
    let content_rev = b[3];
    if format_rev != 1 || !(1..=3).contains(&content_rev) {
        return None;
    }
    let u16le = |o: usize| -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) };

    let temp_edge = u16le(4);
    let temp_hotspot = u16le(6);
    let gfx = u16le(16);
    let power = u16le(22);
    let avg_gfxclk = u16le(40);
    let cur_gfxclk = u16le(54);
    let cur_uclk = u16le(58);
    let fan = u16le(72);
    let pcie_width = u16le(74);

    // All-zero readings mean the table isn't populated; let the caller fall back.
    if temp_edge == 0 && temp_hotspot == 0 && power == 0 && fan == 0 && gfx == 0 {
        return None;
    }

    let temp = if temp_hotspot > 0 { temp_hotspot } else { temp_edge };
    let sclk = if cur_gfxclk > 0 { cur_gfxclk } else { avg_gfxclk };

    Some(GpuMetrics {
        gfx_activity: gfx as f32,
        temp_c: (temp > 0).then_some(temp as f32),
        power_w: (power > 0).then_some(power as f32),
        sclk_mhz: (sclk > 0).then_some(sclk as u32),
        mclk_mhz: (cur_uclk > 0).then_some(cur_uclk as u32),
        fan_rpm: (fan > 0).then_some(fan as u32),
        pcie_width: (pcie_width > 0).then_some(pcie_width),
    })
}

/// Parse the current frequency (the line ending with `*`) from a `pp_dpm_*` file.
fn read_current_clock(path: PathBuf) -> Option<u32> {
    let content = fs::read_to_string(path).ok()?;
    let line = content.lines().find(|l| l.trim_end().ends_with('*'))?;
    // e.g. "1: 400Mhz *"
    let mhz = line
        .split_whitespace()
        .find_map(|tok| tok.to_lowercase().strip_suffix("mhz").map(str::to_string))?;
    mhz.parse().ok()
}
