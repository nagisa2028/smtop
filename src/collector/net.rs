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
        super::sample_interval()
    }

    fn sample(&mut self) -> anyhow::Result<Vec<NetSnapshot>> {
        let content = fs::read_to_string("/proc/net/dev")?;
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64());
        self.last = Some(now);

        let mut out = Vec::new();
        for line in content.lines().skip(2) {
            let Some((iface, rx, tx)) = parse_netdev_line(line) else {
                continue;
            };
            if self.skip_loopback && iface == "lo" {
                continue;
            }

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

/// Pure parse of a `/proc/net/dev` data line into `(iface, rx_bytes, tx_bytes)`.
/// Per-iface columns after the colon: 0 rx_bytes … 8 tx_bytes.
fn parse_netdev_line(line: &str) -> Option<(String, u64, u64)> {
    let (iface, rest) = line.split_once(':')?;
    let cols: Vec<u64> = rest
        .split_whitespace()
        .map(|v| v.parse().unwrap_or(0))
        .collect();
    if cols.len() < 9 {
        return None;
    }
    Some((iface.trim().to_string(), cols[0], cols[8]))
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
}
