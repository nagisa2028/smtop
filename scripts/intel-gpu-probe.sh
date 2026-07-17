#!/usr/bin/env bash
# Collect the read-only Linux interfaces relevant to adding Intel GPU support
# to smtop. This script never installs packages, changes sysfs/debugfs values,
# or records process command lines, environment variables, display EDIDs, IP
# addresses, or kernel logs.
#
# Run as your normal desktop user first, preferably while a GPU workload (video,
# WebGL, a game, etc.) is active so the two counter snapshots differ:
#
#   ./scripts/intel-gpu-probe.sh
#
# It writes intel-gpu-probe-<timestamp>.txt in the current directory. An
# explicit output path can be supplied as the only argument. A second run with
# sudo is optional and useful only when the report says debugfs/fdinfo was not
# readable; review the script before doing that.

set -uo pipefail
export LC_ALL=C
umask 077
shopt -s nullglob

timestamp=$(date -u +%Y%m%dT%H%M%SZ)
output=${1:-"intel-gpu-probe-${timestamp}.txt"}
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/smtop-intel-probe.XXXXXX") || exit 1
trap 'rm -rf -- "$tmp_dir"' EXIT

if [[ $# -gt 1 ]]; then
    printf 'usage: %s [output-file]\n' "$0" >&2
    exit 2
fi

if ! : >"$output"; then
    printf 'cannot write report: %s\n' "$output" >&2
    exit 1
fi

exec > >(tee "$output") 2>&1

section() {
    printf '\n===== %s =====\n' "$1"
}

command_line() {
    printf '$'
    printf ' %q' "$@"
    printf '\n'
    "$@"
    local status=$?
    if ((status != 0)); then
        printf '[exit status: %d]\n' "$status"
    fi
    return 0
}

read_value() {
    local path=$1
    [[ -e $path ]] || return 0
    printf '%s = ' "$path"
    if [[ ! -r $path ]]; then
        printf '<not readable>\n'
        return 0
    fi
    # All selected files are text sysfs/procfs attributes. Cap unexpected
    # output so a future driver cannot make the report unreasonably large.
    local value
    value=$(head -c 65536 -- "$path" 2>&1)
    local status=$?
    if ((status != 0)); then
        printf '<read failed: %s>\n' "$value"
    elif [[ $value == *$'\n'* ]]; then
        printf '\n%s\n' "$value"
    else
        printf '%s\n' "$value"
    fi
}

read_selected_files() {
    local path
    for path in "$@"; do
        [[ -f $path ]] && read_value "$path"
    done
}

tool_version() {
    local tool=$1
    if command -v "$tool" >/dev/null 2>&1; then
        printf '%-18s %s\n' "$tool" "$(command -v "$tool")"
    else
        printf '%-18s <not installed>\n' "$tool"
    fi
}

driver_name() {
    local device=$1
    local target
    target=$(readlink -f "$device/driver" 2>/dev/null) || return 0
    basename "$target"
}

section "report notes"
printf '%s\n' \
    'Purpose: discover stable Intel i915/xe telemetry available to smtop.' \
    'Collection is read-only. No process names, command lines, EDIDs, logs, or network data are included.' \
    'For useful deltas, keep a GPU workload active during the two snapshots one second apart.' \
    'Run as a normal user first; sudo is optional only for diagnosing permission gaps.'
printf 'report_utc = %s\n' "$timestamp"
printf 'effective_uid = %s\n' "$(id -u)"
groups=" $(id -Gn 2>/dev/null || true) "
[[ $groups == *' video '* ]] && in_video=yes || in_video=no
[[ $groups == *' render '* ]] && in_render=yes || in_render=no
printf 'member_of_video_group = %s\n' "$in_video"
printf 'member_of_render_group = %s\n' "$in_render"

section "operating system"
command_line uname -srvmo
if [[ -r /etc/os-release ]]; then
    # Product/version fields only; avoid distro-specific URLs and unrelated
    # metadata that do not affect GPU support.
    command_line awk -F= '/^(ID|VERSION_ID|PRETTY_NAME)=/{print}' /etc/os-release
fi
command_line getconf LONG_BIT

section "available diagnostic tools"
for tool in lspci intel_gpu_top gputop perf glxinfo vulkaninfo vainfo clinfo jq findmnt; do
    tool_version "$tool"
done

section "DRM device nodes and permissions"
if [[ -d /dev/dri ]]; then
    command_line ls -l /dev/dri
else
    printf '/dev/dri is absent\n'
fi
for node in /dev/dri/card* /dev/dri/renderD*; do
    command_line stat -Lc '%n type=%F major_minor=%t:%T mode=%a owner=%U group=%G' "$node"
done

section "PCI display controllers"
if command -v lspci >/dev/null 2>&1; then
    # -D includes the PCI domain, -nn keeps numeric IDs, and this awk program
    # emits only display-controller blocks rather than the full machine inventory.
    lspci -Dnnk 2>&1 | awk '
        /^[[:xdigit:]]{4}:[[:xdigit:]]{2}:[[:xdigit:]]{2}\.[[:xdigit:]].*(VGA compatible controller|3D controller|Display controller)/ {
            printing=1; print; next
        }
        printing && /^\t/ { print; next }
        printing { printing=0 }
    '
else
    printf 'lspci is not installed\n'
fi

section "kernel permission controls"
read_value /proc/sys/kernel/perf_event_paranoid
read_value /proc/sys/dev/i915/perf_stream_paranoid
read_value /proc/sys/dev/xe/observation_paranoid
if command -v findmnt >/dev/null 2>&1; then
    command_line findmnt -no TARGET,FSTYPE,OPTIONS /sys/kernel/debug
fi

section "loaded Intel GPU modules"
for module in i915 xe; do
    if [[ -d /sys/module/$module ]]; then
        printf '%s: loaded\n' "$module"
        read_value "/sys/module/$module/version"
        for parameter in "/sys/module/$module/parameters/"*; do
            read_value "$parameter"
        done
    else
        printf '%s: not loaded\n' "$module"
    fi
done

intel_cards=()
for card in /sys/class/drm/card[0-9]*; do
    [[ $(basename "$card") =~ ^card[0-9]+$ ]] || continue
    device=$(readlink -f "$card/device" 2>/dev/null) || continue
    vendor=$(head -n 1 "$device/vendor" 2>/dev/null || true)
    driver=$(driver_name "$device")
    if [[ $vendor == 0x8086 || $driver == i915 || $driver == xe ]]; then
        intel_cards+=("$card")
    fi
done

section "Intel DRM inventory"
if ((${#intel_cards[@]} == 0)); then
    printf 'No Intel DRM card using vendor 0x8086, i915, or xe was found.\n'
else
    printf 'intel_card_count = %d\n' "${#intel_cards[@]}"
fi

for card in "${intel_cards[@]}"; do
    card_name=$(basename "$card")
    card_minor=${card_name#card}
    device=$(readlink -f "$card/device")
    driver=$(driver_name "$device")

    section "$card_name identity and topology"
    printf 'card = %s\n' "$card"
    printf 'device = %s\n' "$device"
    printf 'driver = %s\n' "${driver:-<unknown>}"
    read_selected_files \
        "$card/dev" \
        "$device/vendor" \
        "$device/device" \
        "$device/revision" \
        "$device/subsystem_vendor" \
        "$device/subsystem_device" \
        "$device/class" \
        "$device/boot_vga" \
        "$device/enable" \
        "$device/numa_node" \
        "$device/modalias" \
        "$device/uevent"

    printf 'DRM nodes sharing this PCI device:\n'
    for node in /sys/class/drm/card[0-9]* /sys/class/drm/renderD*; do
        [[ -e $node/device ]] || continue
        [[ $(readlink -f "$node/device") == "$device" ]] || continue
        printf '  %s dev=%s\n' "$(basename "$node")" "$(head -n 1 "$node/dev" 2>/dev/null || printf '?')"
    done

    section "$card_name runtime power"
    read_selected_files \
        "$device/power/runtime_status" \
        "$device/power/runtime_usage" \
        "$device/power/runtime_active_time" \
        "$device/power/runtime_suspended_time" \
        "$device/power/control" \
        "$device/power/autosuspend_delay_ms"

    section "$card_name telemetry sysfs file inventory"
    # Names reveal which kernel ABI generation is present. Contents are read
    # separately from a conservative allow-list below.
    find "$device" -maxdepth 6 -type f -printf '%P\n' 2>&1 \
        | grep -E '(^|/)(gt|tile|freq|throttle|hwmon|mem|memory|vram|gtt|power|energy|temp|fan|engine|busy)' \
        | sort -u \
        | head -n 1000

    section "$card_name frequency, throttle, and memory values (snapshot 1)"
    sysfs_values=(
        "$card"/gt_*_freq_mhz
        "$card"/gt/gt*/id
        "$card"/gt/gt*/rps_*_freq_mhz
        "$card"/gt/gt*/punit_req_freq_mhz
        "$card"/gt/gt*/throttle_reason*
        "$card"/power/rc6_enable
        "$card"/power/rc6_residency_ms
        "$device"/gt_*_freq_mhz
        "$device"/gt/gt*/id
        "$device"/gt/gt*/rps_*_freq_mhz
        "$device"/gt/gt*/throttle_reason*
        "$device"/tile*/gt*/freq*/act_freq
        "$device"/tile*/gt*/freq*/cur_freq
        "$device"/tile*/gt*/freq*/rpn_freq
        "$device"/tile*/gt*/freq*/rpa_freq
        "$device"/tile*/gt*/freq*/rpe_freq
        "$device"/tile*/gt*/freq*/rp0_freq
        "$device"/tile*/gt*/freq*/min_freq
        "$device"/tile*/gt*/freq*/max_freq
        "$device"/tile*/gt*/freq*/throttle/status
        "$device"/tile*/gt*/freq*/throttle/reasons
        "$device"/mem_info_*
        "$device"/memory_region*/*_size
    )
    read_selected_files "${sysfs_values[@]}"

    section "$card_name hwmon"
    hwmon_dirs=("$device"/hwmon/hwmon*)
    if ((${#hwmon_dirs[@]} == 0)); then
        printf 'No hwmon directory found below the Intel PCI device.\n'
    fi
    for hwmon in "${hwmon_dirs[@]}"; do
        printf -- '-- %s --\n' "$hwmon"
        read_selected_files \
            "$hwmon/name" \
            "$hwmon"/temp*_label "$hwmon"/temp*_input \
            "$hwmon"/power*_label "$hwmon"/power*_average "$hwmon"/power*_input \
            "$hwmon"/energy*_label "$hwmon"/energy*_input \
            "$hwmon"/fan*_label "$hwmon"/fan*_input \
            "$hwmon"/freq*_label "$hwmon"/freq*_input
    done

    section "$card_name debugfs availability"
    debug_dir="/sys/kernel/debug/dri/$card_minor"
    if [[ ! -d /sys/kernel/debug/dri ]]; then
        printf '/sys/kernel/debug/dri is absent (debugfs may not be mounted).\n'
    elif [[ ! -x /sys/kernel/debug/dri ]]; then
        printf '/sys/kernel/debug/dri exists but cannot be searched by this user.\n'
    elif [[ ! -d $debug_dir ]]; then
        printf '%s is absent (debugfs may not be mounted).\n' "$debug_dir"
    elif [[ ! -r $debug_dir ]]; then
        printf '%s exists but is not readable by this user.\n' "$debug_dir"
    else
        printf 'debugfs directory = %s\n' "$debug_dir"
        find "$debug_dir" -maxdepth 2 -type f -printf '%P\n' 2>&1 | sort | head -n 1000
        # Small, read-only diagnostics useful for validating fallback metrics.
        read_selected_files \
            "$debug_dir/i915_frequency_info" \
            "$debug_dir/i915_engine_info" \
            "$debug_dir/i915_drpc_info" \
            "$debug_dir/i915_runtime_pm_status" \
            "$debug_dir/i915_capabilities" \
            "$debug_dir/i915_gem_objects" \
            "$debug_dir/i915_memory_region_info" \
            "$debug_dir/gt0/frequency"
    fi
done

section "direct Intel render-node fdinfo schema"
for card in "${intel_cards[@]}"; do
    device=$(readlink -f "$card/device")
    for render in /sys/class/drm/renderD*; do
        [[ -e $render/device ]] || continue
        [[ $(readlink -f "$render/device") == "$device" ]] || continue
        node="/dev/dri/$(basename "$render")"
        printf -- '-- %s --\n' "$node"
        if [[ ! -r $node || ! -w $node ]]; then
            printf '<render node is not readable and writable by this user>\n'
            continue
        fi
        # Merely opening a render node creates a DRM client and lets us inspect
        # the driver-provided fdinfo schema without issuing GPU ioctls or work.
        if exec {probe_fd}<>"$node" 2>/dev/null; then
            awk '/^drm-/{print}' "/proc/$$/fdinfo/$probe_fd" 2>&1
            exec {probe_fd}>&-
        else
            printf '<failed to open render node>\n'
        fi
    done
done

section "Intel GPU PMU event sources"
pmu_dirs=(/sys/bus/event_source/devices/i915 /sys/bus/event_source/devices/i915_* /sys/bus/event_source/devices/xe /sys/bus/event_source/devices/xe_*)
declare -A seen_pmu=()
for pmu in "${pmu_dirs[@]}"; do
    [[ -d $pmu ]] || continue
    real_pmu=$(readlink -f "$pmu")
    [[ -z ${seen_pmu[$real_pmu]+x} ]] || continue
    seen_pmu[$real_pmu]=1
    printf -- '-- %s --\n' "$pmu"
    read_selected_files "$pmu/type" "$pmu/cpumask"
    for event in "$pmu"/events/* "$pmu"/format/*; do
        read_value "$event"
    done
done
if ((${#seen_pmu[@]} == 0)); then
    printf 'No i915/xe perf PMU event source found.\n'
fi

section "Intel RAPL/powercap energy sources (snapshot 1)"
powercap_values=()
for zone in /sys/class/powercap/intel-rapl*; do
    [[ -d $zone ]] || continue
    powercap_values+=("$zone/name" "$zone/energy_uj" "$zone/max_energy_range_uj")
done
if ((${#powercap_values[@]})); then
    read_selected_files "${powercap_values[@]}"
else
    printf 'No intel-rapl powercap zones found.\n'
fi

snapshot_fdinfo() {
    local label=$1
    local count=0 proc_dir fd target info content driver pdev client key
    local -A seen_client=()

    section "Intel DRM fdinfo $label"
    for proc_dir in /proc/[0-9]*; do
        [[ -d $proc_dir/fd ]] || continue
        for fd in "$proc_dir"/fd/[0-9]*; do
            target=$(readlink "$fd" 2>/dev/null) || continue
            case $target in
                /dev/dri/card*|/dev/dri/renderD*) ;;
                *) continue ;;
            esac
            info="$proc_dir/fdinfo/${fd##*/}"
            [[ -r $info ]] || continue
            content=$(awk '/^drm-/{print}' "$info" 2>/dev/null) || continue
            [[ -n $content ]] || continue
            driver=$(awk -F: '/^drm-driver:/{gsub(/^[[:space:]]+|[[:space:]]+$/, "", $2); print $2; exit}' <<<"$content")
            [[ $driver == i915 || $driver == xe ]] || continue
            pdev=$(awk '/^drm-pdev:/{sub(/^[^:]*:[[:space:]]*/, ""); print; exit}' <<<"$content")
            client=$(awk '/^drm-client-id:/{sub(/^[^:]*:[[:space:]]*/, ""); print; exit}' <<<"$content")
            key="$driver|$pdev|$client"
            [[ -z ${seen_client[$key]+x} ]] || continue
            seen_client[$key]=1
            ((count += 1))
            printf -- '-- client sample %d (process identity intentionally omitted) --\n%s\n' "$count" "$content"
        done
    done
    printf 'readable_unique_intel_clients = %d\n' "$count"
    if ((count == 0)); then
        printf 'No readable i915/xe fdinfo clients were found. Keep a local GPU workload active and check /proc permissions.\n'
    fi
}

snapshot_fdinfo "snapshot 1"

section "counter wait"
printf 'Waiting 1 second before the second raw-counter snapshot...\n'
sleep 1

snapshot_fdinfo "snapshot 2 (+1 second)"

section "frequency, hwmon energy, and RAPL values (snapshot 2)"
for card in "${intel_cards[@]}"; do
    device=$(readlink -f "$card/device")
    sysfs_values=(
        "$card"/gt_*_freq_mhz
        "$card"/gt/gt*/rps_*_freq_mhz
        "$card"/gt/gt*/punit_req_freq_mhz
        "$card"/power/rc6_residency_ms
        "$device"/gt_*_freq_mhz
        "$device"/gt/gt*/rps_*_freq_mhz
        "$device"/tile*/gt*/freq*/act_freq
        "$device"/tile*/gt*/freq*/cur_freq
        "$device"/hwmon/hwmon*/energy*_input
        "$device"/hwmon/hwmon*/power*_average
    )
    read_selected_files "${sysfs_values[@]}"
done
read_selected_files "${powercap_values[@]}"

section "optional userspace probes"
if command -v glxinfo >/dev/null 2>&1; then
    command_line timeout 8s glxinfo -B
fi
if command -v vulkaninfo >/dev/null 2>&1; then
    # UUIDs are not needed for implementation and are removed from the report.
    printf '$ timeout 8s vulkaninfo --summary (UUID lines omitted)\n'
    timeout 8s vulkaninfo --summary 2>&1 | grep -v -E '^[[:space:]]*(deviceUUID|driverUUID)[[:space:]]*='
    printf '[pipeline status: %s]\n' "${PIPESTATUS[*]}"
fi
if command -v vainfo >/dev/null 2>&1; then
    command_line timeout 8s vainfo
fi
if command -v clinfo >/dev/null 2>&1; then
    command_line timeout 8s clinfo -l
fi
if command -v intel_gpu_top >/dev/null 2>&1 && ((${#intel_cards[@]})); then
    printf '%s\n' 'intel_gpu_top device list:'
    command_line timeout 8s intel_gpu_top -L
    if command -v jq >/dev/null 2>&1; then
        printf '%s\n' 'intel_gpu_top JSON samples (client/PID/name fields removed):'
        gpu_top_json="$tmp_dir/intel_gpu_top.json"
        timeout 8s intel_gpu_top -J -s 500 -n 3 -o "$gpu_top_json" 2>&1
        gpu_top_status=$?
        if [[ -s $gpu_top_json ]]; then
            # intel_gpu_top versions emit either a stream of root objects or an
            # array. Slurp both forms, then recursively remove client identity.
            jq -s '
                walk(
                    if type == "object" then
                        del(.clients, .pid, .name, .command, .comm)
                    else . end
                )
            ' "$gpu_top_json" 2>&1
        else
            printf '<intel_gpu_top produced no JSON>\n'
        fi
        printf '[intel_gpu_top exit status: %d]\n' "$gpu_top_status"
    else
        printf '%s\n' 'intel_gpu_top sampling skipped because jq is unavailable; raw output may contain process identities.'
    fi
fi

section "report summary"
printf 'intel_cards = %d\n' "${#intel_cards[@]}"
printf 'report_file = %s\n' "$output"
printf '%s\n' \
    'Please return this report. If possible, note what workload was active.' \
    'A normal-user report is the priority; an optional sudo report can clarify only permission-limited interfaces.'
