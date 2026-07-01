# smtop

[![CI](https://github.com/nagisa2028/smtop/actions/workflows/ci.yml/badge.svg)](https://github.com/nagisa2028/smtop/actions/workflows/ci.yml)

A single-screen terminal (TUI) node monitor: it shows **every NVIDIA and AMD GPU
on the box** — multiple cards, no ROCm needed — alongside CPU / RAM /
Network / Disk I/O / Free Space, with time-series history, all on one screen.
Few tools put a multi-GPU view and the rest of a node's compute/system
resources together in one place. Runs the same on single-GPU, multi-GPU,
single-vendor, or GPU-less hosts.

> **Status:** early solo project (v0.x), built by one developer with AI
> assistance. Tested on a limited set of hardware, so behavior on other GPUs
> may differ — bug reports and hardware feedback are very welcome. Provided
> as-is (see the Apache-2.0 license).

## Why another monitor?

- **btop** reads AMD GPUs through **ROCm SMI** (as of writing), so it can't
  enumerate consumer Radeon cards / APUs that ROCm doesn't support.
- **nvtop** unifies multiple GPU vendors, but it is **GPU-only** — no CPU,
  network, or disk.
- The common workaround is to run btop and a GPU tool side by side in tmux.

**smtop** reads AMD via the **amdgpu sysfs interface directly (no ROCm)** and
NVIDIA via **NVML**, so both vendors — plus the rest of the node — appear on the
same screen at a glance.

## Layout (3 rows)

1. **CPU / RAM** — per-core usage bars + usage/memory% history + loadavg +
   uptime + task counts (running/total) + CPU temperature/clock + RAM/Swap
   gauges + memory breakdown (available/cache).
2. **GPU** — one card panel per GPU, side by side (util/VRAM history,
   temperature, power, clock, fan, PCIe link width; APUs also show GTT).
3. **Network | Disk I/O | Free Space** — three columns (rates as time-series
   graphs, capacities as gauges). Network shows link speed/state; Disk shows I/O
   %util and IOPS. Reflows to a vertical stack when the terminal is narrow.

> Not implemented yet (future): NVMe/drive temperatures and full
> motherboard-sensor enumeration.

## Install

Download a prebuilt Linux x86_64 binary from the
[Releases](https://github.com/nagisa2028/smtop/releases) page (built on glibc
2.35, so it runs on any newer distro), or build from source below.

## Build / Run

```sh
cargo build --release
./target/release/smtop                  # q / Ctrl-C to quit, Esc backs out one level (quits on Overview), space to pause
./target/release/smtop --interval 500   # sampling interval (ms, default 1000)
./target/release/smtop --log smtop.log  # record collector errors to a file (for diagnosing new hardware)
./target/release/smtop --probe          # one-shot dump without a TTY (handy over SSH)
```

- The header shows hostname, time, tabs, and per-collector liveness: green =
  fresh, yellow = published but stale (the collector stopped updating — e.g. a
  driver died — so the on-screen data is frozen), red = never published.
- Tabs: `Tab`/`1`/`2`/`3` switch **Overview** (dashboard) / **Processes** (PID
  list) / **GPU** (per-GPU nvtop-style detail). `Esc` backs out one level (quits
  on Overview).
  - Processes columns: PID / CPU% / MEM / DISK R / DISK W / **GPU** (which GPU +
    util, e.g. `N0 45%`) / **VRAM** / STATE / COMMAND.
  - Sorting: cycle with `s`, or pick `c` (CPU) / `m` (MEM) / `d` (DISK R) /
    `D` (DISK W) / `g` (GPU%) / `G` (VRAM) / `p` (PID); `r` reverses the current
    sort. `↑↓` (or `j`/`k`) scrolls. The active column is marked `▾` (`▴` when
    reversed).
  - Per-process GPU: **NVIDIA via NVML for all processes** (VRAM for all; SM%
    best-effort, shown where the driver reports it).
    **AMD via `/proc/<pid>/fdinfo`** (`drm-total-vram` / `drm-engine-*`,
    de-duplicated by `drm-pdev` + `drm-client-id`). Like DISK I/O, seeing other
    users' processes needs **root or `setcap cap_sys_ptrace+ep smtop`**.
    Utilization is reported only while active (idle = 0).
  - DISK I/O comes from `/proc/<pid>/io`. Other users' processes need **root or
    `setcap cap_sys_ptrace+ep smtop`** (otherwise only your own processes are
    shown; the rest are `n/a`).
  - **`setcap` caveat**: a file capability applies to **every user who can
    execute the binary**. `CAP_SYS_PTRACE` is powerful (it permits reading other
    processes' memory), so on shared hosts restrict execution to a dedicated
    group (e.g. `chgrp smtopusers smtop && chmod 750 smtop`, then `setcap`). If
    you can't restrict it, prefer running as root/sudo instead of setcap.
  - **Per-process network bandwidth is not supported** (procfs exposes no
    per-PID bandwidth; it would require pcap/eBPF + root).
- NVIDIA support is behind the `nvidia` feature (on by default). `nvml-wrapper`
  **dlopens `libnvidia-ml` at runtime**, so smtop still builds and runs on hosts
  without the driver — NVIDIA GPUs simply don't appear.
- To drop NVML entirely: `cargo build --release --no-default-features`.

## Data sources

| Metric | Source |
|--------|--------|
| CPU / RAM / load | `/proc/stat`, `/proc/meminfo`, `/proc/loadavg` |
| AMD GPU | `/sys/class/drm/card*/device/` (binary `gpu_metrics` table first, then legacy `gpu_busy_percent`, `mem_info_vram/gtt_*`, hwmon, `pp_dpm_*`) |
| NVIDIA GPU | NVML (`nvml-wrapper`) |
| Network | `/proc/net/dev` |
| Disk I/O | `/proc/diskstats` (physical devices only) |
| Free Space | `/proc/mounts` + `statvfs` (unresponsive network mounts time out at 500 ms, are skipped for 60 s, then retried) |

Each collector samples on its own thread with a drift-correcting ticker and
publishes a history-bearing snapshot lock-free via `ArcSwap`. The UI renders the
latest values at an independent frame rate, so an NVML stall or high CPU load
never blocks other metrics from updating.

### AMD GPU details

- Utilization, temperature, power, clock, and fan are read first from the binary
  **`gpu_metrics` table** (v1.3 layout, decoded from a single `read()`; v1.4+ and
  APU v2.x aren't decoded and surface an "unsupported" note on the card), falling
  back to legacy sysfs (`gpu_busy_percent` / hwmon / `pp_dpm_*`) when it isn't
  present. Newer discrete cards (e.g. RDNA4 / Navi 48) return `EBUSY` on the
  legacy path, so `gpu_metrics` is the primary source there; APUs use the legacy
  path as primary.
- **Idle suspend**: with no load, discrete cards enter D3cold via runtime PM and
  their SMU telemetry (util/temp/power/clock) becomes unreadable (VRAM still
  reads). smtop shows this as `idle (suspended)`. While the GPU is actually in
  use, `runtime_status=active` and the full set reads back. Reading values at
  idle would require the libdrm `AMDGPU_INFO` ioctl path, but that wakes the GPU
  every second and raises idle power, so it isn't used.
- **GPU names** are resolved from `pci.ids`. smtop bundles an AMD-only snapshot
  compiled into the binary and overlays the host's system `pci.ids` on top
  (system wins per id, the bundle fills gaps). So recent APU iGPUs (Barcelo,
  Phoenix, Raphael, Rembrandt, …) still resolve on hosts whose system `pci.ids`
  predates that hardware, instead of falling back to a raw PCI id. (Hardware
  newer than the bundled snapshot and absent from the system database still
  falls back.)

## Roadmap (not implemented)

A config file and threshold alerts.

## Releases & development

Single trunk: development happens on `master`, which CI keeps green
(`fmt`/`clippy`/tests on every push). A release is cut by pushing a `vX.Y.Z`
tag — CI then builds the binary and uploads it to the matching
[GitHub Release](https://github.com/nagisa2028/smtop/releases). No long-lived
release branches; fixes land on `master` and go out with the next tag.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

The bundled `src/collector/pci_ids_amd.txt` is a subset of the [pci.ids
database](https://pci-ids.ucw.cz/), redistributed under its BSD terms (pci.ids
is dual-licensed BSD / GPLv2+); see the file header for attribution.
