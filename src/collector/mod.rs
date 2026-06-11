//! Collector framework: each source runs on its own thread with a
//! drift-correcting ticker, publishing snapshots through a closure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub mod amd;
pub mod cpu;
pub mod disk;
pub mod fs;
pub mod net;
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
    thread::Builder::new()
        .name(collector.name().to_string())
        .spawn(move || {
            let mut next = Instant::now();
            while !shutdown.load(Ordering::Relaxed) {
                match collector.sample() {
                    Ok(out) => publish(out),
                    Err(_e) => { /* transient read error; keep last published */ }
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
