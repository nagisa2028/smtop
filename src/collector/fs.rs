//! Filesystem free-space collector: `/proc/mounts` + `statvfs`.
//!
//! `statvfs` can block indefinitely on a dead network mount (e.g. an NFS
//! server that went away), so calls run on a helper thread bounded by a
//! timeout. On timeout the helper (stuck in the uncancellable syscall) is
//! parked with its mount, and retries poll that parked helper's channel
//! instead of probing again — so a permanently dead mount costs one stuck
//! thread total, not one per retry.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::Collector;
use crate::model::FsSnapshot;

/// How long one `statvfs` may take before the mount is considered hung.
const STAT_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a hung mount stays blacklisted before being probed again.
const RETRY_AFTER: Duration = Duration::from_secs(60);

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

pub struct FsCollector {
    worker: Option<Worker>,
    /// Mounts whose `statvfs` timed out, each holding the worker that is
    /// still stuck on it.
    hung: HashMap<String, HungMount>,
}

/// A parked worker whose `statvfs` call never returned, plus when its channel
/// was last polled for a late answer.
struct HungMount {
    worker: Worker,
    checked: Instant,
}

/// Outcome of polling a hung mount's parked worker.
#[derive(Debug, PartialEq)]
enum HungPoll {
    /// Still stuck in the syscall; keep skipping this mount.
    Stuck,
    /// The call finally returned — the mount is live again (or errored).
    Recovered(Option<(u64, u64)>),
    /// The parked worker is gone (thread died); probe the mount normally.
    Reprobe,
}

/// Helper thread running `statvfs` so the collector can bound it by a timeout.
/// Requests and responses stay in lockstep: one outstanding call at a time,
/// and the whole worker is dropped on timeout (never reused out of sync).
struct Worker {
    req: mpsc::Sender<String>,
    res: mpsc::Receiver<(String, Option<(u64, u64)>)>,
}

fn spawn_worker() -> Worker {
    let (req_tx, req_rx) = mpsc::channel::<String>();
    let (res_tx, res_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("fs-statvfs".into())
        .spawn(move || {
            while let Ok(mount) = req_rx.recv() {
                let r = statvfs_used_total(&mount);
                if res_tx.send((mount, r)).is_err() {
                    break; // collector abandoned us after a timeout
                }
            }
        })
        .expect("failed to spawn statvfs worker");
    Worker {
        req: req_tx,
        res: res_rx,
    }
}

/// `(used, total)` bytes for a mount, or `None` on error / zero-sized fs.
fn statvfs_used_total(mount: &str) -> Option<(u64, u64)> {
    let st = rustix::fs::statvfs(mount).ok()?;
    let total = st.f_blocks * st.f_frsize;
    (total > 0).then(|| (st.f_blocks.saturating_sub(st.f_bfree) * st.f_frsize, total))
}

impl FsCollector {
    pub fn new() -> Self {
        Self {
            worker: None,
            hung: HashMap::new(),
        }
    }

    /// `statvfs` via the worker thread, bounded by `STAT_TIMEOUT`. On timeout
    /// the worker is parked with the mount (its thread cannot be cancelled)
    /// and the mount is skipped until `poll_hung` sees the call return.
    fn stat_with_timeout(&mut self, mount: &str) -> Option<(u64, u64)> {
        let worker = self.worker.get_or_insert_with(spawn_worker);
        if worker.req.send(mount.to_string()).is_err() {
            self.worker = None; // worker died; respawn on the next call
            return None;
        }
        match worker.res.recv_timeout(STAT_TIMEOUT) {
            Ok((m, r)) => {
                debug_assert_eq!(m, mount);
                r
            }
            Err(_) => {
                if let Some(stuck) = self.worker.take() {
                    self.hung.insert(
                        mount.to_string(),
                        HungMount {
                            worker: stuck,
                            checked: Instant::now(),
                        },
                    );
                }
                super::log_line(&format!("[fs] statvfs timed out on {mount}; skipping it"));
                None
            }
        }
    }

    /// Check whether a hung mount's parked worker has answered yet.
    fn poll_hung(&mut self, mount: &str) -> HungPoll {
        let Some(h) = self.hung.get_mut(mount) else {
            return HungPoll::Reprobe;
        };
        h.checked = Instant::now();
        match h.worker.res.try_recv() {
            Ok((_, r)) => {
                self.hung.remove(mount);
                super::log_line(&format!("[fs] statvfs recovered on {mount}"));
                HungPoll::Recovered(r)
            }
            Err(mpsc::TryRecvError::Empty) => HungPoll::Stuck,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.hung.remove(mount);
                HungPoll::Reprobe
            }
        }
    }
}

impl Collector for FsCollector {
    type Out = Vec<FsSnapshot>;

    fn name(&self) -> &'static str {
        "fs"
    }

    fn interval(&self) -> Duration {
        // Free space changes slowly; sample less often than rates.
        super::sample_interval() * 5
    }

    fn sample(&mut self) -> anyhow::Result<Vec<FsSnapshot>> {
        let content = fs::read_to_string("/proc/mounts")?;
        let mut out = Vec::new();

        for mount in parse_mounts(&content) {
            // Hung mounts: never probe again (each probe would leak a stuck
            // thread). Instead, periodically poll the parked worker.
            if let Some(h) = self.hung.get(&mount) {
                if !retry_due(h.checked, Instant::now()) {
                    continue;
                }
                match self.poll_hung(&mount) {
                    HungPoll::Stuck => continue,
                    HungPoll::Recovered(r) => {
                        if let Some((used, total)) = r {
                            out.push(FsSnapshot { mount, used, total });
                        }
                        continue; // probe normally from the next sample on
                    }
                    HungPoll::Reprobe => {}
                }
            }

            let Some((used, total)) = self.stat_with_timeout(&mount) else {
                continue;
            };
            out.push(FsSnapshot { mount, used, total });
        }
        // Largest filesystems first.
        sort_filesystems(&mut out);
        Ok(out)
    }
}

fn parse_mounts(content: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut mounts = Vec::new();
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let _device = fields.next();
        let Some(mount_raw) = fields.next() else {
            continue;
        };
        let Some(fstype) = fields.next() else {
            continue;
        };
        if PSEUDO_FS.contains(&fstype) {
            continue;
        }
        let mount = unescape_octal(mount_raw);
        if seen.insert(mount.clone()) {
            mounts.push(mount);
        }
    }
    mounts
}

fn retry_due(checked: Instant, now: Instant) -> bool {
    now.saturating_duration_since(checked) >= RETRY_AFTER
}

fn sort_filesystems(filesystems: &mut [FsSnapshot]) {
    filesystems.sort_by_key(|f| std::cmp::Reverse(f.total));
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
        // `get` (not slicing) so a multibyte char after `\` can't panic on a
        // char-boundary violation.
        if bytes[i] == b'\\'
            && let Some(oct) = s.get(i + 1..i + 4)
            && let Ok(code) = u8::from_str_radix(oct, 8)
        {
            out.push(code);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_timeout_returns_for_root_and_rejects_missing() {
        let mut c = FsCollector::new();
        let (used, total) = c.stat_with_timeout("/").expect("statvfs / should succeed");
        assert!(total > 0 && used <= total);
        // An error (not a hang) returns None without blacklisting.
        assert!(c.stat_with_timeout("/nonexistent-mon-test").is_none());
        assert!(c.hung.is_empty());
    }

    /// A parked worker with a hand-built channel, simulating a hung statvfs.
    fn park(c: &mut FsCollector, mount: &str) -> mpsc::Sender<(String, Option<(u64, u64)>)> {
        let (req, _) = mpsc::channel(); // poll_hung never touches the req side
        let (res_tx, res) = mpsc::channel();
        c.hung.insert(
            mount.to_string(),
            HungMount {
                worker: Worker { req, res },
                checked: Instant::now(),
            },
        );
        res_tx
    }

    #[test]
    fn hung_mount_recovers_via_parked_worker_poll() {
        let mut c = FsCollector::new();
        let res_tx = park(&mut c, "/dead");
        // Still stuck: nothing on the channel, entry stays parked.
        assert_eq!(c.poll_hung("/dead"), HungPoll::Stuck);
        assert!(c.hung.contains_key("/dead"));
        // The stuck call finally returns -> recovered with its result.
        res_tx.send(("/dead".into(), Some((1, 2)))).unwrap();
        assert_eq!(c.poll_hung("/dead"), HungPoll::Recovered(Some((1, 2))));
        assert!(c.hung.is_empty());
    }

    #[test]
    fn hung_mount_with_dead_worker_reprobes() {
        let mut c = FsCollector::new();
        drop(park(&mut c, "/dead")); // res sender dropped = worker thread died
        assert_eq!(c.poll_hung("/dead"), HungPoll::Reprobe);
        assert!(c.hung.is_empty());
    }

    #[test]
    fn unescape_octal_decodes_and_passes_through() {
        assert_eq!(unescape_octal("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape_octal("/plain"), "/plain");
        assert_eq!(unescape_octal("/a\\zz"), "/a\\zz"); // non-octal stays literal
        assert_eq!(unescape_octal("/end\\04"), "/end\\04"); // truncated escape
    }

    #[test]
    fn unescape_octal_multibyte_after_backslash_no_panic() {
        // A multibyte char right after `\` must not panic on byte slicing.
        assert_eq!(unescape_octal("/mnt/\\あ"), "/mnt/\\あ");
    }

    #[test]
    fn mount_parser_filters_pseudo_filesystems_decodes_and_deduplicates() {
        let mounts = parse_mounts(
            "dev / ext4 rw 0 0\nproc /proc proc rw 0 0\nother / ext4 rw 0 0\ndev /mnt/my\\040disk xfs rw 0 0\nbad\n",
        );
        assert_eq!(mounts, ["/", "/mnt/my disk"]);
    }

    #[test]
    fn retry_boundary_and_filesystem_sorting_are_deterministic() {
        let now = Instant::now();
        assert!(!retry_due(now - Duration::from_secs(59), now));
        assert!(retry_due(now - RETRY_AFTER, now));

        let mut filesystems = [
            FsSnapshot {
                mount: "/small".into(),
                used: 1,
                total: 10,
            },
            FsSnapshot {
                mount: "/large".into(),
                used: 1,
                total: 100,
            },
        ];
        sort_filesystems(&mut filesystems);
        assert_eq!(filesystems[0].mount, "/large");
    }
}
