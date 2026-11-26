//! A sweep runs once an interval, not once a start.
//!
//! fcvm keeps four tables that a killed fcvm leaves an entry in: the VM states
//! (`crate::state::manager`), the owner records (`crate::reclaim`), the per-VM cgroup
//! scopes under their parent (`crate::cgroup`) and the routed IPv6 address leases
//! (`crate::network::routed`). Removing what the dead left means visiting every entry of
//! the table, so its cost grows with the VMs alive. Each of the four once did that in every
//! start, under a lock every other start needed, and each bound the start rate (#2077,
//! #2126, #2178, #2202). This module is the one mechanism they share: when a start sweeps,
//! and what it may hold while it does.
//!
//! # When: a turn
//!
//! A sweep has a [`Place`]: a directory with a stamp and a lock in it. The stamp says when
//! the last sweep began, as `<boot id> <CLOCK_BOOTTIME ns>`: the kernel's count since boot,
//! which no clock change moves, and the boot whose count it is. A start asks for a turn
//! ([`turn_if_due`]) and gets one only when no sweep began within the interval and no other
//! process is sweeping. A start that gets none has opened and read one file by name,
//! whatever the size of the table.
//!
//! | case | what happens |
//! |------|---------------|
//! | a sweep began less than the interval ago, in this boot | no turn |
//! | the stamp is another boot's, is older than the interval, does not parse, or names a time after now | a sweep is due |
//! | another process holds the lock | no turn: it is sweeping, or has just stamped |
//! | two starts find a sweep due together | one gets the lock; the other finds it held, or gets it after and finds the stamp new |
//! | a sweeper dies | the kernel releases its lock. It stamped before it swept, so what it left waits one more interval |
//! | the interval is zero | every start has a turn, with no lock and no stamp |
//!
//! So an entry whose owner died is removed by the first start that begins an interval or
//! more after the last sweep began, or two intervals after when that sweep's process died
//! before it reached the entry. A stamp kept on a disk survives a reboot: it is another
//! boot's then, and the first start of the boot sweeps.
//!
//! # Holding the table: takers shared, the sweeper exclusive
//!
//! Two of the tables are directories whose entries are made in two steps (a scope is made
//! and then entered, a lease file is opened and then locked), and an entry between the two
//! steps looks like a dead owner's. [`table`] is the rule for them. A start holds the
//! directory's `flock` shared across its two steps, so starts do not wait for each other.
//! A sweep removes only under the lock held exclusively, so it never meets an entry between
//! its steps. It asks for the lock only when it has found something to remove, and waits
//! for it at most [`TABLE_WAIT`]: a stream of starts can keep it from the lock, and then
//! what it found waits for the next sweep. [`sweep_table`] is a turn and that rule together.
//!
//! What says an owner is dead is each table's own: the owner's `flock` on its record or
//! its lease, which the kernel releases however the process dies and no reused pid holds,
//! and for a scope the (pid, start time) in its name.
//!
//! # The override
//!
//! `FCVM_SWEEP_EVERY_MS` replaces the interval of the sweeps it names, as
//! `name=milliseconds` pairs separated by commas: `reclaim=0` makes every start sweep the
//! owner records, `cgroup-scopes=3600000` keeps a run of an hour from sweeping scopes. `*`
//! names every sweep not named. A test that counts what one start lists
//! (tests/test_start_dir_reads.rs) holds off the sweeps it knows by name and sets `*=0`, so
//! a sweep added later runs in every start there and fails that test until it is named.

use std::borrow::Cow;
use std::fs::File;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg, OFlag};
use tracing::warn;

/// The environment variable that replaces sweeps' intervals (see the module doc).
pub const EVERY_ENV: &str = "FCVM_SWEEP_EVERY_MS";

/// A sweep: its name in [`EVERY_ENV`], and how often a start runs it.
#[derive(Debug, PartialEq, Eq)]
pub struct Sweep {
    pub name: &'static str,
    pub every: Duration,
}

/// The owner records' sweep (`crate::reclaim`). It lists every bucket and tries every
/// owner's lock; one a second is one launch in hundreds during a ramp, and bounds how long
/// a killed VM's directory, namespace and rules outlive it on a host that keeps launching.
pub const RECLAIM: Sweep = Sweep {
    name: "reclaim",
    every: Duration::from_secs(1),
};

/// The sweep of a cgroup scopes' parent (`crate::cgroup`). A killed fcvm's scope is an
/// empty cgroup until it is removed.
pub const CGROUP_SCOPES: Sweep = Sweep {
    name: "cgroup-scopes",
    every: Duration::from_secs(5),
};

/// The sweep of the routed IPv6 address leases (`crate::network::routed`). A killed VM's
/// lease file holds no address, since its lock went with the process: it costs an inode.
pub const ROUTED_LEASES: Sweep = Sweep {
    name: "routed-leases",
    every: Duration::from_secs(5),
};

/// The sweep of a state directory (`crate::state::manager`). Until it runs a dead VM's
/// state holds nothing: its owner is not live.
pub const STATE: Sweep = Sweep {
    name: "state",
    every: Duration::from_secs(60),
};

/// Every sweep. A name in [`EVERY_ENV`] that is none of these is reported.
pub const ALL: [&Sweep; 4] = [&RECLAIM, &CGROUP_SCOPES, &ROUTED_LEASES, &STATE];

impl Sweep {
    /// The interval in force: the one [`EVERY_ENV`] gives this sweep, or its own.
    pub fn interval(&self) -> Duration {
        self.interval_from(std::env::var(EVERY_ENV).ok().as_deref())
    }

    /// [`Sweep::interval`] from the value of [`EVERY_ENV`]. A pair that names this sweep
    /// wins over `*`, the last of two pairs with one name wins, and a pair that is not
    /// `name=milliseconds` changes nothing.
    pub fn interval_from(&self, value: Option<&str>) -> Duration {
        let (mut named, mut any) = (None, None);
        for pair in value.unwrap_or_default().split(',') {
            let Some((name, ms)) = pair.split_once('=') else {
                continue;
            };
            let Ok(ms) = ms.trim().parse::<u64>() else {
                continue;
            };
            match name.trim() {
                "*" => any = Some(ms),
                name if name == self.name => named = Some(ms),
                _ => {}
            }
        }
        named.or(any).map_or(self.every, Duration::from_millis)
    }
}

/// The parts of the value of [`EVERY_ENV`] that set nothing: a pair that is not
/// `name=milliseconds`, or whose name is no sweep's.
pub fn unknown_in(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            !pair.split_once('=').is_some_and(|(name, ms)| {
                let name = name.trim();
                ms.trim().parse::<u64>().is_ok()
                    && (name == "*" || ALL.iter().any(|sweep| sweep.name == name))
            })
        })
        .collect()
}

/// Say once, at startup, which parts of [`EVERY_ENV`] set nothing: a mistyped name would
/// otherwise leave a sweep at its own interval with no sign of it.
pub fn warn_about_unknown_names() {
    let Ok(value) = std::env::var(EVERY_ENV) else {
        return;
    };
    let unknown = unknown_in(&value);
    if !unknown.is_empty() {
        let known: Vec<&str> = ALL.iter().map(|sweep| sweep.name).collect();
        warn!(
            ?unknown,
            ?known,
            "{EVERY_ENV} has parts that set no sweep's interval; each is name=milliseconds"
        );
    }
}

/// The names of a directory's one sweep ([`Place::new`]).
pub(crate) const STAMP: &str = ".sweep-stamp";
pub(crate) const LOCK: &str = ".sweep-lock";

/// Where a sweep keeps its stamp and its lock: two names in a directory that is held open.
/// Every file is reached through the descriptor, never by path, so a directory another
/// user could swap for one of theirs between a check and a use is judged once, by whoever
/// opened it.
pub struct Place<'a> {
    dir: &'a File,
    path: &'a Path,
    stamp: Cow<'static, str>,
    lock: Cow<'static, str>,
    /// Whether users share the directory ([`Place::shared`]).
    shared: bool,
}

impl<'a> Place<'a> {
    /// The place of a directory's one sweep: `.sweep-stamp` and `.sweep-lock` in it.
    /// Neither name ends in `.lock` or `.json`, so a table that keeps such files beside
    /// them does not take them for entries.
    pub fn new(dir: &'a File, path: &'a Path) -> Self {
        Self {
            dir,
            path,
            stamp: Cow::Borrowed(STAMP),
            lock: Cow::Borrowed(LOCK),
            shared: false,
        }
    }

    /// The place of one of several sweeps kept in one directory, each with a `key` of
    /// letters, digits, `-` and `_`.
    pub fn keyed(dir: &'a File, path: &'a Path, key: &str) -> Self {
        Self {
            dir,
            path,
            stamp: Cow::Owned(format!("{STAMP}-{key}")),
            lock: Cow::Owned(format!("{LOCK}-{key}")),
            shared: false,
        }
    }

    /// The place of a sweep in a directory that root and another user both work in, under
    /// the names `stamp` and `lock`. Its files are readable and writable by everyone
    /// whatever the umask, and the stamp is rewritten in place: in a sticky directory a
    /// user cannot rename over a file another user made. A start that reads half a stamp
    /// finds a sweep due and then the lock held by the stamp's writer, so it has no turn.
    pub fn shared(dir: &'a File, path: &'a Path, stamp: &'static str, lock: &'static str) -> Self {
        Self {
            dir,
            path,
            stamp: Cow::Borrowed(stamp),
            lock: Cow::Borrowed(lock),
            shared: true,
        }
    }

    /// The stamp's name in the directory.
    pub fn stamp_name(&self) -> &str {
        &self.stamp
    }

    /// The lock's name in the directory.
    pub fn lock_name(&self) -> &str {
        &self.lock
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        self.path
    }

    fn display(&self, name: &str) -> String {
        self.path.join(name).display().to_string()
    }

    fn open(&self, name: &str, flags: OFlag) -> nix::Result<File> {
        use std::os::unix::fs::PermissionsExt;
        let mode = if self.shared { 0o666 } else { 0o600 };
        let file = nix::fcntl::openat(
            self.dir,
            name,
            flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::from_bits_truncate(mode),
        )
        .map(File::from)?;
        if self.shared && flags.contains(OFlag::O_CREAT) {
            // Whatever the umask. Only the file's owner and root can; the other user finds
            // the mode its maker gave it.
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode));
        }
        Ok(file)
    }

    /// Whether the sweep that last began did so in this boot and less than `every` ago.
    fn swept_lately(&self, every: Duration) -> Result<bool> {
        use std::io::Read;
        let mut file = match self.open(&self.stamp, OFlag::O_RDONLY) {
            Ok(file) => file,
            Err(nix::errno::Errno::ENOENT) => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("opening {}", self.display(&self.stamp)))
            }
        };
        let mut stamp = String::new();
        // A stamp that is not text names no time.
        if file.read_to_string(&mut stamp).is_err() {
            return Ok(false);
        }
        Ok(stamp_is_fresh(&stamp, &boot_id(), boottime_ns(), every))
    }

    /// Write the stamp of a sweep that began at `began`, in the boot `boot`. It is written
    /// under a temporary name and renamed, so a start never reads half of one; in a
    /// directory users share it is rewritten in place ([`Place::shared`]).
    fn write_stamp(&self, boot: &str, began: u64) -> Result<()> {
        use std::io::Write;
        if self.shared {
            let mut file = self
                .open(&self.stamp, OFlag::O_CREAT | OFlag::O_WRONLY)
                .with_context(|| format!("opening {}", self.display(&self.stamp)))?;
            return file
                .set_len(0)
                .and_then(|()| file.write_all(stamp_text(boot, began).as_bytes()))
                .with_context(|| format!("writing {}", self.display(&self.stamp)));
        }
        let temp = format!("{}.{}", self.stamp, std::process::id());
        let mut file = self
            .open(&temp, OFlag::O_CREAT | OFlag::O_WRONLY | OFlag::O_TRUNC)
            .with_context(|| format!("creating {}", self.display(&temp)))?;
        file.write_all(stamp_text(boot, began).as_bytes())
            .with_context(|| format!("writing {}", self.display(&temp)))?;
        drop(file);
        nix::fcntl::renameat(self.dir, temp.as_str(), self.dir, &*self.stamp)
            .with_context(|| format!("publishing {}", self.display(&self.stamp)))
    }

    /// Try the sweep's lock without waiting.
    fn try_lock(&self) -> Result<Option<Flock<File>>> {
        let lock = self
            .open(&self.lock, OFlag::O_CREAT | OFlag::O_WRONLY)
            .with_context(|| format!("opening {}", self.display(&self.lock)))?;
        match Flock::lock(lock, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(Some(lock)),
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
            Err((_, error)) => {
                Err(error).with_context(|| format!("locking {}", self.display(&self.lock)))
            }
        }
    }
}

/// Open the directory of a [`Place`], without following a symbolic link.
pub fn open_dir(path: &Path) -> Result<File> {
    nix::fcntl::open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map(File::from)
    .with_context(|| format!("opening {}", path.display()))
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|id| id.trim().to_string())
        .unwrap_or_default()
}

/// Nanoseconds since boot, suspend included (`CLOCK_BOOTTIME`).
pub(crate) fn boottime_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the kernel writes one timespec through a valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) };
    (now.tv_sec as u64) * 1_000_000_000 + now.tv_nsec as u64
}

/// What a sweep that begins at `now` writes in the stamp.
pub(crate) fn stamp_text(boot: &str, now: u64) -> String {
    format!("{boot} {now}")
}

/// Whether `stamp` says a sweep began in the boot `boot` less than `every` before `now`.
/// Anything else makes a sweep due: another boot's stamp, one that does not parse, and one
/// later than `now`, which a process in another time namespace can write.
fn stamp_is_fresh(stamp: &str, boot: &str, now: u64, every: Duration) -> bool {
    let Some((stamp_boot, began)) = stamp.trim().split_once(' ') else {
        return false;
    };
    let Ok(began) = began.parse::<u64>() else {
        return false;
    };
    stamp_boot == boot && began <= now && u128::from(now - began) < every.as_nanos()
}

/// The right to sweep now, held for the length of the sweep. With it the starts that
/// arrive while the sweep runs find either the lock held or the stamp new.
#[derive(Debug)]
pub struct Turn {
    _lock: Option<Flock<File>>,
}

/// The turn to sweep `place` now: `None` when a sweep began less than `every` ago or
/// another process holds the lock. Otherwise the stamp is rewritten and the turn returned.
/// The stamp is written when the sweep begins, not when it ends: a sweeper killed at once
/// has removed nothing and leaves its stamp, so what was there waits one more interval, and
/// its lock goes with its process.
///
/// With `every` zero every caller has a turn, beside whichever caller is sweeping, and
/// neither the lock nor the stamp is touched.
pub fn turn_if_due(place: &Place, every: Duration) -> Result<Option<Turn>> {
    if every.is_zero() {
        return Ok(Some(Turn { _lock: None }));
    }
    if place.swept_lately(every)? {
        return Ok(None);
    }
    let Some(lock) = place.try_lock()? else {
        return Ok(None);
    };
    // The process that held the lock before this one may have stamped and swept already.
    if place.swept_lately(every)? {
        return Ok(None);
    }
    place.write_stamp(&boot_id(), boottime_ns())?;
    Ok(Some(Turn { _lock: Some(lock) }))
}

/// The turn to sweep `place` whatever its stamp says, for a caller that cannot go on
/// without a sweep: `None` only while another process holds the lock. The lock is the
/// turn. A stamp that cannot be written (a full disk) is reported and the turn is given
/// all the same: the starts that come during this sweep find the lock held, and the ones
/// after it sweep once more than they needed to.
pub fn turn_now(place: &Place) -> Result<Option<Turn>> {
    let Some(lock) = place.try_lock()? else {
        return Ok(None);
    };
    if let Err(error) = place.write_stamp(&boot_id(), boottime_ns()) {
        warn!(error = %format!("{error:#}"), "sweeping without a stamp");
    }
    Ok(Some(Turn { _lock: Some(lock) }))
}

/// Set `place`'s stamp to a sweep that began `ago` before now, for tests of what a start
/// does with a fresh stamp and with an old one. An `ago` longer than this boot is old
/// writes the stamp of another boot, so a test that needs a stamp of this boot asks for
/// seconds, not hours: a CI runner is minutes old.
#[doc(hidden)]
pub fn set_stamp(place: &Place, ago: Duration) -> Result<()> {
    let now = boottime_ns();
    match u64::try_from(ago.as_nanos())
        .ok()
        .and_then(|ago| now.checked_sub(ago))
    {
        Some(began) => place.write_stamp(&boot_id(), began),
        None => place.write_stamp("the-boot-before", now),
    }
}

/// How long a sweep waits for a table's lock before it leaves what it found to the next
/// sweep. Starts hold the lock shared for the two steps that make an entry, so a moment
/// with no holder comes within milliseconds.
pub const TABLE_WAIT: Duration = Duration::from_millis(100);

/// The lock of a table whose entries are made in two steps (see the module doc): the
/// `flock` of the table's directory. Released when the returned file is dropped, and by
/// the kernel when its holder dies.
pub mod table {
    use super::*;

    fn lock(dir: &Path, arg: FlockArg) -> nix::Result<Option<Flock<File>>> {
        let file = nix::fcntl::open(
            dir,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .map(File::from)?;
        let mut file = Some(file);
        loop {
            match Flock::lock(file.take().expect("put back on each retry"), arg) {
                Ok(lock) => return Ok(Some(lock)),
                Err((_, nix::errno::Errno::EWOULDBLOCK)) => return Ok(None),
                Err((back, nix::errno::Errno::EINTR)) => file = Some(back),
                Err((_, error)) => return Err(error),
            }
        }
    }

    /// Hold the table shared, waiting for a sweep that is removing: for a start, across
    /// the steps that make its entry.
    pub fn shared(dir: &Path) -> nix::Result<Flock<File>> {
        lock(dir, FlockArg::LockShared).map(|lock| lock.expect("a waiting lock is granted"))
    }

    /// Hold the table exclusively, waiting for it: for a caller that removes entries
    /// outside a sweep's turn.
    pub fn exclusive(dir: &Path) -> nix::Result<Flock<File>> {
        lock(dir, FlockArg::LockExclusive).map(|lock| lock.expect("a waiting lock is granted"))
    }

    /// Hold the table exclusively if it can be had within `wait`: `None` when starts held
    /// it shared the whole time.
    pub fn exclusive_within(dir: &Path, wait: Duration) -> nix::Result<Option<Flock<File>>> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            if let Some(lock) = lock(dir, FlockArg::LockExclusiveNonblock)? {
                return Ok(Some(lock));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// What [`sweep_table`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum TableSweep<R> {
    /// No sweep was due, or another process is sweeping.
    NotDue,
    /// It was this caller's turn and `find` found nothing to remove.
    NothingFound,
    /// It found something and the table stayed locked for `wait`: the next sweep removes
    /// what this one found.
    TableBusy,
    /// What `remove` returned.
    Swept(R),
}

/// Sweep the table `table` if it is this caller's turn at `place`.
///
/// `find` looks for what may be removed. It runs with no lock on the table, so it must only
/// read, and what it returns can be stale by the time `remove` has it. `remove` runs under
/// the table's lock held exclusively, so no start is between the two steps that make its
/// entry, and it decides again for each entry whether its owner is gone.
pub fn sweep_table<C, R>(
    place: &Place,
    every: Duration,
    table: &Path,
    wait: Duration,
    find: impl FnOnce() -> Vec<C>,
    remove: impl FnOnce(Vec<C>) -> R,
) -> Result<TableSweep<R>> {
    let Some(_turn) = turn_if_due(place, every)? else {
        return Ok(TableSweep::NotDue);
    };
    let found = find();
    if found.is_empty() {
        return Ok(TableSweep::NothingFound);
    }
    match table::exclusive_within(table, wait)
        .with_context(|| format!("locking {} for a sweep", table.display()))?
    {
        Some(_table) => Ok(TableSweep::Swept(remove(found))),
        None => Ok(TableSweep::TableBusy),
    }
}

/// What the tests of this module and of the sweeps' callers share.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;

    /// Another process holding an exclusive `flock` on a file, as a sweeper holds a
    /// sweep's lock or a start holds its lease.
    pub(crate) struct Holder(std::process::Child);

    /// Take an exclusive `flock` on `path` in another process, and return once it is held.
    pub(crate) fn hold_from_another_process(path: &Path) -> Holder {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let mut holder = std::process::Command::new("flock")
            .args(["-x", "-o"])
            .arg(path)
            .args(["sh", "-c", "echo held; exec sleep 600"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("flock(1) from util-linux");
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "held");
        Holder(holder)
    }

    impl Holder {
        /// SIGKILL the holder, as a sweeper or an owner is killed: the kernel releases its
        /// lock and nothing of its own runs.
        pub(crate) fn kill(mut self) {
            self.end();
        }

        fn end(&mut self) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.0.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = self.0.wait();
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            self.end();
        }
    }

    /// One of the sweeps' callers, as the matrix below drives it: a table in a temporary
    /// directory, entries of dead and of live owners in it, and what one start does.
    pub(crate) trait Caller: Sized + Sync {
        /// The sweep this caller runs.
        const SWEEP: &'static Sweep;
        fn new() -> Self;
        /// Leave what a killed owner leaves, and return a path that is there until a sweep
        /// has removed it.
        fn abandon(&self, n: usize) -> PathBuf;
        /// An entry whose owner is alive, the path that is there while the entry is, and
        /// what keeps the owner alive.
        fn live(&self, n: usize) -> (PathBuf, Box<dyn std::any::Any>);
        /// What one start does about the sweep, with the interval `every`.
        fn start(&self, every: Duration);
        /// Run `f` with the place of this caller's sweep.
        fn with_place<R>(&self, f: impl FnOnce(&Place) -> R) -> R;
        /// The directories a sweep lists.
        fn listed(&self) -> Vec<PathBuf>;
        /// The table starts hold shared (`table`), for a caller that has one.
        fn table(&self) -> Option<PathBuf> {
            None
        }
    }

    use super::{table, turn_if_due, Duration, Place, Sweep};
    use std::path::PathBuf;

    const HOUR: Duration = Duration::from_secs(3600);

    /// A stamp's age and an interval it has outlived. Seconds, so that the stamp is this
    /// boot's on a host that has just booted ([`super::set_stamp`]).
    const OLD: Duration = Duration::from_secs(2);
    const SHORT: Duration = Duration::from_secs(1);

    /// How long a test waits for something that must happen before it fails for a hang:
    /// half of what nextest gives a unit test before it kills it, so the failure names
    /// what did not happen.
    pub(crate) const HANG_GUARD: Duration = Duration::from_secs(60);

    fn age_stamp<C: Caller>(caller: &C, ago: Duration) {
        caller
            .with_place(|place| super::set_stamp(place, ago))
            .unwrap();
    }

    /// How many times `dirs` are listed while `f` runs. A listing shows as an access of
    /// the directory itself: an event with no name. Reading or writing a file in a watched
    /// directory raises one with the file's name.
    fn listings_during(dirs: &[PathBuf], f: impl FnOnce()) -> usize {
        use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        for dir in dirs {
            inotify.add_watch(dir, AddWatchFlags::IN_ACCESS).unwrap();
        }
        f();
        inotify.read_events().map_or(0, |events| {
            events.iter().filter(|event| event.name.is_none()).count()
        })
    }

    /// The cases every caller of a sweep is held to (`caller_matrix!`).
    pub(crate) mod cases {
        use super::*;

        /// The caller's sweep is one of [`ALL`], so `FCVM_SWEEP_EVERY_MS` can name it.
        pub(crate) fn the_callers_sweep_can_be_named<C: Caller>() {
            let sweep = C::SWEEP;
            assert!(super::super::ALL.contains(&sweep));
            let named = format!("*=7,{}=0", sweep.name);
            assert_eq!(sweep.interval_from(Some(&named)), Duration::ZERO);
            assert!(super::super::unknown_in(&named).is_empty());
        }

        /// A start inside the interval removes nothing and lists nothing, whatever a killed
        /// owner left: its cost does not grow with the table.
        pub(crate) fn a_start_inside_the_interval_lists_nothing<C: Caller>() {
            let caller = C::new();
            caller.start(HOUR);
            let dead: Vec<PathBuf> = (0..8).map(|n| caller.abandon(n)).collect();
            let listings = listings_during(&caller.listed(), || caller.start(HOUR));
            assert_eq!(listings, 0, "a start listed with no sweep due");
            assert!(dead.iter().all(|path| path.exists()));
            // The detector fires: the sweep that is due lists.
            age_stamp(&caller, OLD);
            let listings = listings_during(&caller.listed(), || caller.start(SHORT));
            assert!(listings > 0, "a sweep that was due listed nothing");
        }

        /// A killed owner's entry is removed by the first start an interval or more after
        /// the last sweep began, and a live owner's entry is kept.
        pub(crate) fn a_dead_owners_entry_goes_after_the_interval_and_a_live_ones_stays<
            C: Caller,
        >() {
            let caller = C::new();
            caller.start(HOUR);
            let dead = caller.abandon(0);
            let (live, _owner) = caller.live(1);
            age_stamp(&caller, OLD);
            caller.start(HOUR);
            assert!(dead.exists(), "swept inside the interval");
            caller.start(SHORT);
            assert!(!dead.exists(), "not swept after the interval");
            assert!(live.exists(), "a live owner's entry was removed");
        }

        /// A stamp kept on a disk survives a reboot. One of another boot does not hold off
        /// the first sweep of this one, however new it is.
        pub(crate) fn a_stamp_of_another_boot_does_not_hold_off_the_sweep<C: Caller>() {
            let caller = C::new();
            caller.start(HOUR);
            let dead = caller.abandon(0);
            // Longer ago than this boot is old: written as another boot's, at this time.
            age_stamp(&caller, Duration::from_secs(40 * 365 * 86_400));
            caller.start(HOUR);
            assert!(!dead.exists());
        }

        /// While another process has the turn a start does not sweep and does not wait, and
        /// a sweeper that is killed gives the turn back.
        pub(crate) fn a_start_does_not_sweep_while_another_process_does<C: Caller>() {
            let caller = C::new();
            let dead = caller.abandon(0);
            let lock = caller.with_place(|place| place.path().join(place.lock_name()));
            let stamp = caller.with_place(|place| place.path().join(place.stamp_name()));
            let sweeper = hold_from_another_process(&lock);
            let began = std::time::Instant::now();
            caller.start(HOUR);
            assert!(began.elapsed() < Duration::from_secs(5), "the start waited");
            assert!(dead.exists(), "swept beside the process that has the turn");
            assert!(!stamp.exists(), "stamped without sweeping");
            sweeper.kill();
            caller.start(HOUR);
            assert!(!dead.exists());
        }

        /// With the interval set to nothing every start sweeps.
        pub(crate) fn a_zero_interval_sweeps_in_every_start<C: Caller>() {
            let caller = C::new();
            for n in 0..3 {
                let dead = caller.abandon(n);
                caller.start(Duration::ZERO);
                assert!(!dead.exists(), "start {n} did not sweep");
            }
        }

        /// Starts that find a sweep due together remove a dead owner's entry between them,
        /// and none of them fails for meeting another's sweep.
        pub(crate) fn starts_that_are_due_together_all_go_on<C: Caller>() {
            let caller = C::new();
            let dead: Vec<PathBuf> = (0..4).map(|n| caller.abandon(n)).collect();
            let together = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    scope.spawn(|| {
                        together.wait();
                        caller.start(HOUR);
                    });
                }
            });
            assert!(dead.iter().all(|path| !path.exists()));
            // The turn was taken once: the stamp is new.
            assert!(caller
                .with_place(|place| turn_if_due(place, HOUR))
                .unwrap()
                .is_none());
        }

        /// Starts do not wait for each other: one goes on while another holds the table, as
        /// a start does across the two steps that make its entry.
        pub(crate) fn a_start_does_not_wait_for_another_starts_hold_of_the_table<C: Caller>() {
            let caller = C::new();
            let table_dir = caller.table().expect("a caller with a table");
            let another_start = table::shared(&table_dir).unwrap();
            let (done, went_on) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    caller.start(HOUR);
                    let _ = done.send(());
                });
                let went_on = went_on.recv_timeout(HANG_GUARD);
                // Let go first, so a start that did wait can end and this test can fail.
                drop(another_start);
                went_on.expect("a start waited for another start's hold of the table");
            });
        }

        /// A sweep that is due while another start is between the steps that make its entry
        /// removes nothing: it cannot have the table. The start that swept goes on, and the
        /// next sweep removes what is dead.
        pub(crate) fn a_sweep_leaves_the_table_alone_while_a_start_holds_it<C: Caller>() {
            let caller = C::new();
            let table_dir = caller.table().expect("a caller with a table");
            let dead = caller.abandon(0);
            let in_the_window = table::shared(&table_dir).unwrap();
            caller.start(Duration::ZERO);
            assert!(dead.exists(), "removed while a start held the table");
            drop(in_the_window);
            caller.start(Duration::ZERO);
            assert!(!dead.exists());
        }

        /// A start waits for a sweep that is removing: it does not make its entry while the
        /// table is held exclusively.
        pub(crate) fn a_start_waits_for_a_sweep_that_holds_the_table<C: Caller>() {
            let caller = C::new();
            let table_dir = caller.table().expect("a caller with a table");
            let sweeping = table::exclusive(&table_dir).unwrap();
            let (done, went_on) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    caller.start(HOUR);
                    let _ = done.send(());
                });
                assert!(
                    went_on.recv_timeout(Duration::from_millis(300)).is_err(),
                    "a start went on while a sweep held the table"
                );
                drop(sweeping);
                went_on
                    .recv_timeout(HANG_GUARD)
                    .expect("the start never went on");
            });
        }
    }

    /// The tests every caller of a sweep has: `caller_matrix!(Type)` in the caller's test
    /// module, for a type that implements [`Caller`].
    macro_rules! caller_matrix {
        ($caller:ty) => {
            mod sweep_caller_matrix {
                use super::*;
                use $crate::sweep::testing::cases;

                #[test]
                fn the_callers_sweep_can_be_named() {
                    cases::the_callers_sweep_can_be_named::<$caller>()
                }
                #[test]
                fn a_start_inside_the_interval_lists_nothing() {
                    cases::a_start_inside_the_interval_lists_nothing::<$caller>()
                }
                #[test]
                fn a_dead_owners_entry_goes_after_the_interval_and_a_live_ones_stays() {
                    cases::a_dead_owners_entry_goes_after_the_interval_and_a_live_ones_stays::<
                        $caller,
                    >()
                }
                #[test]
                fn a_stamp_of_another_boot_does_not_hold_off_the_sweep() {
                    cases::a_stamp_of_another_boot_does_not_hold_off_the_sweep::<$caller>()
                }
                #[test]
                fn a_start_does_not_sweep_while_another_process_does() {
                    cases::a_start_does_not_sweep_while_another_process_does::<$caller>()
                }
                #[test]
                fn a_zero_interval_sweeps_in_every_start() {
                    cases::a_zero_interval_sweeps_in_every_start::<$caller>()
                }
                #[test]
                fn starts_that_are_due_together_all_go_on() {
                    cases::starts_that_are_due_together_all_go_on::<$caller>()
                }
            }
        };
    }
    pub(crate) use caller_matrix;

    /// The further tests of a caller whose table starts hold shared ([`Caller::table`]).
    macro_rules! table_matrix {
        ($caller:ty) => {
            mod sweep_table_matrix {
                use super::*;
                use $crate::sweep::testing::cases;

                #[test]
                fn a_start_does_not_wait_for_another_starts_hold_of_the_table() {
                    cases::a_start_does_not_wait_for_another_starts_hold_of_the_table::<$caller>()
                }
                #[test]
                fn a_sweep_leaves_the_table_alone_while_a_start_holds_it() {
                    cases::a_sweep_leaves_the_table_alone_while_a_start_holds_it::<$caller>()
                }
                #[test]
                fn a_start_waits_for_a_sweep_that_holds_the_table() {
                    cases::a_start_waits_for_a_sweep_that_holds_the_table::<$caller>()
                }
            }
        };
    }
    pub(crate) use table_matrix;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place_in(dir: &tempfile::TempDir) -> (File, std::path::PathBuf) {
        (open_dir(dir.path()).unwrap(), dir.path().to_path_buf())
    }

    const HOUR: Duration = Duration::from_secs(3600);

    /// A stamp is fresh only when it is this boot's, parses, and names a time at or before
    /// now and less than the interval ago.
    #[test]
    fn a_stamp_is_fresh_only_within_the_interval_of_this_boot() {
        let second = Duration::from_secs(1);
        let now = 50_000_000_000u64;
        let fresh = |stamp: &str| stamp_is_fresh(stamp, "boot-a", now, second);
        assert!(fresh(&stamp_text("boot-a", now)));
        assert!(fresh(&stamp_text("boot-a", now - 999_999_999)));
        assert!(
            !fresh(&stamp_text("boot-a", now - 1_000_000_000)),
            "the interval has passed"
        );
        assert!(!fresh(&stamp_text("boot-a", 0)));
        assert!(
            !fresh(&stamp_text("boot-b", now)),
            "a stamp of another boot"
        );
        assert!(
            !fresh(&stamp_text("boot-a", now + 1)),
            "a stamp later than now"
        );
        for junk in [
            "",
            "boot-a",
            "boot-a soon",
            "boot-a -5",
            "boot-a 1 2",
            "\0\0",
        ] {
            assert!(!fresh(junk), "{junk:?}");
        }
        // The clock a stamp is taken from does not go back.
        let (a, b) = (boottime_ns(), boottime_ns());
        assert!(a > 0 && b >= a);
    }

    #[test]
    fn a_turn_is_given_once_an_interval() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&dir);
        let place = Place::new(&fd, &path);
        // No stamp: due. The turn stamps when it begins.
        let turn = turn_if_due(&place, HOUR).unwrap().expect("no stamp yet");
        assert!(dir.path().join(STAMP).exists() && dir.path().join(LOCK).exists());
        // While it is held, and after, within the interval: no turn.
        assert!(turn_if_due(&place, HOUR).unwrap().is_none());
        drop(turn);
        assert!(turn_if_due(&place, HOUR).unwrap().is_none());
        // The interval has passed.
        assert!(turn_if_due(&place, Duration::from_nanos(1))
            .unwrap()
            .is_some());
        set_stamp(&place, Duration::from_secs(7200)).unwrap();
        assert!(turn_if_due(&place, HOUR).unwrap().is_some());
        // Nothing but the stamp and the lock is left: the temporary name is gone.
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [LOCK, STAMP]);
    }

    /// A stamp on a disk survives a reboot. It must not hold off the first sweep of the
    /// next boot, however new it looks.
    #[test]
    fn a_stamp_of_another_boot_makes_a_sweep_due() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&dir);
        let place = Place::new(&fd, &path);
        place.write_stamp("the-boot-before", boottime_ns()).unwrap();
        assert!(turn_if_due(&place, HOUR).unwrap().is_some());
        // And one older than this boot, as `set_stamp` writes it.
        set_stamp(&place, Duration::from_secs(40 * 365 * 86_400)).unwrap();
        assert!(turn_if_due(&place, HOUR).unwrap().is_some());
        // A stamp that names no time.
        std::fs::write(dir.path().join(STAMP), b"\xff\xfe").unwrap();
        assert!(turn_if_due(&place, HOUR).unwrap().is_some());
        std::fs::write(dir.path().join(STAMP), b"").unwrap();
        assert!(turn_if_due(&place, HOUR).unwrap().is_some());
    }

    /// While a process holds the lock nobody else has a turn, and it stamped nothing if it
    /// was refused. Its lock goes with it however it dies.
    #[test]
    fn a_held_lock_refuses_the_turn_and_a_dead_holder_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&dir);
        let place = Place::new(&fd, &path);
        let holder = testing::hold_from_another_process(&dir.path().join(LOCK));
        assert!(turn_if_due(&place, HOUR).unwrap().is_none());
        assert!(turn_now(&place).unwrap().is_none());
        assert!(!dir.path().join(STAMP).exists(), "stamped without a turn");
        holder.kill();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(turn) = turn_if_due(&place, HOUR).unwrap() {
                drop(turn);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the lock outlived its holder"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_turn_now_ignores_the_stamp_and_writes_it() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&dir);
        let place = Place::new(&fd, &path);
        set_stamp(&place, Duration::ZERO).unwrap();
        assert!(turn_if_due(&place, HOUR).unwrap().is_none());
        let turn = turn_now(&place).unwrap().expect("nobody holds the lock");
        assert!(turn_now(&place).unwrap().is_none());
        drop(turn);
        set_stamp(&place, Duration::from_secs(7200)).unwrap();
        drop(turn_now(&place).unwrap().expect("free again"));
        assert!(
            turn_if_due(&place, HOUR).unwrap().is_none(),
            "a turn taken now did not stamp"
        );
    }

    /// A stamp that cannot be written does not cost a caller that must sweep its turn, and
    /// is an error for a start, whose caller decides.
    #[test]
    fn a_turn_now_needs_the_lock_and_not_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&dir);
        let place = Place::new(&fd, &path);
        std::fs::create_dir(dir.path().join(STAMP)).unwrap();
        let turn = turn_now(&place).unwrap().expect("nobody holds the lock");
        assert!(turn_now(&place).unwrap().is_none());
        drop(turn);
        assert!(turn_if_due(&place, HOUR).is_err());
    }

    #[test]
    fn a_zero_interval_gives_every_caller_a_turn_and_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&dir);
        let place = Place::new(&fd, &path);
        let first = turn_if_due(&place, Duration::ZERO).unwrap();
        let second = turn_if_due(&place, Duration::ZERO).unwrap();
        assert!(first.is_some() && second.is_some());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Of the callers that find a sweep due together, one has the turn.
    #[test]
    fn one_of_the_callers_that_are_due_together_has_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let callers: Vec<_> = (0..16)
            .map(|_| {
                let (path, start) = (path.clone(), start.clone());
                std::thread::spawn(move || {
                    let fd = open_dir(&path).unwrap();
                    let place = Place::new(&fd, &path);
                    start.wait();
                    let turn = turn_if_due(&place, HOUR).unwrap();
                    // Held as long as a sweep would be.
                    std::thread::sleep(Duration::from_millis(50));
                    turn.is_some()
                })
            })
            .collect();
        let turns = callers
            .into_iter()
            .map(|caller| caller.join().unwrap())
            .filter(|had_the_turn| *had_the_turn)
            .count();
        assert_eq!(turns, 1);
    }

    #[test]
    fn keyed_places_in_one_directory_do_not_share_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&dir);
        let (a, b) = (
            Place::keyed(&fd, &path, "0a1b"),
            Place::keyed(&fd, &path, "2c3d"),
        );
        let _held = turn_if_due(&a, HOUR).unwrap().expect("a is due");
        assert!(turn_if_due(&a, HOUR).unwrap().is_none());
        assert!(turn_if_due(&b, HOUR).unwrap().is_some());
        assert_eq!(a.stamp_name(), ".sweep-stamp-0a1b");
        assert_eq!(a.lock_name(), ".sweep-lock-0a1b");
        for name in [a.stamp_name(), a.lock_name(), STAMP, LOCK] {
            assert!(!name.ends_with(".lock") && !name.ends_with(".json"));
            assert!(Path::new(name).extension().is_none_or(|ext| ext != "lock"));
        }
    }

    /// A place users share keeps the names it is given, its files are everyone's to read
    /// and write whatever the umask, and no temporary name is left or used.
    #[test]
    fn a_shared_place_is_everyones_and_rewrites_its_stamp_in_place() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&dir);
        let place = Place::shared(&fd, &path, "sweep.stamp", "sweep.lock");
        let old = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
        let turn = turn_if_due(&place, HOUR);
        nix::sys::stat::umask(old);
        drop(turn.unwrap().expect("no stamp yet"));
        let stamp = dir.path().join("sweep.stamp");
        let inode = std::fs::metadata(&stamp).unwrap().ino();
        for name in ["sweep.stamp", "sweep.lock"] {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o666, "{name}");
        }
        assert!(turn_if_due(&place, HOUR).unwrap().is_none());
        // A stamp another build wrote, longer than this one's: rewritten whole, in place.
        std::fs::write(
            &stamp,
            "2026-11-26T12:00:00.123456789Z and then some more text",
        )
        .unwrap();
        drop(turn_if_due(&place, HOUR).unwrap().expect("not a stamp"));
        assert_eq!(std::fs::metadata(&stamp).unwrap().ino(), inode);
        let written = std::fs::read_to_string(&stamp).unwrap();
        let (boot, began) = written.split_once(' ').unwrap();
        assert_eq!(boot, boot_id());
        assert!(began.parse::<u64>().unwrap() <= boottime_ns());
        assert!(turn_if_due(&place, HOUR).unwrap().is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    /// The place's files are reached through the directory's descriptor: a symbolic link
    /// under either name is refused, not followed.
    #[test]
    fn a_symbolic_link_in_the_place_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("target");
        std::fs::write(&target, b"untouched").unwrap();
        let (fd, path) = place_in(&dir);
        let place = Place::new(&fd, &path);
        std::os::unix::fs::symlink(&target, dir.path().join(LOCK)).unwrap();
        assert!(turn_if_due(&place, HOUR).is_err());
        std::fs::remove_file(dir.path().join(LOCK)).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(STAMP)).unwrap();
        assert!(turn_if_due(&place, HOUR).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
    }

    #[test]
    fn the_override_names_each_sweep() {
        for sweep in ALL {
            let other = ALL
                .iter()
                .find(|other| other.name != sweep.name)
                .unwrap()
                .name;
            let of = |value: &str| sweep.interval_from(Some(value));
            assert_eq!(sweep.interval_from(None), sweep.every);
            assert_eq!(of(""), sweep.every);
            assert_eq!(of(&format!("{}=0", sweep.name)), Duration::ZERO);
            assert_eq!(of(&format!(" {} = 250 ", sweep.name)).as_millis(), 250);
            // Another sweep's pair, a pair that is not one, and a name that is a prefix.
            assert_eq!(of(&format!("{other}=0")), sweep.every);
            assert_eq!(of(&format!("{}=soon", sweep.name)), sweep.every);
            assert_eq!(of(&format!("{}x=0", sweep.name)), sweep.every);
            assert_eq!(of(sweep.name), sweep.every);
            // `*` is every sweep not named, wherever it stands.
            assert_eq!(of("*=0"), Duration::ZERO);
            assert_eq!(of(&format!("*=0,{}=3600000", sweep.name)), HOUR);
            assert_eq!(of(&format!("{}=3600000,*=0", sweep.name)), HOUR);
            assert_eq!(of(&format!("{other}=3600000,*=0")), Duration::ZERO);
            // The last of two pairs with one name.
            assert_eq!(of(&format!("{0}=7,{0}=9", sweep.name)).as_millis(), 9);
        }
        let mut names: Vec<&str> = ALL.iter().map(|sweep| sweep.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), ALL.len(), "two sweeps share a name");
    }

    #[test]
    fn the_parts_of_the_override_that_set_nothing_are_found() {
        assert!(unknown_in("reclaim=0,cgroup-scopes=1,routed-leases=2,state=4,*=3").is_empty());
        assert!(unknown_in("").is_empty() && unknown_in(" , ").is_empty());
        assert_eq!(
            unknown_in("reclaim=0, recliam=0 ,cgroup-scopes,*=x,=5"),
            ["recliam=0", "cgroup-scopes", "*=x", "=5"]
        );
    }

    /// Every sweep constant is in [`ALL`], so the override's names and the caller matrix
    /// cover it: a `Sweep` defined in this file and left out fails here.
    #[test]
    fn every_sweep_defined_here_is_listed() {
        let source = include_str!("sweep.rs");
        let defined = source
            .lines()
            .filter(|line| line.starts_with("pub const ") && line.contains(": Sweep = Sweep {"))
            .count();
        assert_eq!(defined, ALL.len());
        // None is defined in a caller, and each has a caller that runs the matrix for it.
        let callers = [
            ("reclaim.rs", include_str!("reclaim.rs"), "RECLAIM", false),
            (
                "cgroup.rs",
                include_str!("cgroup.rs"),
                "CGROUP_SCOPES",
                true,
            ),
            (
                "network/routed.rs",
                include_str!("network/routed.rs"),
                "ROUTED_LEASES",
                true,
            ),
            (
                "state/manager.rs",
                include_str!("state/manager.rs"),
                "STATE",
                false,
            ),
        ];
        assert_eq!(callers.len(), ALL.len());
        for (file, text, sweep, has_table) in callers {
            assert!(
                !text.contains("Sweep {\n"),
                "{file} defines a sweep of its own: define it in sweep.rs and list it in ALL"
            );
            let matrix = format!("const SWEEP: &'static Sweep = &crate::sweep::{sweep};");
            assert!(
                text.contains(&matrix),
                "{file} has no matrix caller of {sweep}"
            );
            assert_eq!(text.matches("caller_matrix!(").count(), 1, "{file}");
            assert_eq!(
                text.matches("table_matrix!(").count(),
                usize::from(has_table),
                "{file}"
            );
        }
    }

    #[test]
    fn a_table_is_shared_by_takers_and_exclusive_for_a_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Some(table::shared(dir.path()).unwrap());
        // Takers do not wait for each other.
        let (took, taken) = std::sync::mpsc::channel();
        let taker = {
            let path = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let held = table::shared(&path).unwrap();
                let _ = took.send(());
                held
            })
        };
        if taken.recv_timeout(testing::HANG_GUARD).is_err() {
            // Let go, so the taker can end and this test can fail.
            first.take();
            let _ = taker.join();
            panic!("a taker waited for another taker");
        }
        let second = taker.join().unwrap();
        // A sweep waits for them, and no longer than it was told to.
        let began = std::time::Instant::now();
        assert!(
            table::exclusive_within(dir.path(), Duration::from_millis(30))
                .unwrap()
                .is_none()
        );
        let waited = began.elapsed();
        assert!(
            waited >= Duration::from_millis(30) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        drop((first, second));
        let sweeping = table::exclusive_within(dir.path(), Duration::ZERO)
            .unwrap()
            .expect("nobody holds the table");
        // A taker waits for the sweep.
        let path = dir.path().to_path_buf();
        let (took, taken) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _held = table::shared(&path).unwrap();
            let _ = took.send(());
        });
        assert!(taken.recv_timeout(Duration::from_millis(200)).is_err());
        drop(sweeping);
        taken
            .recv_timeout(testing::HANG_GUARD)
            .expect("the taker never got the table");
    }

    #[test]
    fn a_table_sweep_finds_without_the_lock_and_removes_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let stamps = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&stamps);
        let place = Place::new(&fd, &path);
        let table = dir.path();
        let sweep = |every, found: Vec<u32>| {
            sweep_table(
                &place,
                every,
                table,
                Duration::from_millis(30),
                || {
                    // No lock on the table while finding: a taker can have it.
                    drop(
                        table::exclusive_within(table, Duration::ZERO)
                            .unwrap()
                            .unwrap(),
                    );
                    found
                },
                |found| {
                    // Exclusive while removing: no taker can.
                    assert!(table::exclusive_within(table, Duration::ZERO)
                        .unwrap()
                        .is_none());
                    found.len()
                },
            )
            .unwrap()
        };
        assert_eq!(sweep(HOUR, vec![1, 2]), TableSweep::Swept(2));
        assert_eq!(sweep(HOUR, vec![1, 2]), TableSweep::NotDue);
        set_stamp(&place, Duration::from_secs(7200)).unwrap();
        assert_eq!(sweep(HOUR, vec![]), TableSweep::NothingFound);
        // Every start with a zero interval.
        assert_eq!(sweep(Duration::ZERO, vec![7]), TableSweep::Swept(1));
        assert_eq!(sweep(Duration::ZERO, vec![7]), TableSweep::Swept(1));
    }

    /// Starts that hold the table for a sweep's whole wait keep it from removing, and it
    /// has still taken its turn: the next sweep, an interval later, tries again.
    #[test]
    fn a_table_sweep_that_cannot_have_the_lock_leaves_what_it_found() {
        let dir = tempfile::tempdir().unwrap();
        let stamps = tempfile::tempdir().unwrap();
        let (fd, path) = place_in(&stamps);
        let place = Place::new(&fd, &path);
        let _start = table::shared(dir.path()).unwrap();
        let swept = sweep_table(
            &place,
            HOUR,
            dir.path(),
            Duration::from_millis(30),
            || vec![1],
            |_| panic!("removed while a start held the table"),
        )
        .unwrap();
        assert_eq!(swept, TableSweep::<()>::TableBusy);
        assert!(turn_if_due(&place, HOUR).unwrap().is_none());
    }
}
