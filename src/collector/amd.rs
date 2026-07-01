//! AMD GPU collector via the `amdgpu` DRM sysfs interface.
//!
//! Deliberately does NOT use ROCm SMI: reading sysfs directly covers consumer
//! Radeon cards and APUs that ROCm SMI refuses to enumerate (the reason btop
//! can't see them). Validated against a local Barcelo APU.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<Vec<GpuSnapshot>> {
        let mut out = Vec::new();
        for (idx, dev) in enumerate_amdgpu_cards().into_iter().enumerate() {
            let key = dev.to_string_lossy().into_owned();

            // Prefer the binary gpu_metrics table: on newer discrete cards
            // (e.g. RDNA4 / Navi 48) the legacy gpu_busy_percent and hwmon
            // sensors return EBUSY, but gpu_metrics is populated. Fall back to
            // the legacy sysfs nodes (which APUs expose) when metrics are absent.
            let metrics_res = read_gpu_metrics(&dev);
            let metrics = metrics_res.as_ref().ok().copied();

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
            let fan = m.and_then(|m| m.fan_rpm).map(Fan::Rpm).or(hw_fan);
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
            // Explain missing telemetry from an undecodable metrics revision.
            let note = match metrics_res {
                Err(GpuMetricsErr::Unsupported { format, content })
                    if temp_c.is_none() && power_w.is_none() && !suspended =>
                {
                    Some(format!("gpu_metrics v{format}.{content} unsupported"))
                }
                _ => None,
            };

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
                // amdgpu doesn't expose VCN enc/dec engine util via sysfs.
                enc_pct: None,
                dec_pct: None,
                suspended,
                note,
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
    // PCI_ID is "VVVV:DDDD" (hex). Resolve the device id to a marketing name via
    // the pci.ids database (bundled AMD subset + system copy); fall back to the
    // raw id only when even that misses.
    if let Some((_vendor, dev_id)) = pci_id.split_once(':')
        && let Ok(id) = u16::from_str_radix(dev_id.trim(), 16)
        && let Some(name) = amd_pci_names().get(&id)
    {
        return name.clone();
    }
    format!("AMD {pci_id}")
}

/// Standard locations for the `pci.ids` database (`lspci`'s name source).
const PCI_IDS_PATHS: &[&str] = &[
    "/usr/share/hwdata/pci.ids",
    "/usr/share/misc/pci.ids",
    "/var/lib/pciutils/pci.ids",
];

/// AMD (vendor `0x1002`) device-id → marketing name. amdgpu's vendor is always
/// 0x1002, so we only scan that one block rather than the whole ~40k-line table.
///
/// Built once from a bundled AMD-only snapshot of pci.ids, then overlaid with
/// the host's system `pci.ids` (system wins where it has an id, snapshot fills
/// the rest). The bundle guarantees names for recent APU iGPUs (Barcelo,
/// Phoenix, Raphael, Rembrandt, …) even on hosts whose system pci.ids predates
/// that hardware — without it those cards fall back to the raw PCI id.
fn amd_pci_names() -> &'static HashMap<u16, String> {
    /// AMD subset of pci.ids compiled into the binary; see the file header.
    const BUNDLED_AMD_PCI_IDS: &str = include_str!("pci_ids_amd.txt");
    static NAMES: OnceLock<HashMap<u16, String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut map = parse_pci_ids_vendor(BUNDLED_AMD_PCI_IDS, 0x1002);
        if let Some(system) = PCI_IDS_PATHS
            .iter()
            .find_map(|p| fs::read_to_string(p).ok())
        {
            map.extend(parse_pci_ids_vendor(&system, 0x1002));
        }
        map
    })
}

/// Parse device-id → name for a single vendor block of a `pci.ids` file.
///
/// Format: vendor lines have no indent (`1002  AMD/ATI`), device lines are
/// one-tab indented (`\t7551  Navi 48 [...]`), subsystem lines are two-tab
/// (ignored). We start collecting at the target vendor and stop at the next
/// unindented (vendor) line.
fn parse_pci_ids_vendor(content: &str, vendor: u16) -> HashMap<u16, String> {
    let mut map = HashMap::new();
    let mut in_vendor = false;
    for line in content.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('\t') {
            // Two-tab lines are subsystem entries; skip them.
            if !in_vendor || rest.starts_with('\t') {
                continue;
            }
            if let Some((id, name)) = rest.split_once(char::is_whitespace)
                && let Ok(d) = u16::from_str_radix(id.trim(), 16)
            {
                map.insert(d, marketing_name(name.trim()));
            }
        } else {
            // Unindented = vendor line. Leaving our block ends the scan.
            if in_vendor {
                break;
            }
            in_vendor = line
                .split_once(char::is_whitespace)
                .and_then(|(id, _)| u16::from_str_radix(id.trim(), 16).ok())
                == Some(vendor);
        }
    }
    map
}

/// Prefer the bracketed board name ("Navi 48 [Radeon AI PRO R9700]" →
/// "Radeon AI PRO R9700"), which is the user-facing marketing name; otherwise
/// use the chip name as-is ("Barcelo").
fn marketing_name(s: &str) -> String {
    match (s.find('['), s.rfind(']')) {
        (Some(o), Some(c)) if c > o + 1 => s[o + 1..c].to_string(),
        _ => s.to_string(),
    }
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
#[derive(Debug, Clone, Copy, PartialEq)]
struct GpuMetrics {
    gfx_activity: f32,
    temp_c: Option<f32>,
    power_w: Option<f32>,
    sclk_mhz: Option<u32>,
    mclk_mhz: Option<u32>,
    fan_rpm: Option<u32>,
    pcie_width: Option<u16>,
}

/// Why a `gpu_metrics` read produced no usable data (surfaced as status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMetricsErr {
    /// Node missing or read failed (e.g. EBUSY while runtime-suspended).
    Io,
    TooShort,
    /// A revision we don't decode (e.g. APU v2_x, or newer v1_4/v1_5).
    Unsupported {
        format: u8,
        content: u8,
    },
    /// Table present but all-zero (not yet populated).
    Empty,
}

fn read_gpu_metrics(dev: &Path) -> Result<GpuMetrics, GpuMetricsErr> {
    // Must be read with a single read() syscall: read_to_end would issue a
    // second read that re-triggers the SMU and returns EBUSY, failing the whole
    // call. Read once into a fixed buffer instead.
    use std::io::Read;
    let mut f = fs::File::open(dev.join("gpu_metrics")).map_err(|_| GpuMetricsErr::Io)?;
    let mut buf = [0u8; 256];
    let n = f.read(&mut buf).map_err(|_| GpuMetricsErr::Io)?;
    parse_gpu_metrics(&buf[..n])
}

/// Pure decoder for the binary `gpu_metrics_v1_x` table (discrete GPUs).
///
/// v1_3 layout (little-endian): 0 u16 structure_size, 2/3 u8 format/content rev,
/// 4 temperature_edge, 6 hotspot, 16 average_gfx_activity, 22 average_socket_power,
/// 40 average_gfxclk, 54 current_gfxclk, 58 current_uclk, 72 fan, 74 pcie_link_width.
/// Offsets are stable for content revisions 1..=3; other revisions are rejected.
fn parse_gpu_metrics(b: &[u8]) -> Result<GpuMetrics, GpuMetricsErr> {
    if b.len() < 76 {
        return Err(GpuMetricsErr::TooShort);
    }
    let format_rev = b[2];
    let content_rev = b[3];
    if format_rev != 1 || !(1..=3).contains(&content_rev) {
        return Err(GpuMetricsErr::Unsupported {
            format: format_rev,
            content: content_rev,
        });
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
        return Err(GpuMetricsErr::Empty);
    }

    let temp = if temp_hotspot > 0 {
        temp_hotspot
    } else {
        temp_edge
    };
    let sclk = if cur_gfxclk > 0 {
        cur_gfxclk
    } else {
        avg_gfxclk
    };

    Ok(GpuMetrics {
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
    parse_current_clock(&fs::read_to_string(path).ok()?)
}

/// Pure: extract the MHz value from the `*`-marked line of a `pp_dpm_*` table.
fn parse_current_clock(content: &str) -> Option<u32> {
    let line = content.lines().find(|l| l.trim_end().ends_with('*'))?;
    // e.g. "1: 400Mhz *"
    line.split_whitespace()
        .find_map(|tok| tok.to_lowercase().strip_suffix("mhz").map(str::to_string))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic gpu_metrics v1_3 buffer with known field values.
    fn metrics_buf() -> [u8; 120] {
        let mut b = [0u8; 120];
        b[2] = 1; // format_revision
        b[3] = 3; // content_revision
        for (off, v) in [
            (0u16, 120u16), // structure_size
            (4, 39),        // temperature_edge
            (6, 40),        // temperature_hotspot
            (16, 25),       // average_gfx_activity
            (22, 12),       // average_socket_power
            (40, 2000),     // average_gfxclk
            (54, 2500),     // current_gfxclk
            (58, 96),       // current_uclk
            (72, 890),      // current_fan_speed
            (74, 16),       // pcie_link_width
        ] {
            let o = off as usize;
            b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn gpu_metrics_v1_3_decodes_fields() {
        let m = parse_gpu_metrics(&metrics_buf()).expect("should decode");
        assert_eq!(m.gfx_activity, 25.0);
        assert_eq!(m.temp_c, Some(40.0)); // hotspot preferred over edge
        assert_eq!(m.power_w, Some(12.0));
        assert_eq!(m.sclk_mhz, Some(2500)); // current preferred over average
        assert_eq!(m.mclk_mhz, Some(96));
        assert_eq!(m.fan_rpm, Some(890));
        assert_eq!(m.pcie_width, Some(16));
    }

    #[test]
    fn gpu_metrics_falls_back_to_edge_and_avg_clk() {
        let mut b = metrics_buf();
        b[6..8].copy_from_slice(&0u16.to_le_bytes()); // no hotspot
        b[54..56].copy_from_slice(&0u16.to_le_bytes()); // no current gfxclk
        let m = parse_gpu_metrics(&b).unwrap();
        assert_eq!(m.temp_c, Some(39.0)); // edge
        assert_eq!(m.sclk_mhz, Some(2000)); // average
    }

    #[test]
    fn gpu_metrics_rejects_unsupported_and_empty() {
        let mut b = metrics_buf();
        b[3] = 4; // content_revision 4 (v1_4)
        assert_eq!(
            parse_gpu_metrics(&b),
            Err(GpuMetricsErr::Unsupported {
                format: 1,
                content: 4
            })
        );

        let mut apu = metrics_buf();
        apu[2] = 2; // format_revision 2 (APU)
        assert!(matches!(
            parse_gpu_metrics(&apu),
            Err(GpuMetricsErr::Unsupported { .. })
        ));

        let mut empty = [0u8; 120];
        empty[2] = 1; // valid v1_3 header but all-zero metrics
        empty[3] = 3;
        assert_eq!(parse_gpu_metrics(&empty), Err(GpuMetricsErr::Empty));
        assert_eq!(parse_gpu_metrics(&[0u8; 10]), Err(GpuMetricsErr::TooShort));
    }

    #[test]
    fn current_clock_picks_starred_line() {
        let table = "0: 200Mhz \n1: 400Mhz *\n2: 2000Mhz ";
        assert_eq!(parse_current_clock(table), Some(400));
        assert_eq!(parse_current_clock("0: 200Mhz \n1: 400Mhz "), None);
    }

    #[test]
    fn pci_ids_vendor_block_isolates_devices() {
        // Two vendors, with subsystem lines and a comment interleaved.
        let db = "\
# comment line
1001  Other Vendor
\t1234  Should Not Appear
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t7551  Navi 48 [Radeon AI PRO R9700]
\t\t1043 8950  Subsystem Should Be Skipped
\t15e7  Barcelo
10de  NVIDIA Corporation
\t2504  GA106 [GeForce RTX 3060]
";
        let amd = parse_pci_ids_vendor(db, 0x1002);
        // Bracketed board name extracted.
        assert_eq!(
            amd.get(&0x7551).map(String::as_str),
            Some("Radeon AI PRO R9700")
        );
        // No brackets → chip name verbatim.
        assert_eq!(amd.get(&0x15e7).map(String::as_str), Some("Barcelo"));
        // Other vendors and subsystem (two-tab) lines are excluded.
        assert!(!amd.contains_key(&0x1234));
        assert!(!amd.contains_key(&0x2504));
        assert!(!amd.contains_key(&0x8950));
        assert_eq!(amd.len(), 2);
    }

    #[test]
    fn bundled_pci_ids_resolves_apu_igpus() {
        // The compiled-in AMD subset must name recent APU iGPUs on its own, so
        // hosts with a stale/absent system pci.ids don't fall back to raw hex.
        let amd = parse_pci_ids_vendor(include_str!("pci_ids_amd.txt"), 0x1002);
        assert_eq!(amd.get(&0x15e7).map(String::as_str), Some("Barcelo")); // 5825U
        assert_eq!(amd.get(&0x1681).map(String::as_str), Some("Radeon 680M")); // Rembrandt
        assert_eq!(amd.get(&0x164e).map(String::as_str), Some("Raphael"));
        assert!(
            amd.len() > 100,
            "expected the full AMD block, got {}",
            amd.len()
        );
    }

    #[test]
    fn marketing_name_prefers_bracketed_board() {
        assert_eq!(
            marketing_name("Navi 48 [Radeon AI PRO R9700]"),
            "Radeon AI PRO R9700"
        );
        assert_eq!(marketing_name("Barcelo"), "Barcelo");
        // Degenerate brackets fall back to the original string.
        assert_eq!(marketing_name("Weird []"), "Weird []");
    }
}
