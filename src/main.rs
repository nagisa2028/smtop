mod collector;
mod model;
mod ui;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use collector::spawn;
use model::{SharedState, Stamped};
use ui::fmt_bytes;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // `--log <file>`: record collector errors for diagnosing odd hardware.
    if let Some(i) = args.iter().position(|a| a == "--log") {
        match args.get(i + 1).filter(|p| !p.starts_with("--")) {
            Some(path) => {
                if let Err(e) = collector::init_logger(path) {
                    eprintln!("mon: cannot open log {path}: {e}");
                }
            }
            None => eprintln!("mon: --log expects a file path"),
        }
    }
    // `--interval <ms>`: base sampling interval (default 1000).
    if let Some(i) = args.iter().position(|a| a == "--interval") {
        match args.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
            Some(ms) => collector::set_interval_ms(ms),
            None => eprintln!("mon: --interval expects milliseconds"),
        }
    }

    let state = Arc::new(SharedState::default());
    let shutdown = Arc::new(AtomicBool::new(false));

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

    // Headless probe: dump one sample and exit (useful over SSH, no TTY needed).
    // Wait two intervals so rate collectors have a delta to report.
    if std::env::args().any(|a| a == "--probe") {
        std::thread::sleep(
            collector::sample_interval() * 2 + std::time::Duration::from_millis(500),
        );
        print_probe(&state);
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        return Ok(());
    }

    ui::run(state, shutdown)
}

/// Strip control characters from externally sourced names (process cmdlines,
/// mount paths, device names) so they cannot inject terminal escape sequences
/// into the raw probe output.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

fn print_probe(state: &SharedState) {
    if let Some(c) = state.cpu.load_full() {
        println!(
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
    for (label, slot) in [("NVIDIA", &state.nvidia), ("AMD", &state.amd)] {
        if let Some(gpus) = slot.load_full() {
            for g in gpus.iter() {
                println!(
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
            println!(
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
            println!(
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
            println!(
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
        println!("GPU-PROCS {} using GPU; top by VRAM:", gp.len());
        for (pid, g) in v.iter().take(5) {
            println!(
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
        println!("PROCS {} total; top by CPU:", procs.len());
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
            println!(
                "  {:>7} {:>5.1}% {:>8} MB  disk {disk}  {} {}",
                p.pid,
                p.cpu_pct,
                p.rss / 1048576,
                p.state,
                clean(&p.name).chars().take(36).collect::<String>()
            );
        }
    }
}
