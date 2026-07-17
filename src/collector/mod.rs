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
    log_line("--- smtop log opened ---");
    Ok(())
}

pub(crate) fn log_line(msg: &str) {
    if let Some(lock) = LOGGER.get()
        && let Ok(mut f) = lock.lock()
    {
        let _ = writeln!(f, "{msg}");
    }
}

pub mod amd;
pub mod cpu;
pub mod disk;
pub mod fs;
pub mod gpuproc;
pub mod intel;
pub mod net;
#[cfg(feature = "nvidia")]
pub mod nvidia;
pub mod proc;

/// A periodic sampler. Implementations hold their own previous-sample state and
/// history buffers across `sample` calls (via `&mut self`).
pub trait Collector {
    type Out;

    fn name(&self) -> &'static str;
    fn interval(&self) -> Duration;
    fn sample(&mut self) -> anyhow::Result<Self::Out>;
}

fn advance_deadline(
    next: Instant,
    interval: Duration,
    now: Instant,
) -> (Instant, Option<Duration>) {
    let target = next + interval;
    if target > now {
        (target, Some(target - now))
    } else {
        // Overran the interval; resync to avoid a catch-up spiral.
        (now, None)
    }
}

fn record_changed_error(last: &mut Option<String>, msg: String) -> bool {
    if last.as_deref() == Some(msg.as_str()) {
        false
    } else {
        *last = Some(msg);
        true
    }
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
                        if record_changed_error(&mut last_err, msg.clone()) {
                            log_line(&format!("[{name}] error: {msg}"));
                        }
                    }
                }
                let now = Instant::now();
                let (new_next, wait) = advance_deadline(next, interval, now);
                next = new_next;
                if let Some(wait) = wait {
                    thread::park_timeout(wait);
                }
            }
        })
        .expect("failed to spawn collector thread")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    use super::*;

    struct FakeCollector {
        samples: VecDeque<anyhow::Result<u32>>,
        calls: Arc<AtomicUsize>,
    }

    impl Collector for FakeCollector {
        type Out = u32;

        fn name(&self) -> &'static str {
            "test-collector"
        }

        fn interval(&self) -> Duration {
            Duration::from_millis(5)
        }

        fn sample(&mut self) -> anyhow::Result<Self::Out> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.samples.pop_front().unwrap_or(Ok(99))
        }
    }

    #[test]
    fn interval_is_clamped_to_safe_bounds() {
        let old = INTERVAL_MS.load(Ordering::Relaxed);
        set_interval_ms(1);
        assert_eq!(sample_interval(), Duration::from_millis(100));
        set_interval_ms(u64::MAX);
        assert_eq!(sample_interval(), Duration::from_millis(60_000));
        INTERVAL_MS.store(old, Ordering::Relaxed);
    }

    #[test]
    fn collector_recovers_after_error_publishes_and_stops() {
        let calls = Arc::new(AtomicUsize::new(0));
        let collector = FakeCollector {
            samples: VecDeque::from([Err(anyhow::anyhow!("temporary failure")), Ok(7)]),
            calls: calls.clone(),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let handle = spawn(collector, shutdown.clone(), move |value| {
            tx.send(value).unwrap();
        });

        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(7));
        assert!(calls.load(Ordering::Relaxed) >= 2);
        shutdown.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        // A collector started after shutdown must never sample or publish.
        let stopped_calls = Arc::new(AtomicUsize::new(0));
        let collector = FakeCollector {
            samples: VecDeque::new(),
            calls: stopped_calls.clone(),
        };
        let stopped = Arc::new(AtomicBool::new(true));
        let handle = spawn(collector, stopped, |_| panic!("unexpected publish"));
        handle.join().unwrap();
        assert_eq!(stopped_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn deadline_resyncs_after_overrun_and_errors_are_deduplicated() {
        let base = Instant::now();
        let interval = Duration::from_millis(100);
        let (next, wait) = advance_deadline(base, interval, base + Duration::from_millis(25));
        assert_eq!(next, base + interval);
        assert_eq!(wait, Some(Duration::from_millis(75)));

        let overrun = base + Duration::from_millis(150);
        assert_eq!(advance_deadline(base, interval, overrun), (overrun, None));

        let mut last = None;
        assert!(record_changed_error(&mut last, "failed".into()));
        assert!(!record_changed_error(&mut last, "failed".into()));
        assert!(record_changed_error(&mut last, "different".into()));
        assert_eq!(last.as_deref(), Some("different"));
        assert!(last.take().is_some()); // success logs one recovery and clears state
        assert!(last.is_none());
    }
}
