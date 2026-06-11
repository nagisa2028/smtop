//! Filesystem free-space collector: `/proc/mounts` + `statvfs`.

use std::collections::HashSet;
use std::fs;
use std::time::Duration;

use super::Collector;
use crate::model::FsSnapshot;

/// Pseudo / virtual filesystem types to ignore.
const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "tmpfs",
    "devtmpfs",
    "devpts",
    "cgroup",
    "cgroup2",
    "overlay",
    "squashfs",
    "ramfs",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "mqueue",
    "hugetlbfs",
    "configfs",
    "fusectl",
    "autofs",
    "binfmt_misc",
    "efivarfs",
    "nsfs",
    "fuse.gvfsd-fuse",
    "fuse.portal",
];

pub struct FsCollector;

impl FsCollector {
    pub fn new() -> Self {
        Self
    }
}

impl Collector for FsCollector {
    type Out = Vec<FsSnapshot>;

    fn name(&self) -> &'static str {
        "fs"
    }

    fn interval(&self) -> Duration {
        // Free space changes slowly; sample less often than rates.
        Duration::from_millis(5000)
    }

    fn sample(&mut self) -> anyhow::Result<Vec<FsSnapshot>> {
        let content = fs::read_to_string("/proc/mounts")?;
        let mut seen = HashSet::new();
        let mut out = Vec::new();

        for line in content.lines() {
            let mut f = line.split_whitespace();
            let _device = f.next();
            let Some(mount_raw) = f.next() else { continue };
            let Some(fstype) = f.next() else { continue };

            if PSEUDO_FS.contains(&fstype) {
                continue;
            }
            let mount = unescape_octal(mount_raw);
            if !seen.insert(mount.clone()) {
                continue;
            }

            let Ok(st) = rustix::fs::statvfs(mount.as_str()) else {
                continue;
            };
            let frsize = st.f_frsize;
            let total = st.f_blocks * frsize;
            if total == 0 {
                continue;
            }
            let used = st.f_blocks.saturating_sub(st.f_bfree) * frsize;

            out.push(FsSnapshot { mount, used, total });
        }
        // Largest filesystems first.
        out.sort_by(|a, b| b.total.cmp(&a.total));
        Ok(out)
    }
}

/// Decode `\NNN` octal escapes that `/proc/mounts` uses for special chars.
fn unescape_octal(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = &s[i + 1..i + 4];
            if let Ok(code) = u8::from_str_radix(oct, 8) {
                out.push(code);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
