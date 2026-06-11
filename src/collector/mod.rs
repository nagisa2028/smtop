//! Collector framework: each source runs on its own thread with a
//! drift-correcting ticker, publishing snapshots through a closure.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Base sampling interval (ms), configurable via `--interval`.
static INTERVAL_MS: AtomicU64 = AtomicU64::new(1000);

pub fn set_interval_ms(ms: u64) {
    INTERVAL_MS.store(ms.clamp(100, 60_000), Ordering::Relaxed);
}

/// The configured base sampling interval, used by rate collectors.
pub fn sample_interval() -> Duration {
    Duration::from_millis(INTERVAL_MS.load(Ordering::Relaxed))
}

/// Optional diagnostic log (enabled with `--log <file>`), shared by all
/// collector threads so failures on unfamiliar hardware are diagnosable.
static LOGGER: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init_logger(path: &str) -> std::io::Result<()> {
    let file = File::options().create(true).append(true).open(path)?;
    let _ = LOGGER.set(Mutex::new(file));
    log_line("--- mon log opened ---");
    Ok(())
}

fn log_line(msg: &str) {
    if let Some(lock) = LOGGER.get()
        && let Ok(mut f) = lock.lock() {
            let _ = writeln!(f, "{msg}");
        }
}

pub mod amd;
pub mod cpu;
pub mod disk;
pub mod fs;
pub mod net;
pub mod proc;
#[cfg(feature = "nvidia")]
pub mod nvidia;

/// A periodic sampler. Implementations hold their own previous-sample state and
/// history buffers across `sample` calls (via `&mut self`).
pub trait Collector {
    type Out;

    fn name(&self) -> &'static str;
    fn interval(&self) -> Duration;
    fn sample(&mut self) -> anyhow::Result<Self::Out>;
}

/// Spawn a collector on its own thread.
///
/// The loop is drift-corrected: each tick targets `next += interval`, but if a
/// sample overruns the interval (e.g. under heavy CPU load or a stalled driver
/// call) the deadline resyncs to "now" instead of spiraling to catch up.
pub fn spawn<C, F>(mut collector: C, shutdown: Arc<AtomicBool>, publish: F) -> JoinHandle<()>
where
    C: Collector + Send + 'static,
    C::Out: Send + 'static,
    F: Fn(C::Out) + Send + 'static,
{
    let interval = collector.interval();
    let name = collector.name();
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut next = Instant::now();
            let mut last_err: Option<String> = None;
            while !shutdown.load(Ordering::Relaxed) {
                match collector.sample() {
                    Ok(out) => {
                        if last_err.take().is_some() {
                            log_line(&format!("[{name}] recovered"));
                        }
                        publish(out);
                    }
                    Err(e) => {
                        // Log only on change to avoid flooding every tick.
                        let msg = e.to_string();
                        if last_err.as_deref() != Some(msg.as_str()) {
                            log_line(&format!("[{name}] error: {msg}"));
                            last_err = Some(msg);
                        }
                    }
                }
                next += interval;
                let now = Instant::now();
                if next > now {
                    thread::park_timeout(next - now);
                } else {
                    // Overran the interval; resync to avoid a catch-up spiral.
                    next = now;
                }
            }
        })
        .expect("failed to spawn collector thread")
}
