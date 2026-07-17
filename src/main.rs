mod collector;
mod model;
mod ui;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use collector::spawn;
use model::{SharedState, Stamped};
use ui::fmt_bytes;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let parsed = parse_args(&args);
    for error in &parsed.errors {
        eprintln!("smtop: {error}");
    }
    if let Some(path) = &parsed.log
        && let Err(e) = collector::init_logger(path)
    {
        eprintln!("smtop: cannot open log {path}: {e}");
    }
    if let Some(ms) = parsed.interval_ms {
        collector::set_interval_ms(ms);
    }

    let state = Arc::new(SharedState::default());
    let shutdown = Arc::new(AtomicBool::new(false));

    spawn_collectors(&state, &shutdown);

    if parsed.probe {
        std::thread::sleep(
            collector::sample_interval() * 2 + std::time::Duration::from_millis(500),
        );
        print_probe(&state);
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        return Ok(());
    }

    ui::run(state, shutdown)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedArgs {
    log: Option<String>,
    interval_ms: Option<u64>,
    probe: bool,
    errors: Vec<String>,
}

fn parse_args(args: &[String]) -> ParsedArgs {
    let mut parsed = ParsedArgs::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--log" => match args.get(i + 1).filter(|p| !p.starts_with("--")) {
                Some(path) => {
                    parsed.log = Some(path.clone());
                    i += 1;
                }
                None => parsed.errors.push("--log expects a file path".into()),
            },
            "--interval" => match args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                Some(ms) => {
                    parsed.interval_ms = Some(ms);
                    i += 1;
                }
                None => parsed.errors.push("--interval expects milliseconds".into()),
            },
            "--probe" => parsed.probe = true,
            _ => {}
        }
        i += 1;
    }
    parsed
}

fn spawn_collectors(state: &Arc<SharedState>, shutdown: &Arc<AtomicBool>) {
    {
        let s = state.clone();
        spawn(
            collector::cpu::CpuCollector::new(),
            shutdown.clone(),
            move |o| s.cpu.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
    {
        let s = state.clone();
        spawn(
            collector::amd::AmdCollector::new(),
            shutdown.clone(),
            move |o| s.amd.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
    {
        let s = state.clone();
        spawn(
            collector::intel::IntelCollector::new(),
            shutdown.clone(),
            move |o| s.intel.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
    #[cfg(feature = "nvidia")]
    {
        let s = state.clone();
        spawn(
            collector::nvidia::NvidiaCollector::new(),
            shutdown.clone(),
            move |o| s.nvidia.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
    {
        let s = state.clone();
        spawn(
            collector::net::NetCollector::new(),
            shutdown.clone(),
            move |o| s.net.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
    {
        let s = state.clone();
        spawn(
            collector::disk::DiskCollector::new(),
            shutdown.clone(),
            move |o| s.disk.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
    {
        let s = state.clone();
        spawn(
            collector::fs::FsCollector::new(),
            shutdown.clone(),
            move |o| s.fs.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
    {
        let s = state.clone();
        spawn(
            collector::proc::ProcessCollector::new(),
            shutdown.clone(),
            move |o| s.procs.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
    {
        let s = state.clone();
        spawn(
            collector::gpuproc::GpuProcCollector::new(),
            shutdown.clone(),
            move |o| s.gpu_procs.store(Some(Arc::new(Stamped::new(o)))),
        );
    }
}

/// Strip control characters from externally sourced names (process cmdlines,
/// mount paths, device names) so they cannot inject terminal escape sequences
/// into the raw probe output.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

fn print_probe(state: &SharedState) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = write_probe(state, &mut out);
}

fn write_probe<W: Write>(state: &SharedState, out: &mut W) -> std::io::Result<()> {
    macro_rules! probe_line {
        ($($arg:tt)*) => {
            writeln!(out, $($arg)*)?
        };
    }

    if let Some(c) = state.cpu.load_full() {
        probe_line!(
            "CPU  {}  usage={:.0}%  {}  {}  mem={}/{} MB  load {:.2}",
            clean(&c.model),
            c.usage,
            c.freq_mhz
                .map(|f| format!("{:.2}GHz", f / 1000.0))
                .unwrap_or_else(|| "-".into()),
            c.temp_c
                .map(|t| format!("{t:.0}C"))
                .unwrap_or_else(|| "-".into()),
            c.mem_used / 1048576,
            c.mem_total / 1048576,
            c.load[0]
        );
    }
    for (label, slot) in [
        ("NVIDIA", &state.nvidia),
        ("AMD", &state.amd),
        ("Intel", &state.intel),
    ] {
        if let Some(gpus) = slot.load_full() {
            for g in gpus.iter() {
                probe_line!(
                    "{label}[{}] {}{}  busy={:.0}%  vram={}/{} MB  temp={}  power={}  sclk={}  fan={}",
                    g.index,
                    clean(&g.name),
                    match (&g.note, g.suspended) {
                        (Some(n), _) => format!(" [{}]", clean(n)),
                        (None, true) => " [suspended]".into(),
                        (None, false) => String::new(),
                    },
                    g.busy_pct,
                    g.mem_used / 1048576,
                    g.mem_total / 1048576,
                    g.temp_c
                        .map(|t| format!("{t:.0}C"))
                        .unwrap_or_else(|| "-".into()),
                    g.power_w
                        .map(|p| format!("{p:.0}W"))
                        .unwrap_or_else(|| "-".into()),
                    g.sclk_mhz
                        .map(|s| format!("{s}MHz"))
                        .unwrap_or_else(|| "-".into()),
                    match g.fan {
                        Some(model::Fan::Rpm(r)) => format!("{r}rpm"),
                        Some(model::Fan::Pct(p)) => format!("{p:.0}%"),
                        None => "-".into(),
                    },
                );
            }
        }
    }
    if let Some(net) = state.net.load_full() {
        for n in net.iter().take(3) {
            probe_line!(
                "NET  {}  rx={:.0} tx={:.0} B/s  link={}",
                clean(&n.iface),
                n.rx_bps,
                n.tx_bps,
                match (n.up, n.speed_mbps) {
                    (true, Some(s)) => format!("{s}Mbps"),
                    (true, None) => "up".into(),
                    (false, _) => "down".into(),
                }
            );
        }
    }
    if let Some(disk) = state.disk.load_full() {
        for d in disk.iter() {
            probe_line!(
                "DISK {}  r={:.0} w={:.0} B/s  util={:.0}%  iops r{:.0}/w{:.0}",
                clean(&d.dev),
                d.r_bps,
                d.w_bps,
                d.util_pct,
                d.r_iops,
                d.w_iops
            );
        }
    }
    if let Some(fs) = state.fs.load_full() {
        for f in fs.iter().take(6) {
            probe_line!(
                "FS   {}  {}/{}",
                clean(&f.mount),
                fmt_bytes(f.used),
                fmt_bytes(f.total)
            );
        }
    }
    if let Some(gp) = state.gpu_procs.load_full() {
        let mut v: Vec<_> = gp.iter().collect();
        v.sort_by_key(|(_, g)| std::cmp::Reverse(g.vram));
        probe_line!("GPU-PROCS {} using GPU; top by VRAM:", gp.len());
        for (pid, g) in v.iter().take(5) {
            probe_line!(
                "  {:>7} [{}] {:.0}% util  {} MB VRAM",
                pid,
                clean(&g.label),
                g.util_pct,
                g.vram / 1048576
            );
        }
    }
    if let Some(procs) = state.procs.load_full() {
        let mut top = procs.to_vec();
        top.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
        probe_line!("PROCS {} total; top by CPU:", procs.len());
        for p in top.iter().take(5) {
            let disk = if p.io_ok {
                format!(
                    "r{:.0}/w{:.0} KB/s",
                    p.disk_read_bps / 1024.0,
                    p.disk_write_bps / 1024.0
                )
            } else {
                "n/a (perm)".into()
            };
            probe_line!(
                "  {:>7} {:>5.1}% {:>8} MB  disk {disk}  {} {}",
                p.pid,
                p.cpu_pct,
                p.rss / 1048576,
                p.state,
                clean(&p.name).chars().take(36).collect::<String>()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{FsSnapshot, ProcInfo};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn cli_parser_handles_valid_and_missing_values() {
        assert_eq!(
            parse_args(&args(&[
                "smtop",
                "--log",
                "out.log",
                "--interval",
                "250",
                "--probe"
            ])),
            ParsedArgs {
                log: Some("out.log".into()),
                interval_ms: Some(250),
                probe: true,
                errors: vec![],
            }
        );

        let invalid = parse_args(&args(&["smtop", "--log", "--interval", "bad", "--probe"]));
        assert!(invalid.log.is_none());
        assert!(invalid.interval_ms.is_none());
        assert!(invalid.probe);
        assert_eq!(invalid.errors.len(), 2);
    }

    #[test]
    fn probe_output_is_sanitized_sorted_and_reports_permissions() {
        let state = SharedState::default();
        state
            .intel
            .store(Some(Arc::new(Stamped::new(vec![model::GpuSnapshot {
                vendor: model::GpuVendor::Intel,
                index: 0,
                name: "UHD Graphics 770".into(),
                busy_pct: 12.0,
                util_hist: model::History::new(),
                mem_used: 0,
                mem_total: 0,
                gtt: None,
                vram_hist: model::History::new(),
                temp_c: None,
                power_w: None,
                sclk_mhz: Some(700),
                mclk_mhz: None,
                fan: None,
                pcie_rx_bps: None,
                pcie_tx_bps: None,
                pcie_width: None,
                enc_pct: None,
                dec_pct: None,
                suspended: false,
                note: None,
            }]))));
        state.fs.store(Some(Arc::new(Stamped::new(vec![FsSnapshot {
            mount: "/safe\u{1b}[31m\nmount".into(),
            used: 1,
            total: 2,
        }]))));
        state.procs.store(Some(Arc::new(Stamped::new(vec![
            ProcInfo {
                pid: 1,
                name: "low".into(),
                cpu_pct: 1.0,
                rss: 0,
                state: 'S',
                disk_read_bps: 0.0,
                disk_write_bps: 0.0,
                io_ok: true,
            },
            ProcInfo {
                pid: 2,
                name: "high\u{7}".into(),
                cpu_pct: 90.0,
                rss: 0,
                state: 'R',
                disk_read_bps: 0.0,
                disk_write_bps: 0.0,
                io_ok: false,
            },
        ]))));

        let mut bytes = Vec::new();
        write_probe(&state, &mut bytes).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains('\u{7}'));
        assert!(output.contains("n/a (perm)"));
        assert!(output.contains("Intel[0] UHD Graphics 770"));
        assert!(output.find("high").unwrap() < output.find("low").unwrap());
    }
}
