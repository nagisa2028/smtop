//! Network throughput collector from `/proc/net/dev`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, Instant};

use super::Collector;
use crate::model::{History, NetSnapshot};

#[derive(Default)]
struct Prev {
    rx: u64,
    tx: u64,
    initialized: bool,
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

    fn retain_seen(&mut self, seen: &HashSet<String>) {
        self.prev.retain(|iface, _| seen.contains(iface));
    }
}

impl Collector for NetCollector {
    type Out = Vec<NetSnapshot>;

    fn name(&self) -> &'static str {
        "net"
    }

    fn interval(&self) -> Duration {
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<Vec<NetSnapshot>> {
        let content = fs::read_to_string("/proc/net/dev")?;
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64());
        self.last = Some(now);

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for line in content.lines().skip(2) {
            let Some((iface, rx, tx)) = parse_netdev_line(line) else {
                continue;
            };
            if self.skip_loopback && iface == "lo" {
                continue;
            }
            seen.insert(iface.clone());

            let prev = self.prev.entry(iface.clone()).or_default();
            let initialized = prev.initialized;
            let (rx_bps, tx_bps) = net_rates(prev, rx, tx, dt);
            prev.rx = rx;
            prev.tx = tx;
            prev.initialized = true;
            if initialized && dt.is_some_and(|dt| dt > 0.0) {
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
        self.retain_seen(&seen);
        // Busiest interfaces first.
        out.sort_by(|a, b| {
            (b.rx_bps + b.tx_bps)
                .partial_cmp(&(a.rx_bps + a.tx_bps))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }
}

fn net_rates(prev: &Prev, rx: u64, tx: u64, dt: Option<f64>) -> (f64, f64) {
    match dt {
        Some(dt) if prev.initialized && dt > 0.0 => (
            rx.saturating_sub(prev.rx) as f64 / dt,
            tx.saturating_sub(prev.tx) as f64 / dt,
        ),
        _ => (0.0, 0.0),
    }
}

/// Pure parse of a `/proc/net/dev` data line into `(iface, rx_bytes, tx_bytes)`.
/// Per-iface columns after the colon: 0 rx_bytes … 8 tx_bytes.
fn parse_netdev_line(line: &str) -> Option<(String, u64, u64)> {
    let (iface, rest) = line.split_once(':')?;
    let mut cols = rest.split_whitespace();
    let rx = cols.next()?.parse().unwrap_or(0);
    let tx = cols.nth(7)?.parse().unwrap_or(0);
    Some((iface.trim().to_string(), rx, tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netdev_line_rx_tx_offsets() {
        // iface: rx_bytes rx_pkts errs drop fifo frame compressed multicast tx_bytes ...
        let line = "  eth0: 12345 100 0 0 0 0 0 0 67890 200 0 0 0 0 0 0";
        let (iface, rx, tx) = parse_netdev_line(line).unwrap();
        assert_eq!(iface, "eth0");
        assert_eq!(rx, 12345);
        assert_eq!(tx, 67890);
    }

    #[test]
    fn netdev_rejects_header_and_short() {
        assert!(parse_netdev_line("Inter-|   Receive").is_none());
        assert!(parse_netdev_line("lo: 1 2 3").is_none());
    }

    #[test]
    fn net_rates_ignore_first_observation_and_counter_reset() {
        let mut prev = Prev::default();
        assert_eq!(net_rates(&prev, 10_000, 20_000, Some(1.0)), (0.0, 0.0));

        prev.rx = 10_000;
        prev.tx = 20_000;
        prev.initialized = true;
        assert_eq!(
            net_rates(&prev, 12_000, 26_000, Some(2.0)),
            (1_000.0, 3_000.0)
        );
        assert_eq!(net_rates(&prev, 100, 200, Some(1.0)), (0.0, 0.0));
        assert_eq!(net_rates(&prev, 12_000, 26_000, None), (0.0, 0.0));
    }

    #[test]
    fn disappeared_interfaces_drop_their_baselines() {
        let mut collector = NetCollector::new();
        collector.prev.insert("eth0".into(), Prev::default());
        collector.prev.insert("gone0".into(), Prev::default());
        collector.retain_seen(&HashSet::from(["eth0".to_string()]));
        assert!(collector.prev.contains_key("eth0"));
        assert!(!collector.prev.contains_key("gone0"));
    }
}
