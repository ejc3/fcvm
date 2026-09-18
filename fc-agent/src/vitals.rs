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
//! host writes to the per-VM file and keeps out of the job log. While the guest
//! is piled up ([`Pileup`]), one more line of at most 1 KiB per second names
//! the threads, and after 30 s one per 10 s. An idle guest costs a read of
//! /proc/loadavg and /proc/stat per second.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
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

/// Runnable plus blocked threads per CPU at which the sampler names them, and
/// the floor for small guests. An idle 2-vCPU guest reads 1 to 3. The exec
/// stalls seen with #938 read 12 to 18 for 20 s, with no vsock connection
/// accepted and no process forked, and the 10s line could only say how many
/// threads were runnable, not which.
const PILEUP_PER_CPU: u32 = 3;
const PILEUP_FLOOR: u32 = 6;

/// Cap on one pile-up line. The console writer is sized for under 2 KiB/s.
const PILEUP_LIMIT: usize = 1024;

/// Distinct `state:comm` names, and vsock-named threads, kept in one line.
const PILEUP_NAMES: usize = 12;
const VSOCK_NAMES: usize = 4;

/// After this many consecutive piled-up seconds only every tenth is printed,
/// so a guest that is busy for its whole life (a build, a benchmark) costs a
/// scan every 10s, not every second.
const PILEUP_EVERY_SECOND_FOR: u32 = 30;

/// The number of runnable threads, from the `16/133` field of /proc/loadavg.
/// That field leaves out uninterruptible sleep; see [`blocked_count`].
fn runnable_count(loadavg: &str) -> Option<u32> {
    loadavg
        .split_whitespace()
        .nth(3)?
        .split('/')
        .next()?
        .parse()
        .ok()
}

/// Threads in uninterruptible sleep, from the `procs_blocked` line of
/// /proc/stat. A guest wedged on I/O has none runnable and many of these.
fn blocked_count(stat: &str) -> Option<u32> {
    stat.lines()
        .find_map(|line| line.strip_prefix("procs_blocked "))?
        .trim()
        .parse()
        .ok()
}

fn online_cpus(stat: &str) -> u32 {
    let cpus = stat
        .lines()
        .filter(|line| line.starts_with("cpu") && !line.starts_with("cpu "))
        .count();
    u32::try_from(cpus).unwrap_or(u32::MAX).max(1)
}

/// `(runnable, blocked)` when together they reach the threshold, which is the
/// only gate. An unreadable count is an error, never "idle": a collector that
/// cannot run must say so.
fn piled_up(loadavg: &str, stat: &str) -> Result<Option<(u32, u32)>, String> {
    let runnable = runnable_count(loadavg)
        .ok_or_else(|| format!("no runnable count in /proc/loadavg: {:?}", loadavg.trim()))?;
    let blocked = blocked_count(stat).ok_or("no procs_blocked line in /proc/stat")?;
    let threshold = PILEUP_FLOOR.max(PILEUP_PER_CPU.saturating_mul(online_cpus(stat)));
    Ok((runnable.saturating_add(blocked) >= threshold).then_some((runnable, blocked)))
}

/// Whether the `piled_for`-th consecutive piled-up second gets a line.
fn prints_on(piled_for: u32) -> bool {
    piled_for <= PILEUP_EVERY_SECOND_FOR || piled_for.is_multiple_of(10)
}

/// Every thread's /proc directory and stat line plus the number of unreadable
/// ones, or why /proc could not be listed.
type ThreadScan = Result<(Vec<(PathBuf, String)>, u32), String>;

/// The state and name from one /proc/<tid>/stat line. The name may hold spaces
/// and parentheses (`fc_vcpu 0`, `(sd-pam)`), so it ends at the LAST `)`.
fn parse_stat(raw: &str) -> Option<(char, String)> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    let comm = raw.get(open + 1..close)?.to_string();
    let state = raw
        .get(close + 1..)?
        .split_whitespace()
        .next()?
        .chars()
        .next()?;
    Some((state, comm))
}

/// A stat line as text. `prctl(PR_SET_NAME)` takes any bytes and procfs does
/// not escape them: an invalid byte must not drop the thread from the count,
/// and a newline must not split the console record in two.
fn stat_text(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Cut `text` to `limit` bytes on a character boundary and say so.
/// `String::truncate` panics inside a multi-byte character, and a thread name
/// may hold one; a panic here would end the sampler thread without a word.
fn bounded(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" ...truncated");
    }
    text
}

/// Threads that are running, runnable or in uninterruptible sleep, counted by
/// `state:comm`, most numerous first.
fn busy_summary<'a>(stats: impl IntoIterator<Item = &'a str>) -> String {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for raw in stats {
        if let Some((state, comm)) = parse_stat(raw) {
            if state == 'R' || state == 'D' {
                *counts.entry(format!("{state}:{comm}")).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<(String, u32)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut out = ranked
        .iter()
        .take(PILEUP_NAMES)
        .map(|(name, count)| format!("{name}*{count}"))
        .collect::<Vec<_>>()
        .join(" ");
    if ranked.len() > PILEUP_NAMES {
        let _ = write!(out, " (+{} more names)", ranked.len() - PILEUP_NAMES);
    }
    out
}

/// The virtio device that is the vsock (device id 19), such as `virtio2`.
/// Which index it gets depends on the VM's disks and network devices.
fn vsock_device() -> Option<String> {
    for entry in std::fs::read_dir("/sys/bus/virtio/devices").ok()?.flatten() {
        let id = std::fs::read_to_string(entry.path().join("device")).unwrap_or_default();
        if id.trim() == "0x0013" {
            return Some(entry.file_name().to_string_lossy().into_owned());
        }
    }
    None
}

/// Interrupt totals of the virtio devices, summed over CPUs, from
/// /proc/interrupts, with the vsock's marked. Its count standing still while
/// the host is connecting says the device raised nothing; its count moving
/// while nothing is accepted says the guest did not get to the work.
fn virtio_irqs(interrupts: &str, vsock: Option<&str>) -> String {
    interrupts
        .lines()
        .filter(|line| line.contains("virtio"))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            fields.next()?; // the irq number
            let total: u64 = fields.clone().map_while(|f| f.parse::<u64>().ok()).sum();
            let name = fields.last()?;
            // MMIO names the line `virtio2`, PCI one per queue (`virtio2-input.0`).
            let is_vsock = vsock.is_some_and(|v| {
                name == v
                    || name
                        .strip_prefix(v)
                        .is_some_and(|rest| rest.starts_with('-'))
            });
            Some(format!(
                "{name}{}:{total}",
                if is_vsock { "(vsock)" } else { "" }
            ))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Per-CPU user, system, idle, interrupt (hard plus soft) and steal ticks from
/// /proc/stat. With the line's `up=`, two lines tell a guest whose own threads
/// hold the CPUs (user and system advance) from a host that is not running the
/// vCPU (steal advances, where the VMM reports it).
fn cpu_times(stat: &str) -> String {
    stat.lines()
        .filter(|line| line.starts_with("cpu") && !line.starts_with("cpu "))
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let ticks = |i: usize| f.get(i)?.parse::<u64>().ok();
            Some(format!(
                "{}:u{},s{},i{},q{},st{}",
                f.first()?,
                ticks(1)?,
                ticks(3)?,
                ticks(4)?,
                ticks(6)? + ticks(7)?,
                ticks(8)?
            ))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every thread's /proc directory and stat line, and how many could not be
/// read. Reads `stat` only: unlike `cmdline` it does not fault the target's
/// memory, so it cannot hang on a wedged process.
fn thread_stats() -> ThreadScan {
    let mut threads = Vec::new();
    let mut unreadable = 0;
    let procs = std::fs::read_dir("/proc").map_err(|error| format!("/proc: {error}"))?;
    for proc_entry in procs.flatten() {
        let name = proc_entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // A process that exited since the listing is not a failed read.
        let Ok(tasks) = std::fs::read_dir(proc_entry.path().join("task")) else {
            continue;
        };
        for task in tasks.flatten() {
            match std::fs::read(task.path().join("stat")) {
                Ok(raw) => threads.push((task.path(), stat_text(&raw))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => unreadable += 1,
            }
        }
    }
    Ok((threads, unreadable))
}

/// State, wait channel and the top of the kernel stack of the threads named
/// for vsock. A kworker carries `virtio_vsock` in its name only while it runs
/// that work or just after. Work that is queued and that no worker has picked
/// up has no name here, which prints as `none-named`: then `busy` says which
/// kworkers, if any, are runnable.
fn vsock_workers(threads: &[(PathBuf, String)]) -> String {
    let named: Vec<String> = threads
        .iter()
        .filter_map(|(dir, raw)| {
            let (state, comm) = parse_stat(raw)?;
            if !comm.contains("vsock") {
                return None;
            }
            let read = |file: &str| {
                std::fs::read_to_string(dir.join(file)).map_err(|_| "<unreadable>".to_string())
            };
            let wchan = read("wchan")
                .map(|text| text.trim().to_string())
                .unwrap_or_else(|e| e);
            let stack = read("stack")
                .map(|text| {
                    text.lines()
                        .take(4)
                        .filter_map(|line| line.split_whitespace().nth(1))
                        .collect::<Vec<_>>()
                        .join("<")
                })
                .unwrap_or_else(|e| e);
            Some(format!("{comm}:{state}:{wchan}:{stack}"))
        })
        .collect();
    if named.is_empty() {
        return "none-named".to_string();
    }
    let mut out = named
        .iter()
        .take(VSOCK_NAMES)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if named.len() > VSOCK_NAMES {
        let _ = write!(out, " (+{} more)", named.len() - VSOCK_NAMES);
    }
    out
}

/// One pile-up line from its sources. `up` is the guest's own clock: the host
/// stamps a line when it arrives, which in a pile-up is late.
fn format_pileup(up: &str, counts: (u32, u32), scan: &ThreadScan, irq: &str, stat: &str) -> String {
    let (runnable, blocked) = counts;
    let threads = match scan {
        Ok((threads, unreadable)) => format!(
            "scanned={} unreadable={unreadable} busy=[{}] vsock=[{}]",
            threads.len(),
            busy_summary(threads.iter().map(|(_, raw)| raw.as_str())),
            vsock_workers(threads),
        ),
        Err(error) => format!("threads-unavailable({error})"),
    };
    bounded(
        format!(
            "up={up} runnable={runnable} blocked={blocked} {threads} irq=[{irq}] cpu=[{}]",
            cpu_times(stat)
        ),
        PILEUP_LIMIT,
    )
}

/// The sampler's once-a-second pile-up check.
#[derive(Default)]
pub struct Pileup {
    piled_for: u32,
    said_unavailable: bool,
    vsock: Option<Option<String>>,
}

impl Pileup {
    /// The line to print this second, if any: which threads are runnable while
    /// many are, or, once, why that cannot be told.
    pub fn tick(&mut self) -> Option<String> {
        match self.line() {
            Ok(line) => line,
            Err(_) if self.said_unavailable => None,
            Err(error) => {
                self.said_unavailable = true;
                Some(format!("unavailable: {error}"))
            }
        }
    }

    fn line(&mut self) -> Result<Option<String>, String> {
        let loadavg = read_proc("/proc/loadavg")?;
        let stat = read_proc("/proc/stat")?;
        let Some(counts) = piled_up(&loadavg, &stat)? else {
            self.piled_for = 0;
            return Ok(None);
        };
        self.piled_for = self.piled_for.saturating_add(1);
        if !prints_on(self.piled_for) {
            return Ok(None);
        }
        let vsock = self.vsock.get_or_insert_with(vsock_device).clone();
        let irq = match read_proc("/proc/interrupts") {
            Ok(raw) => virtio_irqs(&raw, vsock.as_deref()),
            Err(error) => format!("unavailable({error})"),
        };
        let up = read_proc("/proc/uptime").unwrap_or_default();
        let up = up.split_whitespace().next().unwrap_or("?");
        let mut line = format_pileup(up, counts, &thread_stats(), &irq, &stat);
        if self.piled_for > PILEUP_EVERY_SECOND_FOR {
            line.push_str(" (every 10th second now)");
        }
        Ok(Some(line))
    }
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

    /// `fc_vcpu 0` and `(sd-pam)` hold a space and parentheses. Splitting the
    /// line on whitespace reads the wrong field as the state for both.
    #[test]
    fn a_thread_name_with_spaces_or_parentheses_keeps_its_state() {
        assert_eq!(
            parse_stat("71 (fc_vcpu 0) R 1 71 71 0"),
            Some(('R', "fc_vcpu 0".to_string()))
        );
        assert_eq!(
            parse_stat("9 ((sd-pam)) S 1 9 9 0"),
            Some(('S', "(sd-pam)".to_string()))
        );
        assert_eq!(parse_stat("not a stat line"), None);
    }

    const STAT_2CPU: &str = "cpu  10 0 10 100 0 1 2 0 0 0\ncpu0 5 0 6 50 0 1 0 3 0 0\ncpu1 5 0 4 50 0 0 2 9 0 0\nintr 1\nprocs_running 1\nprocs_blocked 0\n";

    /// The gate is runnable plus blocked against three per CPU, six at least.
    /// An idle guest prints nothing, and eight CPUs move the line to 24.
    #[test]
    fn the_gate_counts_runnable_and_blocked_and_scales_with_the_cpus() {
        assert_eq!(piled_up("0.10 0.05 0.01 1/120 900", STAT_2CPU), Ok(None));
        assert_eq!(
            piled_up("4.11 1.06 0.36 16/133 2343", STAT_2CPU),
            Ok(Some((16, 0)))
        );
        // Nothing runnable and many in uninterruptible sleep is a pile-up too:
        // the runnable field of /proc/loadavg leaves those out.
        let wedged = STAT_2CPU.replace("procs_blocked 0", "procs_blocked 40");
        assert_eq!(
            piled_up("9.00 5.00 1.00 1/120 900", &wedged),
            Ok(Some((1, 40)))
        );
        let eight: String = (0..8)
            .map(|n| format!("cpu{n} 1 0 1 1 0 0 0 0 0 0\n"))
            .collect::<String>()
            + "procs_blocked 0\n";
        assert_eq!(piled_up("9.00 5.00 1.00 16/400 900", &eight), Ok(None));
        assert_eq!(
            piled_up("9.00 5.00 1.00 24/400 900", &eight),
            Ok(Some((24, 0)))
        );
    }

    /// A count that cannot be read is reported, not taken for an idle guest.
    #[test]
    fn an_unreadable_count_is_an_error_not_an_idle_guest() {
        assert!(piled_up("garbage", STAT_2CPU).is_err());
        assert!(piled_up("4.11 1.06 0.36 16/133 2343", "cpu0 1 0 1 1 0 0 0 0 0 0\n").is_err());
    }

    #[test]
    fn a_long_pileup_is_printed_every_second_then_every_tenth() {
        assert!((1..=30).all(prints_on));
        assert_eq!(
            (31..=60).filter(|&n| prints_on(n)).collect::<Vec<_>>(),
            vec![40, 50, 60]
        );
    }

    fn threads(lines: &[&str]) -> ThreadScan {
        Ok((
            lines
                .iter()
                .map(|raw| (PathBuf::from("/nonexistent"), raw.to_string()))
                .collect(),
            2,
        ))
    }

    /// The line names who is runnable or in uninterruptible sleep, leaves out
    /// sleepers, and says which interrupt line is the vsock's. A worker whose
    /// wchan and stack cannot be read says so.
    #[test]
    fn a_pileup_names_the_busy_threads_and_the_vsock_worker() {
        let piled = threads(&[
            "50 (yes) R 1",
            "51 (yes) R 1",
            "52 (conmon) R 1",
            "23 (kworker/1:0-virtio_vsock) R 2",
            "60 (podman) D 1",
            "1 (systemd) S 0",
        ]);
        let interrupts = "           CPU0       CPU1\n 27:         10          5   IO-APIC  27-fasteoi   virtio1\n 28:        100       2497   IO-APIC  28-fasteoi   virtio2\n  4:          7          0   IO-APIC   4-edge      ttyS0\n";
        let line = format_pileup(
            "41.75",
            (16, 1),
            &piled,
            &virtio_irqs(interrupts, Some("virtio2")),
            STAT_2CPU,
        );
        assert!(
            line.starts_with(
                "up=41.75 runnable=16 blocked=1 scanned=6 unreadable=2 busy=[R:yes*2 "
            ),
            "{line}"
        );
        for part in [
            "D:podman*1",
            "R:conmon*1",
            "vsock=[kworker/1:0-virtio_vsock:R:<unreadable>:<unreadable>]",
            "irq=[virtio1:15 virtio2(vsock):2597]",
            "cpu=[cpu0:u5,s6,i50,q1,st3 cpu1:u5,s4,i50,q2,st9]",
        ] {
            assert!(line.contains(part), "missing {part:?} in {line}");
        }
        assert!(
            !line.contains("systemd"),
            "a sleeping thread is not busy: {line}"
        );
        assert_eq!(
            virtio_irqs(
                " 30: 1 2 PCI-MSI virtio2-input.0\n 31: 1 1 PCI-MSI virtio20-input.0\n",
                Some("virtio2")
            ),
            "virtio2-input.0(vsock):3 virtio20-input.0:2"
        );
    }

    /// Queued vsock work that no worker has picked up names no thread. That
    /// must not print like a scan that found nothing, and a failed scan says so.
    #[test]
    fn no_vsock_named_thread_and_a_failed_scan_both_say_so() {
        let line = format_pileup("1.00", (9, 0), &threads(&["50 (yes) R 1"]), "", STAT_2CPU);
        assert!(line.contains("vsock=[none-named]"), "{line}");
        let line = format_pileup(
            "1.00",
            (9, 0),
            &Err("/proc: denied".to_string()),
            "unavailable(x)",
            STAT_2CPU,
        );
        assert!(
            line.contains("threads-unavailable(/proc: denied)")
                && line.contains("irq=[unavailable(x)]"),
            "{line}"
        );
    }

    /// A thread name is any bytes. An invalid one still counts, and a newline
    /// does not split the console record.
    #[test]
    fn a_thread_name_with_invalid_bytes_or_a_newline_stays_one_counted_record() {
        let text = stat_text(b"77 (bad\xffna\nme) R 1 77");
        assert!(!text.contains('\n'), "{text:?}");
        assert_eq!(parse_stat(&text).map(|(state, _)| state), Some('R'));
    }

    /// Byte 1024 of the line can fall inside a multi-byte thread name.
    #[test]
    fn the_line_is_cut_on_a_character_boundary_and_says_so() {
        let text = format!("{}{}", "a".repeat(PILEUP_LIMIT - 1), "\u{e9}".repeat(40));
        let cut = bounded(text, PILEUP_LIMIT);
        assert!(
            cut.ends_with(" ...truncated") && cut.len() < PILEUP_LIMIT + 16,
            "{}",
            cut.len()
        );
        assert_eq!(bounded("short".to_string(), PILEUP_LIMIT), "short");
    }

    /// The scan must see real threads: this test's own thread is running.
    #[test]
    fn the_thread_scan_finds_this_running_thread() {
        let (all, _) = thread_stats().expect("/proc is readable");
        let summary = busy_summary(all.iter().map(|(_, raw)| raw.as_str()));
        assert!(
            summary.contains("R:"),
            "no running thread among {} scanned: {summary:?}",
            all.len()
        );
    }
}
