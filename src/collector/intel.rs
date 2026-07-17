//! Intel GPU collector for the `i915` DRM driver.
//!
//! Stable sysfs provides identity, runtime-PM state and GT frequency. Overall
//! engine utilization comes from the i915 perf PMU when the process has access
//! (typically root/CAP_PERFMON or a permissive `perf_event_paranoid`). Systems
//! which deny PMU access still publish the GPU with an explanatory note.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::Collector;
use crate::model::{Fan, GpuSnapshot, GpuVendor, History};

#[derive(Default)]
struct Hist {
    util: History,
    memory: History,
}

struct Device {
    card: PathBuf,
    device: PathBuf,
    key: String,
    driver: String,
}

enum PmuState {
    Untried,
    Available(Vec<PerfCounter>),
    Unavailable,
}

pub struct IntelCollector {
    hist: HashMap<String, Hist>,
    pmu: PmuState,
    last: Option<Instant>,
}

impl IntelCollector {
    pub fn new() -> Self {
        Self {
            hist: HashMap::new(),
            pmu: PmuState::Untried,
            last: None,
        }
    }

    fn utilization(&mut self, dt: f64) -> Option<f32> {
        if matches!(self.pmu, PmuState::Untried) {
            self.pmu = open_i915_busy_counters()
                .map(PmuState::Available)
                .unwrap_or(PmuState::Unavailable);
        }
        let PmuState::Available(counters) = &mut self.pmu else {
            return None;
        };
        let mut busiest = 0.0_f32;
        for counter in counters {
            if let Some(delta) = counter.delta() {
                busiest = busiest.max((delta as f64 / (dt * 1e9) * 100.0).min(100.0) as f32);
            }
        }
        // A successfully opened PMU is available even on its first read, when
        // every counter is only establishing a baseline and utilization is 0.
        Some(busiest)
    }
}

impl Collector for IntelCollector {
    type Out = Vec<GpuSnapshot>;

    fn name(&self) -> &'static str {
        "intel"
    }

    fn interval(&self) -> Duration {
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<Self::Out> {
        let devices = enumerate_intel_cards();
        let now = Instant::now();
        let dt = self
            .last
            .map(|last| now.duration_since(last).as_secs_f64())
            .unwrap_or_else(|| self.interval().as_secs_f64());
        self.last = Some(now);
        // The legacy i915 PMU represents the single i915 device. Xe and
        // multi-i915 support can use their per-device PMUs when validated.
        let pmu_busy = (devices.len() == 1 && devices[0].driver == "i915")
            .then(|| self.utilization(dt))
            .flatten();
        Ok(devices
            .iter()
            .enumerate()
            .map(|(index, dev)| sample_device(index, dev, pmu_busy, &mut self.hist))
            .collect())
    }
}

fn enumerate_intel_cards() -> Vec<Device> {
    enumerate_intel_cards_at(Path::new("/sys/class/drm"))
}

fn enumerate_intel_cards_at(root: &Path) -> Vec<Device> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(number) = name.strip_prefix("card") else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let card = entry.path();
        let device = card.join("device");
        let uevent = fs::read_to_string(device.join("uevent")).unwrap_or_default();
        let driver = uevent
            .lines()
            .find_map(|line| line.strip_prefix("DRIVER="))
            .unwrap_or_default();
        let vendor = fs::read_to_string(device.join("vendor")).unwrap_or_default();
        if vendor.trim() != "0x8086" || !matches!(driver, "i915" | "xe") {
            continue;
        }
        let key = uevent
            .lines()
            .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
            .unwrap_or(&name)
            .to_string();
        out.push(Device {
            card,
            device,
            key,
            driver: driver.to_string(),
        });
    }
    out.sort_by(|a, b| a.card.cmp(&b.card));
    out
}

fn sample_device(
    index: usize,
    dev: &Device,
    pmu_busy: Option<f32>,
    histories: &mut HashMap<String, Hist>,
) -> GpuSnapshot {
    let suspended =
        read_text(dev.device.join("power/runtime_status")).as_deref() == Some("suspended");
    let busy_pct = pmu_busy.unwrap_or(0.0);
    let sclk_mhz = read_frequency(dev);
    let (temp_c, power_w, fan) = read_hwmon(&dev.device);
    let mem_used = read_u64(dev.device.join("mem_info_vram_used")).unwrap_or(0);
    let mem_total = read_u64(dev.device.join("mem_info_vram_total")).unwrap_or(0);
    let h = histories.entry(dev.key.clone()).or_default();
    h.util.push(busy_pct as f64);
    h.memory.push(if mem_total > 0 {
        mem_used as f64 / mem_total as f64 * 100.0
    } else {
        0.0
    });

    GpuSnapshot {
        vendor: GpuVendor::Intel,
        index,
        name: read_name(&dev.device),
        busy_pct,
        util_hist: h.util.clone(),
        mem_used,
        mem_total,
        gtt: None,
        vram_hist: h.memory.clone(),
        temp_c,
        power_w,
        sclk_mhz,
        mclk_mhz: None,
        fan,
        pcie_rx_bps: None,
        pcie_tx_bps: None,
        pcie_width: None,
        enc_pct: None,
        dec_pct: None,
        suspended,
        note: (pmu_busy.is_none() && !suspended).then(|| {
            if dev.driver == "i915" {
                "util requires CAP_PERFMON".to_string()
            } else {
                "xe utilization unavailable".to_string()
            }
        }),
    }
}

fn read_frequency(dev: &Device) -> Option<u32> {
    for path in [
        dev.card.join("gt/gt0/rps_act_freq_mhz"),
        dev.card.join("gt_act_freq_mhz"),
        dev.card.join("gt/gt0/rps_cur_freq_mhz"),
        dev.card.join("gt_cur_freq_mhz"),
    ] {
        if let Some(value) = read_u64(path)
            && value > 0
        {
            return Some(value as u32);
        }
    }
    (dev.driver == "xe")
        .then(|| read_xe_frequency(&dev.device))
        .flatten()
}

/// Xe exposes one frequency directory per GT under device/tile*/gt*/freq0.
/// Return the highest active GT clock, which is the most useful single value
/// for the existing per-card UI.
fn read_xe_frequency(device: &Path) -> Option<u32> {
    let mut highest = 0;
    let tiles = fs::read_dir(device).ok()?;
    for tile in tiles.flatten().filter(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .strip_prefix("tile")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }) {
        let Ok(gts) = fs::read_dir(tile.path()) else {
            continue;
        };
        for gt in gts.flatten().filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("gt")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        }) {
            for name in ["act_freq", "cur_freq"] {
                highest = highest.max(read_u64(gt.path().join("freq0").join(name)).unwrap_or(0));
            }
        }
    }
    (highest > 0).then_some(highest as u32)
}

fn read_name(device: &Path) -> String {
    let uevent = fs::read_to_string(device.join("uevent")).unwrap_or_default();
    let pci_id = uevent
        .lines()
        .find_map(|line| line.strip_prefix("PCI_ID="))
        .unwrap_or("8086:????");
    system_pci_name(pci_id).unwrap_or_else(|| format!("Intel {pci_id}"))
}

fn system_pci_name(pci_id: &str) -> Option<String> {
    let (_, device) = pci_id.split_once(':')?;
    let target = u16::from_str_radix(device, 16).ok()?;
    for path in [
        "/usr/share/hwdata/pci.ids",
        "/usr/share/misc/pci.ids",
        "/var/lib/pciutils/pci.ids",
    ] {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        if let Some(name) = parse_pci_name(&content, 0x8086, target) {
            return Some(name);
        }
    }
    None
}

fn parse_pci_name(content: &str, vendor: u16, device: u16) -> Option<String> {
    let mut in_vendor = false;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix('\t') {
            if in_vendor
                && !rest.starts_with('\t')
                && let Some((id, name)) = rest.split_once(char::is_whitespace)
                && u16::from_str_radix(id, 16).ok() == Some(device)
            {
                return Some(name.trim().to_string());
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            if in_vendor {
                break;
            }
            in_vendor = line
                .split_once(char::is_whitespace)
                .and_then(|(id, _)| u16::from_str_radix(id, 16).ok())
                == Some(vendor);
        }
    }
    None
}

fn read_hwmon(device: &Path) -> (Option<f32>, Option<f32>, Option<Fan>) {
    let Ok(entries) = fs::read_dir(device.join("hwmon")) else {
        return (None, None, None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = read_text(path.join("name")).unwrap_or_default();
        if !name.starts_with("i915") && !name.starts_with("xe") {
            continue;
        }
        let temp = read_u64(path.join("temp1_input")).map(|v| v as f32 / 1000.0);
        let power = read_u64(path.join("power1_average")).map(|v| v as f32 / 1_000_000.0);
        let fan = read_u64(path.join("fan1_input")).map(|v| Fan::Rpm(v as u32));
        return (temp, power, fan);
    }
    (None, None, None)
}

fn read_text(path: PathBuf) -> Option<String> {
    Some(fs::read_to_string(path).ok()?.trim().to_string())
}

fn read_u64(path: PathBuf) -> Option<u64> {
    read_text(path)?.parse().ok()
}

struct PerfCounter {
    file: File,
    previous: Option<u64>,
}

impl PerfCounter {
    fn delta(&mut self) -> Option<u64> {
        let mut bytes = [0u8; 8];
        self.file.read_exact(&mut bytes).ok()?;
        let value = u64::from_ne_bytes(bytes);
        let delta = self.previous.map(|old| value.saturating_sub(old));
        self.previous = Some(value);
        delta
    }
}

#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    event_type: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    sample_max_stack: u16,
    reserved2: u16,
    aux_sample_size: u32,
    reserved3: u32,
    sig_data: u64,
}

fn open_i915_busy_counters() -> Option<Vec<PerfCounter>> {
    let root = Path::new("/sys/bus/event_source/devices/i915");
    let event_type = read_u64(root.join("type"))? as u32;
    let mut counters = Vec::new();
    for entry in fs::read_dir(root.join("events")).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with("-busy") {
            continue;
        }
        let config = parse_event_config(&fs::read_to_string(entry.path()).ok()?)?;
        counters.push(open_perf_counter(event_type, config)?);
    }
    (!counters.is_empty()).then_some(counters)
}

fn parse_event_config(value: &str) -> Option<u64> {
    let value = value.trim().strip_prefix("config=")?;
    u64::from_str_radix(value.trim_start_matches("0x"), 16).ok()
}

fn open_perf_counter(event_type: u32, config: u64) -> Option<PerfCounter> {
    let attr = PerfEventAttr {
        event_type,
        size: std::mem::size_of::<PerfEventAttr>() as u32,
        config,
        ..PerfEventAttr::default()
    };
    // SAFETY: `attr` is a C-compatible perf_event_attr whose size is supplied
    // to the kernel. The returned descriptor is checked before ownership is
    // transferred to File, and all remaining syscall arguments are scalars.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            &attr,
            -1_i32,
            0_i32,
            -1_i32,
            1_u64 << 3, // PERF_FLAG_FD_CLOEXEC
        ) as RawFd
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: a successful perf_event_open returns a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    Some(PerfCounter {
        file,
        previous: None,
    })
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
                "smtop-intel-{}-{}",
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

    fn fixture_device(root: &Path, card: &str, vendor: &str, driver: &str) -> PathBuf {
        let device = root.join(card).join("device");
        fs::create_dir_all(&device).unwrap();
        fs::write(device.join("vendor"), vendor).unwrap();
        fs::write(
            device.join("uevent"),
            format!("DRIVER={driver}\nPCI_SLOT_NAME=0000:00:02.0\nPCI_ID=8086:FFFF\n"),
        )
        .unwrap();
        device
    }

    #[test]
    fn fixture_enumerates_i915_and_xe_but_skips_connectors_and_other_vendors() {
        let tmp = TestDir::new();
        for (card, vendor, driver) in [
            ("card0", "0x8086", "i915"),
            ("card0-DP-1", "0x8086", "i915"),
            ("card1", "0x1002", "amdgpu"),
            ("card2", "0x8086", "xe"),
        ] {
            fixture_device(&tmp.0, card, vendor, driver);
        }
        let devices = enumerate_intel_cards_at(&tmp.0);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].driver, "i915");
        assert_eq!(devices[1].driver, "xe");
    }

    #[test]
    fn fixture_samples_i915_frequency_hwmon_history_and_permission_note() {
        let tmp = TestDir::new();
        let device = fixture_device(&tmp.0, "card1", "0x8086", "i915");
        fs::create_dir_all(tmp.0.join("card1/gt/gt0")).unwrap();
        fs::write(tmp.0.join("card1/gt/gt0/rps_act_freq_mhz"), "700\n").unwrap();
        fs::create_dir_all(device.join("power")).unwrap();
        fs::write(device.join("power/runtime_status"), "active\n").unwrap();
        let hwmon = device.join("hwmon/hwmon0");
        fs::create_dir_all(&hwmon).unwrap();
        fs::write(hwmon.join("name"), "i915\n").unwrap();
        fs::write(hwmon.join("temp1_input"), "55000\n").unwrap();
        fs::write(hwmon.join("power1_average"), "12500000\n").unwrap();
        fs::write(hwmon.join("fan1_input"), "1200\n").unwrap();

        let dev = enumerate_intel_cards_at(&tmp.0).remove(0);
        let mut histories = HashMap::new();
        let snapshot = sample_device(0, &dev, None, &mut histories);
        assert_eq!(snapshot.vendor, GpuVendor::Intel);
        assert_eq!(snapshot.name, "Intel 8086:FFFF");
        assert_eq!(snapshot.sclk_mhz, Some(700));
        assert_eq!(snapshot.temp_c, Some(55.0));
        assert_eq!(snapshot.power_w, Some(12.5));
        assert!(matches!(snapshot.fan, Some(Fan::Rpm(1200))));
        assert_eq!(snapshot.note.as_deref(), Some("util requires CAP_PERFMON"));
        assert_eq!(snapshot.util_hist.points().len(), 1);

        let available = sample_device(0, &dev, Some(37.5), &mut histories);
        assert_eq!(available.busy_pct, 37.5);
        assert!(available.note.is_none());
        assert_eq!(available.util_hist.points().len(), 2);
    }

    #[test]
    fn fixture_reads_xe_gt_frequency_and_marks_suspended_without_note() {
        let tmp = TestDir::new();
        let device = fixture_device(&tmp.0, "card3", "0x8086", "xe");
        fs::create_dir_all(device.join("tile0/gt0/freq0")).unwrap();
        fs::create_dir_all(device.join("tile0/gt1/freq0")).unwrap();
        fs::write(device.join("tile0/gt0/freq0/act_freq"), "500\n").unwrap();
        fs::write(device.join("tile0/gt1/freq0/cur_freq"), "900\n").unwrap();
        fs::create_dir_all(device.join("power")).unwrap();
        fs::write(device.join("power/runtime_status"), "suspended\n").unwrap();

        let dev = enumerate_intel_cards_at(&tmp.0).remove(0);
        assert_eq!(read_frequency(&dev), Some(900));
        let snapshot = sample_device(0, &dev, None, &mut HashMap::new());
        assert!(snapshot.suspended);
        assert!(snapshot.note.is_none());
    }

    #[test]
    fn parses_pci_names_and_perf_configs() {
        let ids = "8086  Intel Corporation\n\ta780  Raptor Lake-S GT1 [UHD Graphics 770]\n\t\t1043  subsystem\n8087  Other\n";
        assert_eq!(
            parse_pci_name(ids, 0x8086, 0xa780).as_deref(),
            Some("Raptor Lake-S GT1 [UHD Graphics 770]")
        );
        assert_eq!(parse_event_config("config=0x2010\n"), Some(0x2010));
        assert_eq!(parse_event_config("bad"), None);
        assert_eq!(parse_pci_name(ids, 0x8086, 0xffff), None);
    }

    #[test]
    fn pmu_first_read_is_zero_baseline_then_reports_delta() {
        let tmp = TestDir::new();
        let counter_path = tmp.0.join("counter");
        let mut values = Vec::new();
        values.extend_from_slice(&1_000_000_000_u64.to_ne_bytes());
        values.extend_from_slice(&1_500_000_000_u64.to_ne_bytes());
        fs::write(&counter_path, values).unwrap();

        let mut collector = IntelCollector::new();
        collector.pmu = PmuState::Available(vec![PerfCounter {
            file: File::open(counter_path).unwrap(),
            previous: None,
        }]);
        assert_eq!(collector.utilization(1.0), Some(0.0));
        assert_eq!(collector.utilization(1.0), Some(50.0));
    }
}
