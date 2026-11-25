//! The container's cgroup, frozen across a snapshot boundary, with one owner and a deadline.
//!
//! A restored clone's memory is paged in on demand, and every fault is answered in the order
//! it arrived. A container that runs on every vCPU right after the restore puts its faults
//! ahead of fc-agent's, so fc-agent's restore waits behind the workload's page-in. Measured on
//! a c8gd.metal-48xl: a 128 GiB, 32 vCPU guest whose 28 workers touch 40 GiB, 4 restores per
//! round from a copy-mode memory server, the page cache dropped before each restore and the
//! memory file read at 400 MB/s. Restores 2 to 4 acknowledged in 1.60 to 2.54 s with the
//! container frozen (3 rounds) and in 2.31 to 6.34 s with it running (3 rounds), with 0 soft
//! lockups in all 28 restores of the session.
//!
//! # Ownership
//!
//! The freeze is owned by its record, [`FROZEN_RECORD`], not by the process that froze: the
//! prepare one-shot that freezes exits long before anything thaws. The record names the
//! cgroups this boundary froze and the freeze's id. It is on `/run`, which is in guest
//! memory, so a clone restored from the snapshot holds the same record. Every change to the
//! record and to the cgroups it names happens under [`FROZEN_LOCK`] (`flock`), so a freeze, a
//! thaw and the watchdog never interleave.
//!
//! # The deadline
//!
//! The deadline is [`DEADLINE_LOOKS`] looks by the watchdog thread that find the same freeze,
//! one each [`WATCHDOG_PERIOD`]: time the guest ran, not a clock reading. A guest clock does
//! not measure that. The arm64 default profile's counter runs through a pause, and a restored
//! clone's clock can jump by the time since its snapshot, so a deadline on a clock could pass
//! while the VM was paused and thaw a clone the moment it resumes. A thread does not run while
//! its VM is paused, and a sleep that a clock jump cuts short is one look.
//!
//! # Create, use, clean up
//!
//! 1. **Create** (the prepare one-shot, before the snapshot gate closes): take the lock. Read
//!    an existing record, which a boundary that was never thawed left (a snapshot retried
//!    after its resume was lost): its cgroups are this boundary's too. Add every container
//!    cgroup that is not frozen yet; a cgroup frozen and not in the record was paused by
//!    someone else (`podman pause`) and is left alone. Write the record, with a new id, by
//!    rename, so it is whole or absent, and only then freeze its cgroups: a prepare killed at
//!    any point leaves nothing frozen that the record does not name. Release the lock and
//!    wait, up to [`FREEZE_SETTLE`], for the kernel to report the cgroups frozen.
//! 2. **Use**: the VM is paused and snapshotted.
//! 3. **Clean up**, by whichever comes first, each a thaw of the recorded cgroups:
//!    - the prepare one-shot, when its prepare fails;
//!    - the resume one-shot, when the source resumes;
//!    - a restored clone, after its own restore phases and before publication, whose hook
//!      may exec into the container;
//!    - the watchdog in the long-running agent, once the deadline has passed: a resume that
//!      never reached the guest, an fcvm that died between the prepare and the resume, a
//!      prepare killed part way, a resume that cannot open the gate. It needs nothing from
//!      the host. Its looks do not count while the freeze's boundary is being worked on
//!      (`watchdog_in`): a prepare or a resume that holds the boundary lock, a restore
//!      from its dispatch to its publication. A restore thaws the container itself, and
//!      one that never finishes ends with its VM.
//!
//!    A thaw drops from the record each cgroup it thawed or that no longer exists, and
//!    removes the record when none is left. A cgroup whose thaw failed stays recorded, and
//!    the watchdog tries it again on its next look past the deadline.
//!
//! A thaw with no record does nothing, so cleaning up twice is safe. A cgroup whose freeze
//! write failed is dropped from the record, so a pause someone makes later is not undone.
//!
//! One case is left by the order of the record and the freeze, and it is the lesser one. A
//! prepare killed after writing the record and before freezing a cgroup leaves that cgroup
//! recorded and running; if someone pauses it (`podman pause`, which does not take this
//! lock) before the cleanup, the cleanup resumes it. Freezing before recording instead
//! leaves a frozen container that nothing will ever thaw. fcvm itself never pauses a
//! container.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where podman, started by fc-agent's service, puts the container's cgroup.
const PAYLOAD_PARENT: &str = "/sys/fs/cgroup/system.slice/fc-agent.service";
const PAYLOAD_PREFIX: &str = "libpod-payload-";

/// The record of a freeze: its id and the cgroups it froze.
const FROZEN_RECORD: &str = "/run/fcvm-payload-frozen";
/// The lock every change to the record and its cgroups takes.
const FROZEN_LOCK: &str = "/run/fcvm-payload-frozen.lock";

/// How long a freeze may take to settle before the boundary goes on without it.
const FREEZE_SETTLE: Duration = Duration::from_secs(5);

/// How often the watchdog looks at the record.
const WATCHDOG_PERIOD: Duration = Duration::from_secs(1);

/// How many of the watchdog's looks may find one freeze before it thaws it: about 60 s of
/// the guest running with nobody working on the freeze's boundary. A source runs only for
/// the prepare and the resume while frozen (it is paused in between), and looks made
/// during either, or during a clone's restore, are not counted.
const DEADLINE_LOOKS: u32 = 60;

/// Where a freeze looks and records. The real one is [`Env::system`]; tests point it at a
/// fake cgroup tree.
pub(crate) struct Env<'a> {
    pub(crate) parent: &'a Path,
    pub(crate) record: &'a Path,
    pub(crate) lock: &'a Path,
    /// Called after each cgroup a freeze froze. Tests end the freeze there, as a prepare
    /// killed part way would.
    pub(crate) froze: &'a dyn Fn(&Path),
    /// Whether the boundary that made the freeze is being worked on right now
    /// (`snapshot_network::boundary_in_progress`). The watchdog asks before each look.
    pub(crate) boundary_in_progress: &'a dyn Fn() -> bool,
    /// Whether a source's boundary is owed its resume
    /// (`snapshot_network::source_boundary_is_owed`).
    pub(crate) source_owed: &'a dyn Fn() -> bool,
}

impl Env<'static> {
    fn system() -> Self {
        Env {
            parent: Path::new(PAYLOAD_PARENT),
            record: Path::new(FROZEN_RECORD),
            lock: Path::new(FROZEN_LOCK),
            froze: &|_| {},
            boundary_in_progress: &crate::snapshot_network::boundary_in_progress,
            source_owed: &crate::snapshot_network::source_boundary_is_owed,
        }
    }
}

/// The parsed record: the freeze's id and the cgroups it froze.
#[derive(Debug, PartialEq, Eq)]
struct Record {
    id: u64,
    cgroups: Vec<PathBuf>,
}

impl Record {
    fn read(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        let id = lines.next()?.strip_prefix("id ")?.parse().ok()?;
        let cgroups = lines
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();
        Some(Record { id, cgroups })
    }

    /// Write the record whole or not at all: a temporary file renamed over the path.
    fn write(&self, path: &Path) -> std::io::Result<()> {
        let mut text = format!("id {}\n", self.id);
        for cgroup in &self.cgroups {
            text.push_str(&format!("{}\n", cgroup.display()));
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

/// An id for a new freeze, different from the one it replaces, which is all the watchdog
/// needs to start counting again.
fn new_id(previous: Option<u64>) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes one timespec into the live local.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let id = (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64);
    if Some(id) == previous {
        id.wrapping_add(1)
    } else {
        id
    }
}

/// The exclusive lock on `path`, held until the returned file is dropped.
fn lock(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: flock on a descriptor this function owns; the lock ends with the descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// The container cgroups under `parent`.
fn payload_cgroups(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut cgroups: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(PAYLOAD_PREFIX)
                && entry.path().join("cgroup.freeze").exists()
        })
        .map(|entry| entry.path())
        .collect();
    cgroups.sort();
    cgroups
}

/// Whether `cgroup`'s `cgroup.freeze` says it is to be frozen.
fn freeze_requested(cgroup: &Path) -> bool {
    std::fs::read_to_string(cgroup.join("cgroup.freeze")).is_ok_and(|value| value.trim() == "1")
}

/// Whether `cgroup`'s `cgroup.events` says it is frozen.
fn is_frozen(cgroup: &Path) -> bool {
    std::fs::read_to_string(cgroup.join("cgroup.events"))
        .is_ok_and(|events| events.lines().any(|line| line.trim() == "frozen 1"))
}

/// Write `value` to `cgroup`'s `cgroup.freeze`, and say whether it was written.
fn write_freeze(cgroup: &Path, value: &str) -> bool {
    match std::fs::write(cgroup.join("cgroup.freeze"), value) {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "[fc-agent] WARNING: writing {value} to {}/cgroup.freeze failed: {error}",
                cgroup.display()
            );
            false
        }
    }
}

/// Step 1 of the module doc: record the container cgroups, then freeze them, under the
/// record's lock. Returns the cgroups the record names. Never fails: a container that is not
/// frozen is still snapshotted correctly, only without the benefit.
pub(crate) fn freeze_in(env: &Env) -> Vec<PathBuf> {
    let held = match lock(env.lock) {
        Ok(held) => held,
        Err(error) => {
            eprintln!(
                "[fc-agent] WARNING: locking {} failed, not freezing the container: {error}",
                env.lock.display()
            );
            return Vec::new();
        }
    };
    // A record still here belongs to a boundary nothing thawed: its cgroups are ours.
    let previous = Record::read(env.record);
    let previous_id = previous.as_ref().map(|record| record.id);
    // One that no longer exists (its container is gone) is dropped, as a thaw drops it: it
    // would never report frozen and hold the settle wait for its whole FREEZE_SETTLE.
    let inherited: Vec<PathBuf> = previous
        .map(|record| record.cgroups)
        .unwrap_or_default()
        .into_iter()
        .filter(|cgroup| cgroup.join("cgroup.freeze").exists())
        .collect();
    let mut ours = inherited.clone();
    for cgroup in payload_cgroups(env.parent) {
        if !ours.contains(&cgroup) && !freeze_requested(&cgroup) {
            ours.push(cgroup);
        }
    }
    if ours.is_empty() {
        let _ = std::fs::remove_file(env.record);
        return ours;
    }
    // The record first: whatever happens to this process after it, nothing is frozen that
    // the record does not name.
    let record = Record {
        id: new_id(previous_id),
        cgroups: ours.clone(),
    };
    if let Err(error) = record.write(env.record) {
        eprintln!(
            "[fc-agent] WARNING: recording the container cgroups in {} failed, not freezing \
             them: {error}",
            env.record.display()
        );
        return Vec::new();
    }
    let mut failed = Vec::new();
    for cgroup in &ours {
        if write_freeze(cgroup, "1") {
            (env.froze)(cgroup);
        } else if !inherited.contains(cgroup) {
            // An inherited cgroup was frozen by the boundary before, so it stays ours.
            failed.push(cgroup.clone());
        }
    }
    if !failed.is_empty() {
        // Not frozen by this boundary, so not its to thaw: a pause someone makes later stays.
        ours.retain(|cgroup| !failed.contains(cgroup));
        let narrowed = Record {
            id: record.id,
            cgroups: ours.clone(),
        };
        let written = if ours.is_empty() {
            std::fs::remove_file(env.record)
        } else {
            narrowed.write(env.record)
        };
        if let Err(error) = written {
            eprintln!(
                "[fc-agent] WARNING: dropping the cgroups that did not freeze from {} failed: \
                 {error}",
                env.record.display()
            );
        }
    }
    drop(held);
    let started = Instant::now();
    while !ours.iter().all(|cgroup| is_frozen(cgroup)) {
        if started.elapsed() >= FREEZE_SETTLE {
            eprintln!(
                "[fc-agent] WARNING: the container's cgroup did not report frozen within {FREEZE_SETTLE:?}"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    ours
}

/// Step 3: thaw the recorded cgroups. Each one thawed, or gone, leaves the record, and the
/// record goes when none is left. Returns the cgroups thawed. Called with the lock held.
fn thaw_recorded(env: &Env, record: Record) -> usize {
    let mut thawed = 0;
    let mut left = Vec::new();
    for cgroup in record.cgroups {
        if !cgroup.join("cgroup.freeze").exists() {
            continue;
        }
        if write_freeze(&cgroup, "0") {
            thawed += 1;
        } else {
            left.push(cgroup);
        }
    }
    if left.is_empty() {
        let _ = std::fs::remove_file(env.record);
    } else {
        let kept = Record {
            id: record.id,
            cgroups: left,
        };
        if let Err(error) = kept.write(env.record) {
            eprintln!(
                "[fc-agent] WARNING: recording the cgroups whose thaw failed in {} failed: {error}",
                env.record.display()
            );
        }
    }
    thawed
}

/// Thaw what the boundary froze, now.
pub(crate) fn thaw_in(env: &Env) -> usize {
    let Ok(_held) = lock(env.lock) else {
        return 0;
    };
    match Record::read(env.record) {
        Some(record) => thaw_recorded(env, record),
        None => 0,
    }
}

/// What the watchdog has seen: the freeze it last found and how many looks found it.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    id: Option<u64>,
    looks: u32,
    /// Looks at this freeze that were not counted because its boundary was being worked
    /// on.
    uncounted: u32,
    /// Looks at this freeze that were not counted because a source's boundary was owed
    /// and its host could still be asking for the resume.
    asked: u32,
}

/// How many looks at one freeze go uncounted while a source's boundary is owed: the
/// host's whole bound on asking for the resume (`SOURCE_REOPEN_BOUND`, 150 s, at one look
/// a second). Between two of the host's tries nobody holds the boundary lock, so those
/// looks used to count, and the thaw could land about 90 s into the 150: a container
/// running behind its closed gate with a snapshot create still in flight. Looks, not the
/// mark's age: the guest takes no look while its VM is paused for the save, and a clock
/// that ran through the pause would have used the allowance up before the host began to
/// ask. A source whose host never returns is thawed that many looks later.
pub(crate) const SOURCE_REOPEN_LOOKS: u32 = 150;

/// How many looks at one freeze may go uncounted. 15 minutes of the guest running: the
/// host ends a clone whose restore is not acknowledged by then
/// (`RESTORE_COMPLETION_ACK_TIMEOUT` in src/commands/snapshot.rs), and no prepare or
/// resume holds the boundary lock for a fraction of it. Past the cap every look counts,
/// so a one-shot that hangs while it holds the lock keeps a container frozen for
/// `UNCOUNTED_LOOKS_CAP + DEADLINE_LOOKS` looks at most, not for good.
pub(crate) const UNCOUNTED_LOOKS_CAP: u32 = 900;

/// One watchdog look: count it against the recorded freeze, and thaw that freeze once
/// [`DEADLINE_LOOKS`] looks have found it.
///
/// A look while the freeze's boundary is being worked on is not counted (#2140): a
/// prepare, a resume or a restore inside its boundary, or a restore under way at any
/// step. The deadline is for a boundary nobody is working on (a host that died between
/// the prepare and the resume, a resume that cannot open the gate), and those looks
/// count as before. At most [`UNCOUNTED_LOOKS_CAP`] looks at one freeze go uncounted. Thawed in the middle of a clone's restore, the container ran behind
/// the closed gate, where each connection it made was answered with a reset.
pub(crate) fn watchdog_in(env: &Env, watch: &mut Watch) -> usize {
    let Ok(_held) = lock(env.lock) else {
        return 0;
    };
    let Some(record) = Record::read(env.record) else {
        *watch = Watch::default();
        return 0;
    };
    if watch.id != Some(record.id) {
        *watch = Watch {
            id: Some(record.id),
            looks: 0,
            uncounted: 0,
            asked: 0,
        };
    }
    if (env.boundary_in_progress)() && watch.uncounted < UNCOUNTED_LOOKS_CAP {
        watch.uncounted += 1;
        return 0;
    }
    if (env.source_owed)() && watch.asked < SOURCE_REOPEN_LOOKS {
        watch.asked += 1;
        return 0;
    }
    watch.looks = watch.looks.saturating_add(1);
    if watch.looks < DEADLINE_LOOKS {
        return 0;
    }
    thaw_recorded(env, record)
}

/// Freeze the container before the snapshot boundary closes.
///
/// Not when the guest's GPUs are parked for a warm GPU snapshot: a clone unparks them during
/// its restore, before publication, by restoring the checkpointed CUDA processes and running
/// the container's GPU hook, which must find the container running.
pub fn freeze() {
    if Path::new(crate::gpu_park::PARKED_RECORD).exists() {
        eprintln!("[fc-agent] the GPUs are parked; the container is not frozen for this snapshot");
        return;
    }
    let started = Instant::now();
    let frozen = freeze_in(&Env::system());
    if !frozen.is_empty() {
        eprintln!(
            "[fc-agent] froze {} container cgroup(s) for the snapshot boundary in {} ms",
            frozen.len(),
            started.elapsed().as_millis()
        );
    }
}

/// Thaw the container: on a failed prepare, on the source's resume, on a clone before
/// publication.
pub fn thaw() {
    let n = thaw_in(&Env::system());
    if n > 0 {
        eprintln!("[fc-agent] thawed {n} container cgroup(s)");
    }
}

/// The long-running agent's watchdog: thaws a freeze that has outlived its deadline, whatever
/// happened to the host. A thread of its own, so nothing in the agent's runtime delays it.
pub fn spawn_watchdog() {
    let spawned = std::thread::Builder::new()
        .name("freeze-watchdog".to_string())
        .spawn(|| {
            let mut watch = Watch::default();
            loop {
                let n = watchdog_in(&Env::system(), &mut watch);
                if n > 0 {
                    eprintln!(
                        "[fc-agent] WARNING: thawed {n} container cgroup(s) whose snapshot \
                         boundary was found frozen {DEADLINE_LOOKS} times without a thaw"
                    );
                }
                std::thread::sleep(WATCHDOG_PERIOD);
            }
        });
    if let Err(error) = spawned {
        eprintln!("[fc-agent] WARNING: the freeze watchdog did not start: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake cgroup tree whose `cgroup.events` always reports frozen, and the watchdog's
    /// state.
    struct Tree {
        dir: tempfile::TempDir,
        watch: RefCell<Watch>,
        /// What the watchdog is told when it asks whether the boundary is being worked on.
        in_progress: std::cell::Cell<bool>,
        owed: std::cell::Cell<bool>,
    }

    impl Tree {
        fn new() -> Self {
            Tree {
                dir: tempfile::tempdir().unwrap(),
                watch: RefCell::new(Watch::default()),
                in_progress: std::cell::Cell::new(false),
                owed: std::cell::Cell::new(false),
            }
        }

        fn cgroup(&self, name: &str, frozen: bool) -> PathBuf {
            let cgroup = self.dir.path().join("cg").join(name);
            std::fs::create_dir_all(&cgroup).unwrap();
            std::fs::write(cgroup.join("cgroup.freeze"), if frozen { "1" } else { "0" }).unwrap();
            // Reported frozen from the start, so a freeze does not wait out FREEZE_SETTLE.
            std::fs::write(cgroup.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
            cgroup
        }

        fn run_with<T>(&self, froze: &dyn Fn(&Path), f: impl FnOnce(&Env) -> T) -> T {
            let parent = self.dir.path().join("cg");
            let record = self.dir.path().join("frozen");
            let lock = self.dir.path().join("frozen.lock");
            f(&Env {
                parent: &parent,
                record: &record,
                lock: &lock,
                froze,
                boundary_in_progress: &|| self.in_progress.get(),
                source_owed: &|| self.owed.get(),
            })
        }

        fn run<T>(&self, f: impl FnOnce(&Env) -> T) -> T {
            self.run_with(&|_| {}, f)
        }

        /// `n` looks by the watchdog; the cgroups they thawed.
        fn watchdog(&self, n: u32) -> usize {
            (0..n)
                .map(|_| self.run(|env| watchdog_in(env, &mut self.watch.borrow_mut())))
                .sum()
        }

        fn record_exists(&self) -> bool {
            self.dir.path().join("frozen").exists()
        }
    }

    fn frozen(cgroup: &Path) -> bool {
        freeze_requested(cgroup)
    }

    /// The exit-path matrix. Each case freezes a running container and a container someone
    /// paused, takes one way out of the snapshot boundary, and requires the running one to
    /// run again, the paused one to stay paused, and no record to be left.
    fn after(tree: &Tree, running: &Path, paused: &Path) {
        assert!(!frozen(running), "the container is left frozen");
        assert!(
            frozen(paused),
            "the boundary resumed a container someone had paused"
        );
        assert!(!tree.record_exists(), "a record outlived the boundary");
    }

    #[test]
    fn success_the_source_resume_thaws() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let paused = tree.cgroup("libpod-payload-p", true);
        assert_eq!(tree.run(freeze_in), vec![running.clone()]);
        assert!(frozen(&running));
        assert_eq!(tree.run(thaw_in), 1);
        after(&tree, &running, &paused);
        // A late watchdog finds nothing to do.
        assert_eq!(tree.watchdog(2 * DEADLINE_LOOKS), 0);
    }

    #[test]
    fn prepare_failure_thaws_at_once() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let paused = tree.cgroup("libpod-payload-p", true);
        tree.run(freeze_in);
        // The prepare failed: the one-shot thaws before it reports the failure.
        assert_eq!(tree.run(thaw_in), 1);
        after(&tree, &running, &paused);
    }

    /// The host resumed the VM but its resume one-shot never ran in the guest. Nothing
    /// thaws the container but the watchdog, which waits for the deadline.
    #[test]
    fn resume_lost_the_watchdog_thaws_at_the_deadline() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let paused = tree.cgroup("libpod-payload-p", true);
        tree.run(freeze_in);
        assert_eq!(
            tree.watchdog(DEADLINE_LOOKS - 1),
            0,
            "the watchdog thawed before the deadline"
        );
        assert!(frozen(&running));
        assert_eq!(tree.watchdog(1), 1);
        after(&tree, &running, &paused);
    }

    /// The resume was lost and the snapshot is retried before the deadline: the second
    /// freeze finds the cgroup frozen, and it is still the boundary's, so the retry's
    /// resume thaws it. The retry starts the deadline again.
    #[test]
    fn retry_keeps_the_first_freeze_owned() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let paused = tree.cgroup("libpod-payload-p", true);
        tree.run(freeze_in);
        assert_eq!(tree.watchdog(DEADLINE_LOOKS - 1), 0);
        assert_eq!(tree.run(freeze_in), vec![running.clone()]);
        assert_eq!(tree.watchdog(DEADLINE_LOOKS - 1), 0);
        assert_eq!(tree.run(thaw_in), 1);
        after(&tree, &running, &paused);
    }

    /// The watchdog does not thaw a freeze whose boundary is being worked on (#2140): a
    /// clone's restore that takes over a minute before its own thaw, a prepare that takes
    /// that long. Such looks are not counted. Looks count again once nobody is working on
    /// the boundary, on top of the ones counted before, so a boundary that is abandoned
    /// afterwards is still thawed at [`DEADLINE_LOOKS`] counted looks.
    ///
    /// The looks that go uncounted are capped, so a one-shot that hangs while it holds
    /// the boundary lock cannot keep the container frozen for good.
    ///
    /// RED BEFORE THE FIX: the freeze was thawed at the 60th look of a restore.
    /// RED WITHOUT THE CAP: a boundary that stays in progress is never thawed.
    #[test]
    fn the_watchdog_leaves_a_freeze_alone_while_its_boundary_is_in_progress() {
        // A source whose boundary is owed, with nobody holding the lock: between two of
        // the host's tries at the resume. The host asks for SOURCE_REOPEN_LOOKS seconds;
        // those looks are not counted, and the deadline runs from there, so a source whose
        // host never returns is still thawed.
        //
        // RED BEFORE THE FIX: the thaw came at DEADLINE_LOOKS, with the host still asking
        // and a snapshot create still in flight.
        let asked = Tree::new();
        let source = asked.cgroup("libpod-payload-a", false);
        asked.run(freeze_in);
        asked.owed.set(true);
        assert_eq!(asked.watchdog(SOURCE_REOPEN_LOOKS + DEADLINE_LOOKS - 1), 0);
        assert!(
            frozen(&source),
            "thawed while its host could still be asking"
        );
        assert_eq!(asked.watchdog(1), 1);
        assert!(!frozen(&source));
        // one look a second, for the host's whole bound
        let host = include_str!("../../src/commands/common.rs");
        assert!(host.contains(&format!(
            "const SOURCE_REOPEN_BOUND: std::time::Duration = std::time::Duration::from_secs({SOURCE_REOPEN_LOOKS});"
        )));

        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        tree.run(freeze_in);
        // a few looks before the restore's epoch is dispatched
        assert_eq!(tree.watchdog(5), 0);
        tree.in_progress.set(true);
        assert_eq!(tree.watchdog(3 * DEADLINE_LOOKS), 0);
        assert!(frozen(&running), "thawed in the middle of its boundary");
        assert!(tree.record_exists());
        // Abandoned: nobody is working on it any more. The looks made while it was in
        // progress were not counted, so the deadline is what was left of it before.
        tree.in_progress.set(false);
        assert_eq!(tree.watchdog(DEADLINE_LOOKS - 6), 0);
        assert!(frozen(&running));
        assert_eq!(tree.watchdog(1), 1);
        assert!(
            !frozen(&running),
            "an abandoned boundary's freeze was never thawed"
        );
        assert!(!tree.record_exists());

        // A boundary that stays in progress: uncounted up to the cap, then counted.
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        tree.run(freeze_in);
        tree.in_progress.set(true);
        assert_eq!(tree.watchdog(UNCOUNTED_LOOKS_CAP + DEADLINE_LOOKS - 1), 0);
        assert!(frozen(&running));
        assert_eq!(tree.watchdog(1), 1);
        assert!(
            !frozen(&running),
            "a boundary hung in progress kept its freeze for good"
        );
        // The cap is the host's bound on a restore's acknowledgement, one look a second.
        let host = include_str!("../../src/commands/snapshot.rs");
        assert!(host.contains(
            "const RESTORE_COMPLETION_ACK_TIMEOUT: Duration = Duration::from_secs(15 * 60);"
        ));
        assert_eq!(
            u64::from(UNCOUNTED_LOOKS_CAP) * WATCHDOG_PERIOD.as_secs(),
            15 * 60
        );
    }

    /// fcvm was SIGKILLed between the prepare and the resume, and the snapshot it took is
    /// restored later. The source (if anything resumes it) and the clone each hold the
    /// record, and each one's watchdog thaws its container at the deadline, with no host.
    #[test]
    fn fcvm_sigkill_the_watchdog_thaws_without_the_host() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let paused = tree.cgroup("libpod-payload-p", true);
        tree.run(freeze_in);
        // A clone whose restore never reached its thaw.
        assert_eq!(tree.watchdog(DEADLINE_LOOKS), 1);
        after(&tree, &running, &paused);
    }

    /// The prepare one-shot is killed (its exec session ended) right after it froze a
    /// cgroup. The record already names it, so the watchdog thaws it.
    #[test]
    fn prepare_killed_mid_freeze_the_watchdog_thaws() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let paused = tree.cgroup("libpod-payload-p", true);
        let killed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tree.run_with(&|_| panic!("the prepare is killed"), freeze_in)
        }));
        assert!(killed.is_err());
        assert!(frozen(&running));
        assert_eq!(tree.watchdog(DEADLINE_LOOKS), 1);
        after(&tree, &running, &paused);
    }

    /// A thaw that cannot write a cgroup's `cgroup.freeze` keeps that cgroup recorded, and
    /// the watchdog thaws it once the write works.
    #[test]
    fn a_failed_thaw_stays_recorded_and_the_watchdog_retries_it() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let paused = tree.cgroup("libpod-payload-p", true);
        tree.run(freeze_in);
        // A directory in place of the file: the write fails, for root too.
        let freeze = running.join("cgroup.freeze");
        std::fs::remove_file(&freeze).unwrap();
        std::fs::create_dir(&freeze).unwrap();
        assert_eq!(tree.run(thaw_in), 0);
        assert!(tree.record_exists(), "a failed thaw dropped the record");
        std::fs::remove_dir(&freeze).unwrap();
        std::fs::write(&freeze, "1").unwrap();
        assert_eq!(tree.watchdog(DEADLINE_LOOKS), 1);
        after(&tree, &running, &paused);
    }

    /// A cgroup whose freeze write failed is not the boundary's: when someone pauses it
    /// afterwards, the boundary's thaw leaves it paused.
    #[test]
    fn a_failed_freeze_is_not_recorded() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let stuck = tree.cgroup("libpod-payload-b", false);
        let freeze = stuck.join("cgroup.freeze");
        std::fs::remove_file(&freeze).unwrap();
        std::fs::create_dir(&freeze).unwrap();
        // The directory still reads as not frozen, so the freeze selects it, and its write fails.
        tree.run(freeze_in);
        // Someone pauses it.
        std::fs::remove_dir(&freeze).unwrap();
        std::fs::write(&freeze, "1").unwrap();
        assert_eq!(tree.run(thaw_in), 1);
        after(&tree, &running, &stuck);
    }

    /// A retried snapshot whose freeze write fails on a cgroup the first freeze froze keeps
    /// it recorded, so the retry's resume thaws it once the write works.
    #[test]
    fn a_retry_keeps_an_inherited_cgroup_whose_write_failed() {
        let tree = Tree::new();
        let running = tree.cgroup("libpod-payload-a", false);
        let paused = tree.cgroup("libpod-payload-p", true);
        tree.run(freeze_in);
        let freeze = running.join("cgroup.freeze");
        std::fs::remove_file(&freeze).unwrap();
        std::fs::create_dir(&freeze).unwrap();
        tree.run(freeze_in);
        std::fs::remove_dir(&freeze).unwrap();
        std::fs::write(&freeze, "1").unwrap();
        assert_eq!(tree.run(thaw_in), 1);
        after(&tree, &running, &paused);
    }

    /// A retry after the container was recreated under another cgroup: the old one is gone,
    /// so it leaves the record and does not hold the freeze for FREEZE_SETTLE.
    #[test]
    fn a_retry_drops_an_inherited_cgroup_that_is_gone() {
        let tree = Tree::new();
        let old = tree.cgroup("libpod-payload-a", false);
        tree.run(freeze_in);
        std::fs::remove_dir_all(&old).unwrap();
        let new = tree.cgroup("libpod-payload-b", false);
        let started = Instant::now();
        assert_eq!(tree.run(freeze_in), vec![new.clone()]);
        assert!(
            started.elapsed() < FREEZE_SETTLE,
            "the freeze waited {:?} for a cgroup that is gone",
            started.elapsed()
        );
        assert_eq!(tree.run(thaw_in), 1);
        assert!(!frozen(&new));
        assert!(!tree.record_exists());
    }

    #[test]
    fn no_container_is_nothing_to_freeze_and_records_nothing() {
        let tree = Tree::new();
        std::fs::create_dir_all(tree.dir.path().join("cg")).unwrap();
        assert!(tree.run(freeze_in).is_empty());
        assert!(!tree.record_exists());
        assert_eq!(tree.run(thaw_in), 0);
        assert_eq!(tree.watchdog(DEADLINE_LOOKS), 0);
    }

    #[test]
    fn the_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frozen");
        let record = Record {
            id: 42,
            cgroups: vec![PathBuf::from("/a"), PathBuf::from("/b c")],
        };
        record.write(&path).unwrap();
        assert_eq!(Record::read(&path), Some(record));
    }
}
