# mon

混在GPU環境（NVIDIA RTX + AMD Radeon）向けのノード統合監視TUI。
CPU / RAM / GPU群 / Network / Disk I/O / Free Space を1画面・時系列で表示する。

既存ツールの穴を埋める:
- **btop** は AMD を ROCm SMI 経由で読むため、ROCm非対応のコンシューマRadeon/APUを列挙できない。
- **mon** は AMD を **amdgpu sysfs 直読み**（ROCm不要）、NVIDIA を **NVML** で取得するので、両ベンダーを同一画面に同時表示できる。

## レイアウト（3段）

1. **CPU / RAM** — コア毎使用率バー + 使用率/メモリ% 時系列 + loadavg + uptime + タスク数(running/total) + CPU温度/クロック + RAM/Swap ゲージ + メモリ内訳(available/cache)
2. **GPU** — GPU毎カード横並び（使用率/VRAM時系列、温度・電力・クロック・ファン・PCIe、APUはGTTも）
3. **Network | Disk I/O | Free Space** — 横3カラム（rate系は時系列グラフ、容量系はゲージ）。Networkはリンク速度/状態、DiskはI/O %util と IOPS も表示。幅が狭いと縦スタックにリフロー。

> 既知の未対応（今後）: プロセス一覧/プロセス毎の帰属（CPU/GPU/Disk/Net）、NVMe等のドライブ温度・マザボ系の全センサ列挙。

## ビルド / 実行

```sh
cargo build --release
./target/release/mon          # q または Esc / Ctrl-C で終了
```

- NVIDIA対応は `nvidia` feature（デフォルト有効）。`nvml-wrapper` が `libnvidia-ml` を**実行時dlopen**するため、ドライバが無い環境でもビルド・実行でき、その場合 NVIDIA GPU は単に表示されない。
- NVMLを完全に外したい場合: `cargo build --release --no-default-features`

## データ取得経路

| 指標 | ソース |
|------|--------|
| CPU / RAM / load | `/proc/stat`, `/proc/meminfo`, `/proc/loadavg` |
| AMD GPU | `/sys/class/drm/card*/device/`（`gpu_busy_percent`, `mem_info_vram/gtt_*`, hwmon, `pp_dpm_*`） |
| NVIDIA GPU | NVML（`nvml-wrapper`） |
| Network | `/proc/net/dev` |
| Disk I/O | `/proc/diskstats`（物理デバイスのみ） |
| Free Space | `/proc/mounts` + `statvfs` |

各コレクタは専用スレッドで drift-correct ticker により周期収集し、履歴付きスナップショットを `ArcSwap` でロックフリー公開する。UIは独立したFPSで最新値を描画するため、NVMLのストールやCPU高負荷が他指標の更新を妨げない。

### AMD GPU の取得経路と注意点

- 使用率・温度・電力・クロック・ファンは、まず**バイナリ `gpu_metrics` テーブル**（`gpu_metrics_v1_x`、単一 `read()` でデコード）から取得し、無ければレガシー sysfs（`gpu_busy_percent` / hwmon / `pp_dpm_*`）にフォールバックする。新しい discrete カード（例: RDNA4 / Navi 48）はレガシー経路が EBUSY を返すため `gpu_metrics` が主経路、APU はレガシーが主経路。
- **アイドル時のサスペンド**: discrete カードは無負荷だと runtime PM で D3cold サスペンドし、SMU テレメトリ（使用率/温度/電力/クロック）が読めなくなる（VRAM は読める）。mon はこの状態を `idle (suspended)` と表示する。GPU が実際に使われている間は `runtime_status=active` となりフル取得できる。アイドル時も値が欲しい場合は libdrm の `AMDGPU_INFO` ioctl 経路が必要だが、GPU を毎秒起こしてアイドル電力が上がるため採用していない。

## 対象環境での確認（mgt-gpu01）

```sh
cargo build --release && ./target/release/mon
# RTX(NVML) と Radeon(sysfs) が同一画面に同時表示されることを確認
# 参考: rocm-smi が Radeon を列挙しなくても mon は表示する
```

## v2（未実装）

プロセス単位のCPU+GPU統合表（AMD `fdinfo` / NVML process API）、多ホスト集約、PCI-id→製品名テーブル、設定ファイル、閾値アラート。
