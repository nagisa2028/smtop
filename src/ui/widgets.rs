//! Small pure helpers shared by the UI render functions.

use ratatui::style::Color;

/// Color a utilization/fullness percentage: green → yellow → red.
pub fn usage_color(pct: f32) -> Color {
    if pct >= 90.0 {
        Color::Red
    } else if pct >= 70.0 {
        Color::Yellow
    } else if pct >= 40.0 {
        Color::Green
    } else {
        Color::Cyan
    }
}

/// Unicode horizontal bar of `width` cells representing `pct` (0..=100),
/// using eighth-block characters for sub-cell precision.
pub fn hbar(pct: f32, width: usize) -> String {
    let pct = pct.clamp(0.0, 100.0) as f64 / 100.0;
    let eighths = (pct * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;
    let mut s = String::with_capacity(width * 3);
    for _ in 0..full.min(width) {
        s.push('█');
    }
    let mut len = full.min(width);
    if len < width && rem > 0 {
        s.push(['▏', '▎', '▍', '▌', '▋', '▊', '▉'][rem - 1]);
        len += 1;
    }
    for _ in len..width {
        s.push(' ');
    }
    s
}

/// Format a byte count as a human-readable size.
pub fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", b, U[i])
    } else if v >= 100.0 {
        format!("{v:.0}{}", U[i])
    } else {
        format!("{v:.1}{}", U[i])
    }
}

/// Format an uptime in seconds as a compact `Xd Yh` / `Yh Zm` / `Zm` string.
pub fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Format a network link speed (Mbps) as `100M` / `1G` / `2.5G` / `10G`.
pub fn fmt_link(mbps: u64) -> String {
    if mbps >= 1000 {
        let g = mbps as f64 / 1000.0;
        if (g.fract()).abs() < 0.05 {
            format!("{g:.0}G")
        } else {
            format!("{g:.1}G")
        }
    } else {
        format!("{mbps}M")
    }
}

/// Format a byte/second rate.
pub fn fmt_rate(bps: f64) -> String {
    const U: [&str; 5] = ["B/s", "K/s", "M/s", "G/s", "T/s"];
    let mut v = bps.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 || i == 0 {
        format!("{v:.0}{}", U[i])
    } else {
        format!("{v:.1}{}", U[i])
    }
}
