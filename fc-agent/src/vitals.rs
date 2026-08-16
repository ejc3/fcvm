//! Guest vitals: a fork-free snapshot of the resources that make container
//! operations fail silently.
//!
//! Why this exists. Issue #841: `podman exec` died in the guest with
//! `container create failed (no logs from conmon): conmon bytes ""`, twice, on
//! two different commits. Nothing could say why, because `--log-driver=none`
//! discards conmon's stderr and the VM is torn down before anyone looks. Host
//! evidence excluded the host (127 GB free, load 0.84/32, no OOM); the
//! remaining candidates all live in the guest and none of them were recorded.
//!
//! Why it must not fork. The obvious design is to pull the state from the host
//! with `fcvm exec --pid P --vm -- sh -c '...'`. That cannot work for the cases
//! that matter: serving an exec makes fc-agent fork a child, so a guest out of
//! pids or threads fails the collection for the same reason it failed the
//! operation. Everything here is `read`/`readdir`/`statvfs` on already-open
//! filesystems from an already-running process, so it still answers when the
//! guest can no longer create a process.
//!
//! Cost: one ~200 byte line per 10s over the serial console at DEBUG, which the
//! host writes to the per-VM file and keeps out of the job log.

use std::fmt::Write as _;
use std::time::Duration;

/// Cap on any single collected section, so one pathological file cannot push
/// the useful sections out of a bounded console line.
const SECTION_LIMIT: usize = 4096;

/// Read a procfs file without failing the whole snapshot when it is missing.
///
/// A missing file is reported as such rather than skipped. A section that
/// silently vanishes is indistinguishable from a section whose answer was
/// "nothing", and this file exists precisely because that distinction was lost.
fn read_proc(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))
}

/// Keep only the lines whose first token is in `keys`, in file order.
fn filter_keys(raw: &str, keys: &[&str]) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let key = line.split(':').next().unwrap_or("").trim();
        let first = line.split_whitespace().next().unwrap_or("");
        if keys.contains(&key) || keys.contains(&first) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(
                line.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .as_str(),
            );
        }
        if out.len() > SECTION_LIMIT {
            out.push_str(" ...truncated");
            break;
        }
    }
    out
}

/// The single most decisive counter for "did the kernel kill something".
///
/// `/proc/vmstat`'s `oom_kill` is monotonic and cannot wrap. The guest's
/// printk ring is 128 KiB (`CONFIG_LOG_BUF_SHIFT=17`) and a chatty boot does
/// wrap it, so a dmesg-only OOM check intermittently cannot fire. Prefer this.
fn oom_kill_count() -> Option<u64> {
    let raw = read_proc("/proc/vmstat").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("oom_kill ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Free pseudo-terminals, as `(allocated, max)`.
///
/// conmon allocates a PTY for every `-t` exec. `/proc/sys/kernel/pty/nr`
/// against `pty/max` is the authoritative count; counting `/dev/pts` entries
/// is a readdir that agrees only when no other namespace holds one.
fn pty_usage() -> Option<(u64, u64)> {
    let nr = read_proc("/proc/sys/kernel/pty/nr")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let max = read_proc("/proc/sys/kernel/pty/max")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((nr, max))
}

/// Free space on a filesystem, as `(free_bytes, free_inodes)`.
///
/// Only ever call this on a path whose filesystem is known-local. `statvfs` on
/// a wedged FUSE mount parks in uninterruptible sleep and no timeout can
/// cancel it, which would turn a diagnostic into a second hang.
// The casts below are redundant on this target and NOT on every target: musl
// and glibc disagree on statvfs field widths, and fc-agent ships as a musl
// binary. Keep them, per AGENTS.md on libc types whose width the libc chooses.
#[allow(clippy::unnecessary_cast)]
fn statvfs_free(path: &str) -> Option<(u64, u64)> {
    let c_path = std::ffi::CString::new(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and stat is owned here.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    Some((
        (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64),
        stat.f_favail as u64,
    ))
}

/// Filesystem type for a mount point, from `/proc/self/mountinfo`.
///
/// Used to decide whether [`statvfs_free`] is safe to call at all.
fn fstype_of(mount_point: &str) -> Option<String> {
    let raw = read_proc("/proc/self/mountinfo").ok()?;
    let mut found = None;
    for line in raw.lines() {
        let mut fields = line.split(" - ");
        let left = fields.next()?;
        let right = fields.next()?;
        let mount = left.split_whitespace().nth(4)?;
        if mount == mount_point {
            // Last match wins: a later mount shadows an earlier one.
            found = right.split_whitespace().next().map(str::to_string);
        }
    }
    found
}

/// `true` when `statvfs` on this path cannot park forever.
fn is_local_fs(fstype: &str) -> bool {
    matches!(
        fstype,
        "tmpfs" | "ext4" | "btrfs" | "xfs" | "devtmpfs" | "ramfs" | "overlay"
    )
}

/// Free space for a path, or the reason it was not measured.
fn space_report(path: &str) -> String {
    match fstype_of(path) {
        Some(fstype) if is_local_fs(&fstype) => match statvfs_free(path) {
            Some((bytes, inodes)) => {
                format!("{path}={}MiB/{inodes}inodes", bytes / (1024 * 1024))
            }
            None => format!("{path}=statvfs-failed"),
        },
        // Never statvfs a FUSE mount from a diagnostic: see statvfs_free.
        Some(fstype) => format!("{path}=skipped(fstype={fstype})"),
        None => format!("{path}=not-mounted"),
    }
}

/// Sum `pids.current` and the tightest `pids.max` across the cgroup tree.
///
/// Returns `(current, max)` for the cgroup this process is in, which under
/// `--cgroups=split` is the parent of the container's own cgroup.
fn cgroup_pids() -> Option<(String, String)> {
    let current = read_proc("/sys/fs/cgroup/pids.current").ok()?;
    let max = read_proc("/sys/fs/cgroup/pids.max").ok()?;
    Some((current.trim().to_string(), max.trim().to_string()))
}

/// One compact line, for the periodic sampler.
///
/// Deliberately short: it is emitted every 10s for the life of every VM, so it
/// carries only the fields that discriminate between the known failure modes.
pub fn sample_line() -> String {
    let mut out = String::new();
    let mem = read_proc("/proc/meminfo")
        .map(|raw| filter_keys(&raw, &["MemAvailable", "MemFree", "Committed_AS"]))
        .unwrap_or_else(|error| format!("meminfo-unavailable({error})"));
    let _ = write!(out, "{mem}");
    if let Some((nr, max)) = pty_usage() {
        let _ = write!(out, " pty={nr}/{max}");
    }
    if let Some(count) = oom_kill_count() {
        let _ = write!(out, " oom_kill={count}");
    }
    if let Some((current, max)) = cgroup_pids() {
        let _ = write!(out, " pids={current}/{max}");
    }
    let _ = write!(out, " {}", space_report("/run"));
    if let Ok(load) = read_proc("/proc/loadavg") {
        let _ = write!(out, " loadavg=[{}]", load.trim());
    }
    out
}

/// The full block, for failure sites.
pub fn snapshot() -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "loadavg: {}",
        read_proc("/proc/loadavg").unwrap_or_else(|e| e).trim()
    );
    let _ = writeln!(
        out,
        "meminfo: {}",
        read_proc("/proc/meminfo")
            .map(|raw| filter_keys(
                &raw,
                &[
                    "MemTotal",
                    "MemFree",
                    "MemAvailable",
                    "Cached",
                    "Dirty",
                    "Writeback",
                    "Committed_AS",
                    "CommitLimit",
                    "SUnreclaim",
                ]
            ))
            .unwrap_or_else(|e| e)
    );
    let _ = writeln!(
        out,
        "vmstat: {}",
        read_proc("/proc/vmstat")
            .map(|raw| filter_keys(
                &raw,
                &[
                    "oom_kill",
                    "pgscan_direct",
                    "pgsteal_direct",
                    "nr_free_pages",
                    "compact_fail"
                ]
            ))
            .unwrap_or_else(|e| e)
    );
    match pty_usage() {
        Some((nr, max)) => {
            let _ = writeln!(out, "pty: {nr}/{max} allocated/max");
        }
        None => {
            let _ = writeln!(out, "pty: unavailable");
        }
    }
    let _ = writeln!(
        out,
        "file-nr: {}",
        read_proc("/proc/sys/fs/file-nr")
            .unwrap_or_else(|e| e)
            .trim()
    );
    match cgroup_pids() {
        Some((current, max)) => {
            let _ = writeln!(out, "cgroup pids: {current}/{max}");
        }
        None => {
            let _ = writeln!(out, "cgroup pids: unavailable");
        }
    }
    let _ = writeln!(
        out,
        "space: {} {} {}",
        space_report("/run"),
        space_report("/tmp"),
        space_report("/")
    );
    let _ = writeln!(
        out,
        "limits: {}",
        read_proc("/proc/self/limits")
            .map(|raw| filter_keys(&raw, &["Max"]))
            .unwrap_or_else(|e| e)
    );
    out
}

/// [`snapshot`] on its own thread, abandoned if it outlives `budget`.
///
/// A procfs read normally cannot block, but a diagnostic must not be the thing
/// that hangs a failing guest. Abandoning the thread is deliberate: the process
/// is already on a failure path, and a leaked reader is cheaper than a wedge.
pub fn snapshot_bounded(budget: Duration) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(snapshot());
    });
    match rx.recv_timeout(budget) {
        Ok(text) => text,
        Err(_) => format!("VITALS TRUNCATED: collection exceeded {budget:?}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sampler must never be empty, or its absence in a log is ambiguous
    /// between "not collected" and "collected nothing".
    #[test]
    fn sample_line_is_never_empty() {
        let line = sample_line();
        assert!(
            !line.trim().is_empty(),
            "sample_line produced nothing, so a log without vitals would be unattributable"
        );
    }

    /// Every section must be present by name even when its source is missing,
    /// so a reader can tell "the guest had 0 free PTYs" from "we never looked".
    #[test]
    fn snapshot_names_every_section_it_attempted() {
        let text = snapshot();
        for section in [
            "loadavg:",
            "meminfo:",
            "vmstat:",
            "pty:",
            "cgroup pids:",
            "space:",
        ] {
            assert!(
                text.contains(section),
                "snapshot omitted the {section:?} section entirely; a missing section must be \
                 reported as unavailable, not dropped:\n{text}"
            );
        }
    }

    /// A FUSE mount must be named and skipped, never statvfs'd. `statvfs` on a
    /// wedged FUSE mount is uninterruptible, so this is the difference between
    /// a diagnostic and a second hang.
    #[test]
    fn a_non_local_filesystem_is_skipped_rather_than_probed() {
        assert!(!is_local_fs("fuse.fuse-pipe"));
        assert!(!is_local_fs("fuse"));
        assert!(!is_local_fs("nfs"));
        assert!(is_local_fs("tmpfs"));
        assert!(is_local_fs("ext4"));
    }

    /// A path that is not a mount point must say so rather than report zero.
    #[test]
    fn an_unmounted_path_is_reported_not_silently_zero() {
        let report = space_report("/definitely-not-a-mount-point-9f3a");
        assert!(
            report.contains("not-mounted") || report.contains("skipped"),
            "expected an explicit reason, got {report:?}"
        );
    }

    /// The bounded form must return the truncation notice rather than block.
    #[test]
    fn bounded_snapshot_reports_truncation_instead_of_blocking() {
        let text = snapshot_bounded(Duration::from_nanos(1));
        assert!(
            !text.is_empty(),
            "bounded snapshot returned nothing; silence is the failure mode this replaces"
        );
    }

    /// filter_keys must not invent lines for keys that are absent.
    #[test]
    fn filter_keys_returns_only_requested_keys() {
        let raw = "MemTotal:  1024 kB\nMemFree:   512 kB\nSwapFree:  0 kB\n";
        let filtered = filter_keys(raw, &["MemTotal", "SwapFree"]);
        assert!(filtered.contains("MemTotal:"), "{filtered}");
        assert!(filtered.contains("SwapFree:"), "{filtered}");
        assert!(!filtered.contains("MemFree:"), "{filtered}");
    }
}
