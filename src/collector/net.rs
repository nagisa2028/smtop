//! Network throughput collector from `/proc/net/dev`.

use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};

use super::Collector;
use crate::model::{History, NetSnapshot};

#[derive(Default)]
struct Prev {
    rx: u64,
    tx: u64,
    rx_hist: History,
    tx_hist: History,
}

pub struct NetCollector {
    prev: HashMap<String, Prev>,
    last: Option<Instant>,
    skip_loopback: bool,
}

impl NetCollector {
    pub fn new() -> Self {
        Self {
            prev: HashMap::new(),
            last: None,
            skip_loopback: false,
        }
    }
}

impl Collector for NetCollector {
    type Out = Vec<NetSnapshot>;

    fn name(&self) -> &'static str {
        "net"
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn sample(&mut self) -> anyhow::Result<Vec<NetSnapshot>> {
        let content = fs::read_to_string("/proc/net/dev")?;
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64());
        self.last = Some(now);

        let mut out = Vec::new();
        for line in content.lines().skip(2) {
            let Some((iface, rest)) = line.split_once(':') else {
                continue;
            };
            let iface = iface.trim().to_string();
            if self.skip_loopback && iface == "lo" {
                continue;
            }
            let cols: Vec<u64> = rest
                .split_whitespace()
                .map(|v| v.parse().unwrap_or(0))
                .collect();
            if cols.len() < 9 {
                continue;
            }
            let rx = cols[0]; // receive bytes
            let tx = cols[8]; // transmit bytes

            let prev = self.prev.entry(iface.clone()).or_default();
            let (rx_bps, tx_bps) = match dt {
                Some(dt) if dt > 0.0 => (
                    rx.saturating_sub(prev.rx) as f64 / dt,
                    tx.saturating_sub(prev.tx) as f64 / dt,
                ),
                _ => (0.0, 0.0),
            };
            prev.rx = rx;
            prev.tx = tx;
            if dt.is_some() {
                prev.rx_hist.push(rx_bps);
                prev.tx_hist.push(tx_bps);
            }

            // "unknown" is reported by always-up devices (e.g. loopback).
            let up = fs::read_to_string(format!("/sys/class/net/{iface}/operstate"))
                .map(|s| matches!(s.trim(), "up" | "unknown"))
                .unwrap_or(false);
            let speed_mbps = fs::read_to_string(format!("/sys/class/net/{iface}/speed"))
                .ok()
                .and_then(|s| s.trim().parse::<i64>().ok())
                .filter(|&s| s > 0)
                .map(|s| s as u64);

            out.push(NetSnapshot {
                iface,
                rx_bps,
                tx_bps,
                rx_hist: prev.rx_hist.clone(),
                tx_hist: prev.tx_hist.clone(),
                up,
                speed_mbps,
            });
        }
        // Busiest interfaces first.
        out.sort_by(|a, b| {
            (b.rx_bps + b.tx_bps)
                .partial_cmp(&(a.rx_bps + a.tx_bps))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }
}
