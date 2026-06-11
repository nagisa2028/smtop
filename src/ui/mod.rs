//! Terminal UI: 3-tier responsive dashboard rendered with ratatui.

mod widgets;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart, Dataset, Gauge, GraphType, Paragraph};

use crate::model::{CoreGroup, CpuSnapshot, DiskSnapshot, Fan, FsSnapshot, GpuSnapshot, GpuVendor, NetSnapshot, ProcInfo, SharedState};
use widgets::{fmt_bytes, fmt_link, fmt_rate, fmt_uptime, hbar, usage_color};

const FRAME_MS: u64 = 250;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Processes,
}

#[derive(Clone, Copy, PartialEq)]
enum ProcSort {
    Cpu,
    Mem,
    Pid,
}

impl ProcSort {
    fn next(self) -> Self {
        match self {
            ProcSort::Cpu => ProcSort::Mem,
            ProcSort::Mem => ProcSort::Pid,
            ProcSort::Pid => ProcSort::Cpu,
        }
    }
    fn label(self) -> &'static str {
        match self {
            ProcSort::Cpu => "CPU",
            ProcSort::Mem => "MEM",
            ProcSort::Pid => "PID",
        }
    }
}

/// Mutable view state owned by the event loop.
struct View {
    tab: Tab,
    paused: bool,
    proc_scroll: usize,
    proc_sort: ProcSort,
}

impl Default for View {
    fn default() -> Self {
        Self {
            tab: Tab::Overview,
            paused: false,
            proc_scroll: 0,
            proc_sort: ProcSort::Cpu,
        }
    }
}

pub fn run(state: Arc<SharedState>, shutdown: Arc<AtomicBool>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let res = run_loop(&mut terminal, &state);
    ratatui::restore();
    shutdown.store(true, Ordering::Relaxed);
    res
}

fn run_loop(terminal: &mut DefaultTerminal, state: &SharedState) -> io::Result<()> {
    let mut view = View::default();
    let mut redraw = true;
    loop {
        // While paused the screen is frozen; only redraw on toggle/resize/input.
        if !view.paused || redraw {
            terminal.draw(|frame| render(frame, state, &view))?;
            redraw = false;
        }
        if event::poll(Duration::from_millis(FRAME_MS))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    redraw = true;
                    let in_procs = view.tab == Tab::Processes;
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(());
                        }
                        KeyCode::Char(' ') => view.paused = !view.paused,
                        KeyCode::Tab | KeyCode::BackTab => {
                            view.tab = match view.tab {
                                Tab::Overview => Tab::Processes,
                                Tab::Processes => Tab::Overview,
                            };
                            view.proc_scroll = 0;
                        }
                        KeyCode::Char('1') => view.tab = Tab::Overview,
                        KeyCode::Char('2') => view.tab = Tab::Processes,
                        KeyCode::Char('s') if in_procs => view.proc_sort = view.proc_sort.next(),
                        KeyCode::Char('c') if in_procs => view.proc_sort = ProcSort::Cpu,
                        KeyCode::Char('m') if in_procs => view.proc_sort = ProcSort::Mem,
                        KeyCode::Char('p') if in_procs => view.proc_sort = ProcSort::Pid,
                        KeyCode::Down | KeyCode::Char('j') if in_procs => {
                            view.proc_scroll += 1;
                        }
                        KeyCode::Up | KeyCode::Char('k') if in_procs => {
                            view.proc_scroll = view.proc_scroll.saturating_sub(1);
                        }
                        _ => redraw = false,
                    }
                }
                Event::Resize(_, _) => redraw = true,
                _ => {}
            }
        }
    }
}

fn render(frame: &mut Frame, state: &SharedState, view: &View) {
    let full = frame.area();
    if full.width < 50 || full.height < 18 {
        let p = Paragraph::new("terminal too small — resize (≥50×18)").style(Style::new().fg(Color::Yellow));
        frame.render_widget(p, full);
        return;
    }

    // Reserve the top row for a header; the selected tab fills the rest.
    let split = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(full);
    render_header(frame, split[0], state, view);
    match view.tab {
        Tab::Overview => render_overview(frame, split[1], state),
        Tab::Processes => render_processes(frame, split[1], state, view),
    }
}

fn render_overview(frame: &mut Frame, area: Rect, state: &SharedState) {
    // Responsive tier heights. The CPU tier grows to fit the per-core grid
    // (which expands with core/socket count), keeping ≥6 rows for GPUs.
    let cpu = state.cpu.load_full();
    let bot_h = if area.height >= 26 { 10 } else { 8 };
    let top_region = if area.height >= 30 { 7 } else { 5 }; // chart + gauges
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let core_rows = cpu
        .as_ref()
        .map(|c| cpu_core_rows(&c.core_groups, inner_w))
        .unwrap_or(2);
    let cpu_inner_max = (area.height as usize).saturating_sub(2 + 6 + bot_h as usize);
    let cpu_inner = (top_region + core_rows).clamp(5, cpu_inner_max.max(5));
    let cpu_h = (cpu_inner + 2) as u16;
    let tiers = Layout::vertical([
        Constraint::Length(cpu_h), // ① CPU / RAM
        Constraint::Min(6),        // ② GPU cards
        Constraint::Length(bot_h), // ③ Network | Disk | Free
    ])
    .split(area);

    if let Some(cpu) = &cpu {
        render_cpu(frame, tiers[0], cpu);
    }

    let mut gpus: Vec<GpuSnapshot> = Vec::new();
    if let Some(nv) = state.nvidia.load_full() {
        gpus.extend(nv.iter().cloned());
    }
    if let Some(amd) = state.amd.load_full() {
        gpus.extend(amd.iter().cloned());
    }
    render_gpus(frame, tiers[1], &gpus);

    let net = state.net.load_full();
    let disk = state.disk.load_full();
    let fs = state.fs.load_full();
    render_bottom(
        frame,
        tiers[2],
        net.as_deref().map(Vec::as_slice).unwrap_or(&[]),
        disk.as_deref().map(Vec::as_slice).unwrap_or(&[]),
        fs.as_deref().map(Vec::as_slice).unwrap_or(&[]),
    );
}

/// A single- or dual-series Braille line chart over the given owned point sets.
/// When `y_labels` is non-empty, a left Y-axis with those tick labels is drawn
/// (bottom-to-top) as a reading guide.
fn line_chart<'a>(
    series: &'a [(Color, &'a [(f64, f64)])],
    y_max: f64,
    y_labels: Vec<Line<'a>>,
) -> Chart<'a> {
    let datasets: Vec<Dataset> = series
        .iter()
        .map(|(color, pts)| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(*color))
                .data(pts)
        })
        .collect();
    let mut y_axis = Axis::default().bounds([0.0, y_max.max(1.0)]);
    if !y_labels.is_empty() {
        y_axis = y_axis.labels(y_labels).style(Style::new().fg(Color::DarkGray));
    }
    // Fixed window so the newest sample stays pinned to the right edge and the
    // graph scrolls right-to-left, rather than rescaling as history fills.
    Chart::new(datasets)
        .x_axis(Axis::default().bounds([0.0, (crate::model::HIST_CAP - 1) as f64]))
        .y_axis(y_axis)
}

/// `0 / 50 / 100` percent tick labels for a usage chart's Y-axis.
fn pct_labels() -> Vec<Line<'static>> {
    vec![Line::from("0"), Line::from("50"), Line::from("100")]
}

/// Per-thread bar width in the CPU topology grid.
const TH_W: usize = 3;

/// Digits reserved for the cpu-index label (aligns 1/2/3-digit indices).
fn cpu_label_width(groups: &[CoreGroup]) -> usize {
    let max_cpu = groups.iter().flat_map(|g| g.cpus.iter()).copied().max().unwrap_or(0);
    if max_cpu >= 100 {
        3
    } else if max_cpu >= 10 {
        2
    } else {
        1
    }
}

/// Rendered width of one physical core (labels + bars + thread separators).
fn core_cell_w(threads: usize, lw: usize) -> usize {
    threads * (lw + TH_W) + threads.saturating_sub(1)
}

/// True when more than one socket (package) is present.
fn multi_socket(groups: &[CoreGroup]) -> bool {
    groups.first().is_some_and(|f| groups.iter().any(|g| g.package != f.package))
}

/// Text rows the per-core grid needs: each socket starts a new row (prefixed
/// with an `S<n>` label when multi-socket) and its cores wrap by width.
fn cpu_core_rows(groups: &[CoreGroup], inner_w: usize) -> usize {
    if groups.is_empty() {
        return 1;
    }
    let lw = cpu_label_width(groups);
    let label_w = if multi_socket(groups) { 3 } else { 0 };
    let mut rows = 0usize;
    let mut cur = 0usize;
    let mut prev: Option<i64> = None;
    for g in groups {
        let cw = core_cell_w(g.cpus.len(), lw);
        if prev != Some(g.package) {
            rows += 1;
            cur = label_w + cw;
            prev = Some(g.package);
        } else if cur + 1 + cw > inner_w {
            rows += 1;
            cur = label_w + cw;
        } else {
            cur += 1 + cw;
        }
    }
    rows.max(1)
}

/// `0 / peak` rate tick labels for a throughput chart's Y-axis (unit implied
/// per second). The top label is the current windowed peak.
fn rate_labels(peak: f64) -> Vec<Line<'static>> {
    vec![Line::from("0"), Line::from(fmt_bytes(peak.max(0.0) as u64))]
}

/// System hostname (read once from sysctl).
fn hostname() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into())
    })
}

/// Top header: identity, clock, tab bar, per-collector liveness, key hints.
fn render_header(frame: &mut Frame, area: Rect, state: &SharedState, view: &View) {
    let now = chrono::Local::now().format("%H:%M:%S");
    let mut spans = vec![
        Span::styled("mon", Style::new().fg(Color::Cyan).bold()),
        Span::raw(" "),
        Span::styled(hostname(), Style::new().fg(Color::White)),
        Span::styled(format!("  {now}  "), Style::new().add_modifier(Modifier::DIM)),
    ];
    // Tab bar: selected tab reversed, others dim.
    for (i, (tab, title)) in [(Tab::Overview, "Overview"), (Tab::Processes, "Processes")]
        .into_iter()
        .enumerate()
    {
        let sel = view.tab == tab;
        let style = if sel {
            Style::new().fg(Color::Black).bg(Color::Cyan).bold()
        } else {
            Style::new().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(format!(" {}:{title} ", i + 1), style));
        spans.push(Span::raw(" "));
    }
    // Liveness: green once a collector has published, red if it never has.
    spans.push(Span::raw(" "));
    for (label, alive) in [
        ("cpu", state.cpu.load_full().is_some()),
        ("gpu", state.nvidia.load_full().is_some() || state.amd.load_full().is_some()),
        ("net", state.net.load_full().is_some()),
        ("disk", state.disk.load_full().is_some()),
        ("fs", state.fs.load_full().is_some()),
    ] {
        spans.push(Span::styled(
            format!("{label} "),
            Style::new().fg(if alive { Color::Green } else { Color::Red }),
        ));
    }
    if view.paused {
        spans.push(Span::styled(" PAUSED ", Style::new().fg(Color::Black).bg(Color::Yellow)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    let hints = if view.tab == Tab::Processes {
        "Tab:switch  s:sort  ↑↓:scroll  q:quit"
    } else {
        "Tab:switch  space:pause  q:quit"
    };
    frame.render_widget(
        Paragraph::new(hints)
            .alignment(Alignment::Right)
            .style(Style::new().add_modifier(Modifier::DIM)),
        area,
    );
}

fn proc_state_style(s: char) -> Style {
    match s {
        'R' => Style::new().fg(Color::Green),
        'D' => Style::new().fg(Color::Red),
        'Z' => Style::new().fg(Color::Yellow),
        _ => Style::new().add_modifier(Modifier::DIM),
    }
}

/// Processes tab: a sortable, scrollable PID/CPU/MEM table.
fn render_processes(frame: &mut Frame, area: Rect, state: &SharedState, view: &View) {
    let procs = state.procs.load_full();
    let count = procs.as_ref().map_or(0, |p| p.len());
    let block = Block::bordered().title(
        format!(
            " Processes ({count})   sort:{}   keys: s cycle · c/m/p · ↑↓ scroll ",
            view.proc_sort.label()
        )
        .bold(),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(procs) = procs else {
        frame.render_widget(Paragraph::new("collecting…".dim()), inner);
        return;
    };
    let mut list: Vec<ProcInfo> = procs.to_vec();
    match view.proc_sort {
        ProcSort::Cpu => list.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct)),
        ProcSort::Mem => list.sort_by_key(|p| std::cmp::Reverse(p.rss)),
        ProcSort::Pid => list.sort_by_key(|p| p.pid),
    }

    let cmd_w = (inner.width as usize).saturating_sub(26).max(4);
    // Header row with the active sort column marked (▼) and highlighted.
    let base = Style::new().add_modifier(Modifier::REVERSED);
    let active = Style::new().fg(Color::Yellow).add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let col = |text: String, this: ProcSort| {
        Span::styled(text, if view.proc_sort == this { active } else { base })
    };
    let mark = |this: ProcSort| if view.proc_sort == this { '▾' } else { ' ' };
    let header = Line::from(vec![
        col(format!("{:>6}{}", "PID", mark(ProcSort::Pid)), ProcSort::Pid),
        Span::styled(" ", base),
        col(format!("{:>4}{}", "CPU%", mark(ProcSort::Cpu)), ProcSort::Cpu),
        Span::styled(" ", base),
        col(format!("{:>8}{}", "MEM", mark(ProcSort::Mem)), ProcSort::Mem),
        Span::styled(" S ", base),
        Span::styled(format!("{:<cmd_w$}", "COMMAND"), base),
    ]);

    let visible = (inner.height as usize).saturating_sub(1);
    let scroll = view.proc_scroll.min(list.len().saturating_sub(visible));

    let mut lines = vec![header];
    for p in list.iter().skip(scroll).take(visible) {
        let name: String = p.name.chars().take(cmd_w).collect();
        lines.push(Line::from(vec![
            Span::raw(format!("{:>7} ", p.pid)),
            Span::styled(
                format!("{:>5.1} ", p.cpu_pct),
                Style::new().fg(usage_color(p.cpu_pct.min(100.0))),
            ),
            Span::raw(format!("{:>9} ", fmt_bytes(p.rss))),
            Span::styled(format!("{} ", p.state), proc_state_style(p.state)),
            Span::styled(name, Style::new().add_modifier(Modifier::DIM)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_cpu(frame: &mut Frame, area: Rect, cpu: &CpuSnapshot) {
    let title = format!(
        " CPU  {}   load {:.2} {:.2} {:.2}   up {}   tasks {}/{} ",
        cpu.model,
        cpu.load[0],
        cpu.load[1],
        cpu.load[2],
        fmt_uptime(cpu.uptime_secs),
        cpu.tasks_running,
        cpu.tasks_total,
    );
    let block = Block::bordered().title(title.bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Per-core topology grid: threads within a core joined by │, cores by ║,
    // sockets split onto their own labelled rows. Height grows with core count.
    let lw = cpu_label_width(&cpu.core_groups);
    let multi = multi_socket(&cpu.core_groups);
    let core_lines = cpu_core_rows(&cpu.core_groups, inner.width as usize)
        .min((inner.height as usize).saturating_sub(4))
        .max(1) as u16;

    let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(core_lines)]).split(inner);
    let top = Layout::horizontal([Constraint::Min(10), Constraint::Length(32)]).split(rows[0]);

    // Usage time-series.
    let pts = cpu.usage_hist.points();
    let mem_pts = cpu.mem_hist.points();
    let series = [(Color::Cyan, pts.as_slice()), (Color::Magenta, mem_pts.as_slice())];
    let chart = line_chart(&series, 100.0, pct_labels())
        .block(Block::bordered().title(Line::from(vec![
            Span::styled("usage", Style::new().fg(Color::Cyan)),
            Span::raw(" / "),
            Span::styled("mem %", Style::new().fg(Color::Magenta)),
        ])));
    frame.render_widget(chart, top[0]);

    // RAM / Swap / memory detail / aggregate.
    let g = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(top[1]);
    let mem_pct = pct(cpu.mem_used, cpu.mem_total);
    frame.render_widget(
        Gauge::default()
            .ratio((mem_pct / 100.0) as f64)
            .gauge_style(Style::new().fg(usage_color(mem_pct)))
            .label(format!("RAM {}/{}", fmt_bytes(cpu.mem_used), fmt_bytes(cpu.mem_total))),
        g[0],
    );
    if cpu.swap_total > 0 {
        let sp = pct(cpu.swap_used, cpu.swap_total);
        frame.render_widget(
            Gauge::default()
                .ratio((sp / 100.0) as f64)
                .gauge_style(Style::new().fg(usage_color(sp)))
                .label(format!("Swap {}/{}", fmt_bytes(cpu.swap_used), fmt_bytes(cpu.swap_total))),
            g[1],
        );
    } else {
        frame.render_widget(Paragraph::new("Swap —".dim()), g[1]);
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!(
                "avail {}  cache {}",
                fmt_bytes(cpu.mem_available),
                fmt_bytes(cpu.mem_cached)
            ),
            Style::new().add_modifier(Modifier::DIM),
        )])),
        g[2],
    );
    let mut info = vec![
        Span::raw("CPU "),
        Span::styled(format!("{:>3.0}%", cpu.usage), Style::new().fg(usage_color(cpu.usage))),
    ];
    if let Some(f) = cpu.freq_mhz {
        info.push(Span::raw(format!("  {:.2}GHz", f / 1000.0)));
    }
    if let Some(t) = cpu.temp_c {
        info.push(Span::styled(
            format!("  {t:.0}°C"),
            Style::new().fg(usage_color((t - 30.0).clamp(0.0, 100.0))),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(info)), g[3]);

    // threads within a core: light │   cores: double ║   sockets: new row + label
    let core_sep = Style::new().fg(Color::Gray);
    let thread_sep = Style::new().fg(Color::DarkGray);
    let socket_lbl = Style::new().fg(Color::Yellow);
    let inner_w = inner.width as usize;
    let label_w = if multi { 3 } else { 0 };
    let mut lines: Vec<Line> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut cur_w = 0usize;
    let mut prev: Option<i64> = None;
    for group in &cpu.core_groups {
        let cw = core_cell_w(group.cpus.len(), lw);
        if prev != Some(group.package) {
            // New socket: flush the current row and start a fresh, labelled one.
            if !spans.is_empty() {
                lines.push(Line::from(std::mem::take(&mut spans)));
            }
            cur_w = 0;
            if multi {
                spans.push(Span::styled(format!("S{} ", group.package), socket_lbl));
                cur_w += label_w;
            }
            prev = Some(group.package);
        } else if cur_w + 1 + cw > inner_w {
            // Wrap within the same socket; indent under the socket label.
            lines.push(Line::from(std::mem::take(&mut spans)));
            cur_w = 0;
            if multi {
                spans.push(Span::raw("   "));
                cur_w += label_w;
            }
        } else {
            spans.push(Span::styled("║", core_sep));
            cur_w += 1;
        }
        for (ti, &lcpu) in group.cpus.iter().enumerate() {
            if ti > 0 {
                spans.push(Span::styled("│", thread_sep));
            }
            let u = cpu.per_core.get(lcpu).copied().unwrap_or(0.0);
            spans.push(Span::styled(format!("{lcpu:>lw$}"), Style::new().add_modifier(Modifier::DIM)));
            spans.push(Span::styled(hbar(u, TH_W), Style::new().fg(usage_color(u))));
        }
        cur_w += cw;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), rows[1]);
}

fn render_gpus(frame: &mut Frame, area: Rect, gpus: &[GpuSnapshot]) {
    if gpus.is_empty() {
        let block = Block::bordered().title(" GPU ".bold());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new("No GPUs detected (NVIDIA via NVML / AMD via amdgpu sysfs)".dim()),
            inner,
        );
        return;
    }

    let min_card_w = 34u16;
    let cols = ((area.width / min_card_w).max(1) as usize).min(gpus.len());
    let row_n = gpus.len().div_ceil(cols);

    let row_areas = Layout::vertical(vec![Constraint::Fill(1); row_n]).split(area);
    for (r, row_area) in row_areas.iter().enumerate() {
        let col_areas = Layout::horizontal(vec![Constraint::Fill(1); cols]).split(*row_area);
        for (c, cell) in col_areas.iter().enumerate() {
            if let Some(gpu) = gpus.get(r * cols + c) {
                render_gpu_card(frame, *cell, gpu);
            }
        }
    }
}

fn render_gpu_card(frame: &mut Frame, area: Rect, g: &GpuSnapshot) {
    let (color, sym) = match g.vendor {
        GpuVendor::Nvidia => (Color::Green, "⬢"),
        GpuVendor::Amd => (Color::Red, "⬡"),
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(color))
        .title(format!(" GPU{} {} {} ", g.index, sym, g.name).bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 3 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Min(2),    // util chart
        Constraint::Length(1), // vram gauge
        Constraint::Length(1), // stats: temp/power/fan
        Constraint::Length(1), // clocks / gtt
    ])
    .split(inner);

    let pts = g.util_hist.points();
    let vram_pts = g.vram_hist.points();
    let series = [(usage_color(g.busy_pct), pts.as_slice()), (Color::Blue, vram_pts.as_slice())];
    frame.render_widget(line_chart(&series, 100.0, pct_labels()), rows[0]);

    let vram_pct = pct(g.mem_used, g.mem_total);
    frame.render_widget(
        Gauge::default()
            .ratio((vram_pct / 100.0) as f64)
            .gauge_style(Style::new().fg(usage_color(vram_pct)))
            .label(format!(
                "VRAM {}/{} {:.0}%",
                fmt_bytes(g.mem_used),
                fmt_bytes(g.mem_total),
                vram_pct
            )),
        rows[1],
    );

    let mut stats: Vec<Span> = if let Some(note) = &g.note {
        // Telemetry is missing for an explained reason — show it instead of 0%.
        vec![Span::styled(format!("⚠ {note}"), Style::new().fg(Color::Yellow))]
    } else if g.suspended {
        vec![Span::styled("⏾ idle (suspended)", Style::new().fg(Color::DarkGray))]
    } else {
        vec![Span::styled(
            format!("{:>3.0}% util", g.busy_pct),
            Style::new().fg(usage_color(g.busy_pct)),
        )]
    };
    if let Some(t) = g.temp_c {
        stats.push(Span::raw(format!("  {t:.0}°C")));
    }
    if let Some(p) = g.power_w {
        stats.push(Span::raw(format!("  {p:.0}W")));
    }
    match g.fan {
        Some(Fan::Pct(p)) => stats.push(Span::raw(format!("  fan {p:.0}%"))),
        Some(Fan::Rpm(r)) => stats.push(Span::raw(format!("  fan {r}rpm"))),
        None => {}
    }
    frame.render_widget(Paragraph::new(Line::from(stats)), rows[2]);

    let mut line2: Vec<Span> = Vec::new();
    if let Some(s) = g.sclk_mhz {
        line2.push(Span::raw(format!("core {s}MHz")));
    }
    if let Some(m) = g.mclk_mhz {
        line2.push(Span::raw(format!("  mem {m}MHz")));
    }
    if let Some((u, t)) = g.gtt {
        line2.push(Span::styled(
            format!("  GTT {}/{}", fmt_bytes(u), fmt_bytes(t)),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    match (g.pcie_rx_bps, g.pcie_tx_bps) {
        (Some(rx), Some(tx)) => line2.push(Span::styled(
            format!("  PCIe ▼{} ▲{}", fmt_rate(rx), fmt_rate(tx)),
            Style::new().fg(Color::DarkGray),
        )),
        _ => {
            if let Some(w) = g.pcie_width {
                line2.push(Span::styled(
                    format!("  PCIe x{w}"),
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
        }
    }
    frame.render_widget(Paragraph::new(Line::from(line2)), rows[3]);
}

fn render_bottom(
    frame: &mut Frame,
    area: Rect,
    net: &[NetSnapshot],
    disk: &[DiskSnapshot],
    fs: &[FsSnapshot],
) {
    let chunks = if area.width >= 84 {
        Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1), Constraint::Fill(1)]).split(area)
    } else {
        Layout::vertical([Constraint::Fill(1), Constraint::Fill(1), Constraint::Fill(1)]).split(area)
    };
    render_net(frame, chunks[0], net);
    render_disk(frame, chunks[1], disk);
    render_free(frame, chunks[2], fs);
}

fn render_net(frame: &mut Frame, area: Rect, net: &[NetSnapshot]) {
    let block = Block::bordered().title(" Network ".bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if net.is_empty() {
        return;
    }
    let list_n = net.len().min(3) as u16;
    let parts = Layout::vertical([Constraint::Length(list_n), Constraint::Min(0)]).split(inner);

    let lines: Vec<Line> = net
        .iter()
        .take(3)
        .map(|n| {
            let link = match (n.up, n.speed_mbps) {
                (true, Some(s)) => Span::styled(format!(" {}", fmt_link(s)), Style::new().fg(Color::DarkGray)),
                (true, None) => Span::styled(" up", Style::new().fg(Color::DarkGray)),
                (false, _) => Span::styled(" down", Style::new().fg(Color::Red)),
            };
            Line::from(vec![
                Span::raw(format!("{:<8}", truncate(&n.iface, 8))),
                Span::styled(format!("▼{:>8}", fmt_rate(n.rx_bps)), Style::new().fg(Color::Green)),
                Span::styled(format!(" ▲{:>8}", fmt_rate(n.tx_bps)), Style::new().fg(Color::Blue)),
                link,
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), parts[0]);

    if let Some(top) = net.first() {
        let rx = top.rx_hist.points();
        let tx = top.tx_hist.points();
        let ymax = top.rx_hist.max().max(top.tx_hist.max());
        frame.render_widget(
            line_chart(&[(Color::Green, &rx), (Color::Blue, &tx)], ymax, rate_labels(ymax)),
            parts[1],
        );
    }
}

fn render_disk(frame: &mut Frame, area: Rect, disk: &[DiskSnapshot]) {
    let block = Block::bordered().title(" Disk I/O ".bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if disk.is_empty() {
        return;
    }
    // Show as many devices as fit, keeping ~3 rows for the chart.
    let max_list = inner.height.saturating_sub(3).max(1) as usize;
    let list_n = disk.len().min(max_list);
    let parts =
        Layout::vertical([Constraint::Length(list_n as u16), Constraint::Min(0)]).split(inner);

    let lines: Vec<Line> = disk
        .iter()
        .take(list_n)
        .map(|d| {
            Line::from(vec![
                Span::raw(format!("{:<7}", truncate(&d.dev, 7))),
                Span::styled(format!("{:>3.0}%", d.util_pct), Style::new().fg(usage_color(d.util_pct))),
                Span::styled(format!(" R{:>8}", fmt_rate(d.r_bps)), Style::new().fg(Color::Cyan)),
                Span::styled(format!(" W{:>8}", fmt_rate(d.w_bps)), Style::new().fg(Color::Magenta)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), parts[0]);

    // Busiest device by recent activity.
    if let Some(top) = disk.iter().max_by(|a, b| {
        (a.r_bps + a.w_bps)
            .partial_cmp(&(b.r_bps + b.w_bps))
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        let r = top.r_hist.points();
        let w = top.w_hist.points();
        let ymax = top.r_hist.max().max(top.w_hist.max());
        let iops_title = Line::from(Span::styled(
            format!(" {} {:.0}/{:.0} iops ", top.dev, top.r_iops, top.w_iops),
            Style::new().add_modifier(Modifier::DIM),
        ));
        frame.render_widget(
            line_chart(&[(Color::Cyan, &r), (Color::Magenta, &w)], ymax, rate_labels(ymax))
                .block(Block::default().title(iops_title)),
            parts[1],
        );
    }
}

fn render_free(frame: &mut Frame, area: Rect, fs: &[FsSnapshot]) {
    let block = Block::bordered().title(" Free Space ".bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if fs.is_empty() || inner.height == 0 {
        return;
    }
    let n = (fs.len() as u16).min(inner.height) as usize;
    let rows = Layout::vertical(vec![Constraint::Length(1); n]).split(inner);
    // Mount column width = widest shown name (capped). The text block has a
    // fixed width so the name/capacity/percent columns line up across rows.
    let mw = fs
        .iter()
        .take(n)
        .map(|f| f.mount.chars().count().min(16))
        .max()
        .unwrap_or(4);
    let text_w = (mw as u16 + 16).min(inner.width.saturating_sub(3));
    for (row, f) in rows.iter().zip(fs.iter()) {
        let p = pct(f.used, f.total);
        // Split the row: gauge bar on the left, a 100%-mark separator, then the
        // aligned text columns. Separate rects (no overlay) so the text never
        // erases the bar, and the separator shows the full-scale (100%) line.
        let cols = Layout::horizontal([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(text_w),
        ])
        .split(*row);
        frame.render_widget(
            Gauge::default()
                .ratio((p / 100.0) as f64)
                .gauge_style(Style::new().fg(usage_color(p)))
                .label(""),
            cols[0],
        );
        frame.render_widget(
            Paragraph::new("│").style(Style::new().fg(Color::DarkGray)),
            cols[1],
        );
        frame.render_widget(
            Paragraph::new(format!(
                "{:>mw$} {:>10} {:>4}",
                truncate(&f.mount, mw),
                format!("{}/{}", fmt_bytes(f.used), fmt_bytes(f.total)),
                format!("{:.0}%", p),
            ))
            .alignment(Alignment::Right),
            cols[2],
        );
    }
}

fn pct(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (100.0 * used as f64 / total as f64) as f32
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep: String = s.chars().rev().take(max - 1).collect::<Vec<_>>().into_iter().rev().collect();
        format!("…{keep}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CoreGroup, History, ProcInfo, SharedState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn full_to_text(state: &SharedState, view: &View, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, state, view)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn processes_tab_sorts_by_selected_key() {
        let state = SharedState::default();
        state.procs.store(Some(std::sync::Arc::new(vec![
            ProcInfo { pid: 1, name: "AAA_high_cpu".into(), cpu_pct: 99.0, rss: 10 << 20, state: 'R' },
            ProcInfo { pid: 2, name: "BBB_high_mem".into(), cpu_pct: 1.0, rss: 900 << 20, state: 'S' },
        ])));
        let pos = |t: &str, needle: &str| t.lines().position(|l| l.contains(needle));

        let mut view = View { tab: Tab::Processes, paused: false, proc_scroll: 0, proc_sort: ProcSort::Cpu };
        let t = full_to_text(&state, &view, 80, 20);
        assert!(pos(&t, "AAA_high_cpu") < pos(&t, "BBB_high_mem"), "CPU sort order wrong");

        view.proc_sort = ProcSort::Mem;
        let t = full_to_text(&state, &view, 80, 20);
        assert!(pos(&t, "BBB_high_mem") < pos(&t, "AAA_high_cpu"), "MEM sort order wrong");

        view.proc_sort = ProcSort::Pid;
        let t = full_to_text(&state, &view, 80, 20);
        assert!(pos(&t, "AAA_high_cpu") < pos(&t, "BBB_high_mem"), "PID sort order wrong");
    }

    fn synth(core_groups: Vec<CoreGroup>, ncpu: usize) -> CpuSnapshot {
        CpuSnapshot {
            model: "TestCPU".into(),
            per_core: (0..ncpu).map(|i| (i * 7 % 100) as f32).collect(),
            usage: 12.0,
            usage_hist: History::new(),
            mem_used: 1 << 30,
            mem_total: 4 << 30,
            swap_used: 0,
            swap_total: 0,
            mem_hist: History::new(),
            mem_available: 3 << 30,
            mem_cached: 1 << 29,
            load: [0.0; 3],
            temp_c: Some(40.0),
            freq_mhz: Some(3000.0),
            uptime_secs: 60,
            tasks_total: 100,
            tasks_running: 1,
            core_groups,
        }
    }

    fn render_to_text(s: &CpuSnapshot, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_cpu(f, f.area(), s)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn dual_socket_has_labels_on_separate_rows() {
        // 2 sockets × 2 cores × 2 threads (cpu 0..8).
        let groups = vec![
            CoreGroup { package: 0, cpus: vec![0, 1] },
            CoreGroup { package: 0, cpus: vec![2, 3] },
            CoreGroup { package: 1, cpus: vec![4, 5] },
            CoreGroup { package: 1, cpus: vec![6, 7] },
        ];
        let text = render_to_text(&synth(groups, 8), 90, 14);
        eprintln!("---- dual socket ----\n{text}");
        assert!(text.contains("S0"), "missing S0 label");
        assert!(text.contains("S1"), "missing S1 label");
        // S0 and S1 must be on different rows.
        let row_of = |needle: &str| text.lines().position(|l| l.contains(needle));
        assert_ne!(row_of("S0"), row_of("S1"), "sockets should be on separate rows");
    }

    #[test]
    fn single_socket_has_no_label() {
        let groups = vec![
            CoreGroup { package: 0, cpus: vec![0, 1] },
            CoreGroup { package: 0, cpus: vec![2, 3] },
        ];
        let text = render_to_text(&synth(groups, 4), 90, 14);
        eprintln!("---- single socket ----\n{text}");
        assert!(!text.contains("S0"), "single socket should not be labelled");
    }
}
