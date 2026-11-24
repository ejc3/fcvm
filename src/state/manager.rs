use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
use std::path::{Path, PathBuf};
use tokio::fs;

use super::types::VmState;

/// The abstract socket name that leases loopback address `ip` (`allocate_loopback_ip`).
pub const LOOPBACK_LEASE_PREFIX: &str = "fcvm-loopback-";

/// The file in the state directory that says which layout it holds (`LAYOUT`).
const LAYOUT_FILE: &str = "layout";

/// The layout this build reads and writes:
///
/// ```text
/// layout                                      this number
/// vms/<b>/<vm_id>.json  .json.tmp  .json.lock
/// names/<b>/<sha256(name)>/<pid>.<start>.<vm_id>     a name slot and its one token
/// names/<b>/.claim.<pid>.<start>.<vm_id>/            a claim being built
/// pids/<b>/<pid>.<vm_id>
/// sweep.lock  sweep.stamp
/// ```
///
/// `<b>` is a bucket (`bucket_of`), so no directory takes every VM's writes: a create or a
/// rename holds its directory's lock exclusively, and with every VM's state in one directory
/// that lock was what a thousand clones starting together waited on. Layout 1 kept
/// `<vm_id>.json` in the state directory itself, with `name/`, `pid/` and a host-wide
/// `names.lock`; `StateManager::check_layout` refuses a store that still has running VMs
/// of it.
const LAYOUT: &str = "2";

/// The VMs' states, under the state directory, in buckets.
const VMS_DIR: &str = "vms";

/// The name slots, under the state directory, in buckets: a directory per name in use,
/// holding the token of the VM that has it (`StateManager::claim_name`).
const NAMES_DIR: &str = "names";

/// The PID index, under the state directory, in buckets: an empty file `<pid>.<vm_id>` per
/// saved state with a PID (`StateManager::save_state`, `StateManager::load_state_by_pid`).
const PIDS_DIR: &str = "pids";

/// The prefix of a claim being built in a name bucket, before it is renamed onto its slot.
const CLAIM_PREFIX: &str = ".claim.";

/// How many times a claim retries its rename after removing a dead holder's token. Each
/// retry means another claimer took the slot in between, and that one is live.
const CLAIM_ATTEMPTS: usize = 16;

/// The bucket of `key`: one hex digit, the first of its SHA-256. The same in every process
/// and every build, which a hasher keyed per process would not be: two fcvm processes must
/// look for one VM in one directory.
///
/// Sixteen buckets, between two costs. A start's creates and renames lock its bucket, so
/// more buckets mean less waiting between starts. A listing opens every bucket, and that
/// cost is paid by every `fcvm ls` whatever the number of VMs: on an empty store a listing
/// of 256 buckets was 281 `openat` calls, more than half of the command's time, and took
/// several times as long as a listing of one directory, while 16 buckets cost no more
/// than one. With 16, the wait for a bucket's lock with 128 or 512 clones starting at once
/// was a small fraction of what one directory and the host-wide name lock cost. The count
/// is due for another look when the start rate is several times higher.
pub fn bucket_of(key: &str) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(key.as_bytes())[0] >> 4)
}

/// Whether `name` is a bucket directory's name.
fn is_bucket(name: &str) -> bool {
    name.len() == 1 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// How often a claim sweeps the state directory for what SIGKILLed VMs left
/// (`StateManager::maybe_sweep`).
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// The prefix of an index directory being made, before it is renamed into place
/// (`StateManager::index_dir`).
const INDEX_STAGING_PREFIX: &str = ".index-staging.";

/// Hand the directory `dir`, just made by root, to the owner of `state_dir`. Through a
/// descriptor opened without following links, so a link planted at the path cannot have
/// something else handed away. A refusal (a fuse-pipe mapped store refuses chown) is
/// logged: the owner's next write then fails with a clear EACCES.
fn hand_to_state_dir_owner(state_dir: &Path, dir: &Path) {
    use nix::fcntl::OFlag;
    use std::os::unix::fs::MetadataExt;
    let handed = std::fs::metadata(state_dir)
        .map_err(anyhow::Error::from)
        .and_then(|owner| {
            let fd = nix::fcntl::open(
                dir,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                nix::sys::stat::Mode::empty(),
            )?;
            nix::unistd::fchown(
                &fd,
                Some(nix::unistd::Uid::from_raw(owner.uid())),
                Some(nix::unistd::Gid::from_raw(owner.gid())),
            )?;
            Ok(())
        });
    if let Err(error) = handed {
        tracing::warn!(
            dir = %dir.display(),
            %error,
            "cannot hand an index directory to the state directory's owner"
        );
    }
}

/// Publish the state written at `temp` as `state`, atomically: a reader of `state` gets the
/// state before this write or the one after it, whole.
///
/// When `state` exists the two files are exchanged and the old state, now at `temp`, is
/// unlinked. A plain rename over an existing file would publish the same way, but btrfs
/// answers it by starting the write-out of the new file's data inside the rename ("we're
/// using rename to replace one file with another, start IO on it now"), with the directory
/// locked. A state is rewritten four times in a start, and with many clones starting
/// together each of those renames slept in the block layer, which held the writes back
/// behind the VMs' own disk reads. An exchange starts no I/O:
/// the data goes out with the filesystem's periodic writeback, or never, when the state is
/// replaced or its VM exits first.
///
/// A kill between the exchange and the unlink leaves the new state published and the old
/// one at `temp`. No reader opens that name; the next write truncates it, `delete_state`
/// removes it, and the sweep removes it with the state of a VM whose owner is gone. A
/// filesystem without `RENAME_EXCHANGE` gets the plain rename.
///
/// Nothing here is synced: a state is rewritten within seconds for as long as its VM runs,
/// and no VM outlives a host crash. What a crash can leave is a state file that is empty or
/// cut short, which the sweep removes once it predates the boot (`cleanup_stale_state`).
fn publish_state(temp: &Path, state: &Path) -> std::io::Result<()> {
    let exchanged = exchange_or_rename(temp, state, |temp, state| {
        rename_with_flags(temp, state, libc::RENAME_EXCHANGE)
    })?;
    if exchanged {
        if let Err(error) = remove_if_exists(temp) {
            tracing::warn!(
                path = %temp.display(),
                %error,
                "cannot remove the state a write replaced; the next write overwrites it"
            );
        }
    }
    Ok(())
}

/// The first half of `publish_state`: `exchange` the two names, or rename `temp` onto
/// `state` when there is no state yet (a VM's first write) or the filesystem has no
/// exchange. `Ok(true)` when the names were exchanged, so `temp` now holds the state that
/// was replaced. `exchange` is `RENAME_EXCHANGE`; a test passes one that fails.
fn exchange_or_rename(
    temp: &Path,
    state: &Path,
    exchange: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    match exchange(temp, state) {
        Ok(()) => Ok(true),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOENT)
                    | Some(libc::EINVAL)
                    | Some(libc::ENOSYS)
                    | Some(libc::EOPNOTSUPP)
            ) =>
        {
            std::fs::rename(temp, state).map(|()| false)
        }
        Err(error) => Err(error),
    }
}

/// `rename(from, to)` that fails with EEXIST instead of replacing what is at `to`.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    rename_with_flags(from, to, libc::RENAME_NOREPLACE)
}

/// `renameat2(from, to, flags)`.
fn rename_with_flags(from: &Path, to: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both are NUL-terminated paths that outlive the call.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The token of the VM that holds a name: the one file in the name's slot. Its file name
/// is `<pid>.<start>.<vm_id>`, the owner's PID and start time and the VM, so who holds the
/// name, and whether that process is still running, is read from the directory alone. A
/// token a claimer was killed while writing identifies its owner all the same. The content
/// repeats the name for whoever inspects the store by hand; nothing reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NameToken {
    pid: u32,
    start: u64,
    vm_id: String,
}

impl NameToken {
    fn file_name(&self) -> String {
        format!("{}.{}.{}", self.pid, self.start, self.vm_id)
    }

    fn parse(file_name: &str) -> Option<Self> {
        let mut parts = file_name.splitn(3, '.');
        let pid = parts.next()?.parse().ok()?;
        let start = parts.next()?.parse().ok()?;
        let vm_id = parts.next().filter(|vm_id| !vm_id.is_empty())?.to_string();
        Some(Self { pid, start, vm_id })
    }

    /// Whether the owner is running: `Ok(false)` only when it is known to be gone.
    fn owner_liveness(&self) -> std::io::Result<bool> {
        process_liveness(self.pid, Some(self.start))
    }
}

/// The name of `name`'s slot in its bucket.
fn slot_name(name: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(name.as_bytes()))
}

/// Remove a claim directory and the token in it. Best effort: what a failure leaves names
/// its owner, and the sweep removes it once that process is gone.
fn remove_claim_dir(claim: &Path) {
    if let Ok(entries) = std::fs::read_dir(claim) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let _ = std::fs::remove_dir(claim);
}

/// The loopback addresses this process's VMs lease, by vm_id, each an abstract Unix socket
/// bound to `LOOPBACK_LEASE_PREFIX` + the address. Released by `delete_state`, and by the
/// kernel when the process exits however it exits.
static LOOPBACK_LEASES: std::sync::Mutex<Vec<(String, std::os::unix::net::UnixDatagram)>> =
    std::sync::Mutex::new(Vec::new());

/// Lease `ip` on this host: bind its abstract socket, or `None` when another process holds it.
///
/// A loopback address belongs to the network namespace: every fcvm process in it, whatever
/// its data directory and whatever user runs it, publishes ports on the same 127.0.0.0/8.
/// The abstract socket namespace has the same scope, takes no file permissions, so a root
/// VM and a user's VM see each other's leases, and is released by the kernel when the
/// process that bound it dies.
fn lease_loopback_ip(ip: &str) -> Result<Option<std::os::unix::net::UnixDatagram>> {
    use std::os::linux::net::SocketAddrExt;
    let name = format!("{}{ip}", lease_prefix());
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
        .with_context(|| format!("naming the lease of loopback address {ip}"))?;
    match std::os::unix::net::UnixDatagram::bind_addr(&addr) {
        Ok(socket) => Ok(Some(socket)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Ok(None),
        Err(e) => Err(e).with_context(|| format!("leasing loopback address {ip}")),
    }
}

/// The prefix of every lease name: `LOOPBACK_LEASE_PREFIX`. A unit test that must contend
/// with nothing but its own child process sets a private one (`TEST_LEASE_PREFIX`, and
/// `FCVM_TEST_LEASE_PREFIX` for the child); no other build reads either.
fn lease_prefix() -> String {
    #[cfg(test)]
    {
        let private = TEST_LEASE_PREFIX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .or_else(|| std::env::var("FCVM_TEST_LEASE_PREFIX").ok());
        if let Some(prefix) = private {
            return prefix;
        }
    }
    LOOPBACK_LEASE_PREFIX.to_string()
}

#[cfg(test)]
static TEST_LEASE_PREFIX: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Open or create a lock file with world-readable/writable permissions.
/// Uses fchmod after creation to ensure permissions are set regardless of umask.
/// This allows both root and non-root processes to coordinate via flock.
///
/// Never truncated: a lock file holds nothing, and an O_TRUNC open takes the inode lock for
/// writing and changes the file. Every start and stop of a VM opened the host-wide lock of
/// an earlier layout that way, and at the teardown of 1000 clones that queued 1,075 waiters
/// for 6 s each.
fn open_lock_file(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o666)
        .open(path)?;
    // Force permissions regardless of umask (only effective if we own the file or are root)
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o666));
    Ok(file)
}

fn lock_state_file(path: &Path, arg: FlockArg) -> Result<Flock<std::fs::File>> {
    lock_opened_state_file(path, open_lock_file(path)?, arg)
}

fn lock_opened_state_file(
    path: &Path,
    mut file: std::fs::File,
    arg: FlockArg,
) -> Result<Flock<std::fs::File>> {
    use std::os::unix::fs::MetadataExt;

    loop {
        let guard = match Flock::lock(file, nonblocking(arg)) {
            Ok(guard) => guard,
            Err((file, nix::errno::Errno::EWOULDBLOCK)) if nonblocking(arg) != arg => {
                failpoint::note("lock.wait", path.display());
                Flock::lock(file, arg)
                    .map_err(|(_, error)| error)
                    .context("acquiring state lock")?
            }
            Err((_, error)) => return Err(error).context("acquiring state lock"),
        };
        let held = guard
            .metadata()
            .context("reading held state lock identity")?;
        let current = match std::fs::metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("reading state lock path identity"),
        };
        if current.is_some_and(|current| current.dev() == held.dev() && current.ino() == held.ino())
        {
            return Ok(guard);
        }
        // Cleanup can unlink a lock after this waiter opened it. No payload
        // may be touched until we hold the inode now published at the path.
        drop(guard);
        file = open_lock_file(path)?;
    }
}

/// Lock a state file's existing lock, never creating one. `Ok(None)` when there is no lock
/// at `path`: every saved state has one until `delete_state` removes it, so its absence
/// means the state is gone. A reader that created the lock instead would leave a lock for a
/// deleted state behind.
fn lock_existing_state_file(path: &Path, arg: FlockArg) -> Result<Option<Flock<std::fs::File>>> {
    use std::os::unix::fs::MetadataExt;
    loop {
        let file = match std::fs::OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening state lock"),
        };
        let guard = match Flock::lock(file, nonblocking(arg)) {
            Ok(guard) => guard,
            Err((file, nix::errno::Errno::EWOULDBLOCK)) if nonblocking(arg) != arg => {
                failpoint::note("lock.wait", path.display());
                Flock::lock(file, arg)
                    .map_err(|(_, error)| error)
                    .context("acquiring state lock")?
            }
            Err((_, error)) => return Err(error).context("acquiring state lock"),
        };
        let held = guard
            .metadata()
            .context("reading held state lock identity")?;
        match std::fs::metadata(path) {
            Ok(current) if current.dev() == held.dev() && current.ino() == held.ino() => {
                return Ok(Some(guard))
            }
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("reading state lock path identity"),
        }
    }
}

/// The non-blocking form of a blocking lock request, so a wait can be recorded first.
fn nonblocking(arg: FlockArg) -> FlockArg {
    match arg {
        FlockArg::LockShared => FlockArg::LockSharedNonblock,
        FlockArg::LockExclusive => FlockArg::LockExclusiveNonblock,
        other => other,
    }
}

pub(crate) fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

use crate::utils::process_start_time;

/// Check whether the process recorded in a state file is still the same
/// process that wrote it. Returns false when the state records a start time
/// and the process currently at that PID has a different one (PID reuse by an
/// unrelated process). States without a recorded start time fall back to a
/// PID-only match.
fn pid_identity_matches(state: &VmState) -> bool {
    match (state.pid, state.pid_start_time) {
        (Some(pid), Some(recorded)) => process_start_time(pid) == Some(recorded),
        _ => true,
    }
}

/// Whether the process a state file names is still running and is still the process
/// that wrote it: its PID exists, has not exited (a zombie has), and has the recorded
/// start time. A state without a PID names no process, and every state this fcvm
/// writes has one from its first save (`StateManager::claim_name`).
pub fn owner_is_live(state: &VmState) -> bool {
    match state.pid {
        Some(pid) => process_is_live(pid, state.pid_start_time),
        None => false,
    }
}

/// Whether `pid` is running (not exited, not a zombie) and, when `start_time` is given, is
/// the process that started then and not a later one with a reused PID. A process whose
/// liveness cannot be read counts as running: nothing may be deleted, and no name taken,
/// on the strength of a read that failed.
fn process_is_live(pid: u32, start_time: Option<u64>) -> bool {
    process_liveness(pid, start_time).unwrap_or(true)
}

#[cfg(test)]
thread_local! {
    /// An errno every read of a process's stat fails with on this thread, for tests.
    static PROC_STAT_ERROR: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

fn read_proc_stat(pid: u32) -> std::io::Result<String> {
    #[cfg(test)]
    if let Some(errno) = PROC_STAT_ERROR.with(|error| error.get()) {
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
}

/// `process_is_live`, telling "gone" from "could not be read". `Ok(false)` only when
/// /proc says there is no such process, it has exited, or its start time is another's.
/// Any other failure to read it (out of descriptors, no permission) is an error: the
/// process may well be running.
///
/// /proc is this process's own, so the answer holds for processes of its PID namespace. A
/// state directory is used from one PID namespace: each nesting level has a data directory
/// of its own (`paths.data_dir`), and what levels share is the content-addressed assets.
fn process_liveness(pid: u32, start_time: Option<u64>) -> std::io::Result<bool> {
    let stat = match read_proc_stat(pid) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(false)
        }
        Err(error) => return Err(error),
    };
    // Fields after the last ')' start at field 3 (state); starttime is field 22.
    let Some((_, after_comm)) = stat.rsplit_once(')') else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("/proc/{pid}/stat has no command name"),
        ));
    };
    let mut fields = after_comm.split_whitespace();
    if matches!(fields.next(), None | Some("Z") | Some("X")) {
        return Ok(false);
    }
    Ok(match start_time {
        Some(recorded) => fields.nth(18).and_then(|t| t.parse::<u64>().ok()) == Some(recorded),
        None => true,
    })
}

/// When this boot began, from /proc/stat's `btime`.
fn boot_time() -> Option<std::time::SystemTime> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let seconds = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
}

/// Whether `path` was last written before this boot. No VM outlives a boot, so a state
/// file that old belongs to none that is running. False when either time cannot be read.
fn written_before_this_boot(path: &Path) -> bool {
    match (
        std::fs::metadata(path).and_then(|metadata| metadata.modified()),
        boot_time(),
    ) {
        (Ok(modified), Some(boot)) => modified < boot,
        _ => false,
    }
}

/// Warn that `path` does not parse as a state, once per path in this process: a state a
/// host crash cut short stays until the sweep removes it, and every listing reads it.
fn warn_unparsable_state(path: &Path, error: &dyn std::fmt::Display, context: &str) {
    static WARNED: std::sync::Mutex<Option<std::collections::HashSet<PathBuf>>> =
        std::sync::Mutex::new(None);
    let first = WARNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(Default::default)
        .insert(path.to_path_buf());
    if first {
        tracing::warn!(path = %path.display(), %error, "{context}: failed to parse state file");
    } else {
        tracing::debug!(path = %path.display(), %error, "{context}: failed to parse state file");
    }
}

/// What the state directory's own entries say about its layout (`StateManager::scan_layout`).
#[derive(Default)]
struct LayoutScan {
    /// The content of the `layout` file, when there is one.
    marker: Option<String>,
    /// Layout 1: `<vm_id>.json` in the state directory itself.
    old_states: Vec<PathBuf>,
    /// Layout 1: the states' lock and temporary files.
    old_files: Vec<PathBuf>,
    /// Layout 1: `name/` and `pid/`.
    old_dirs: Vec<PathBuf>,
}

impl LayoutScan {
    fn has_old_entries(&self) -> bool {
        !(self.old_states.is_empty() && self.old_files.is_empty() && self.old_dirs.is_empty())
    }
}

/// Manages VM state persistence
///
/// PID Tracking Note:
/// The `pid` field in VmState stores the fcvm process PID (from std::process::id()),
/// NOT the Firecracker child process PID. This allows external tools and monitors
/// to track the fcvm management process that controls the VM lifecycle.
pub struct StateManager {
    state_dir: PathBuf,
    /// Set once this manager has found nothing in the state directory that a reader must
    /// refuse (`check_layout_for_read`).
    layout_readable: std::sync::OnceLock<()>,
    /// Set once this manager has found the state directory marked with this build's
    /// layout and free of another's files (`check_layout_for_write`).
    layout_written: std::sync::OnceLock<()>,
}

/// The most threads a listing reads on.
const LIST_THREADS: usize = 8;

/// The fewest items a listing thread is started for.
const LIST_CHUNK: usize = 32;

/// `work` over `items`, on up to `LIST_THREADS` threads when there are enough of them,
/// with the results in the items' order. A listing opens and reads every bucket and then
/// every state file, which is most of what `fcvm ls` costs with a thousand VMs up.
fn on_listing_threads<T: Send, R: Send>(items: Vec<T>, work: impl Fn(T) -> R + Sync) -> Vec<R> {
    let cores = std::thread::available_parallelism().map_or(1, |cores| cores.get());
    let threads = LIST_THREADS.min(cores).min(items.len() / LIST_CHUNK).max(1);
    if threads == 1 {
        return items.into_iter().map(work).collect();
    }
    let per_thread = items.len().div_ceil(threads);
    let mut items = items.into_iter();
    let mut chunks = Vec::with_capacity(threads);
    loop {
        let chunk: Vec<T> = items.by_ref().take(per_thread).collect();
        if chunk.is_empty() {
            break;
        }
        chunks.push(chunk);
    }
    let work = &work;
    std::thread::scope(|scope| {
        let running: Vec<_> = chunks
            .into_iter()
            .map(|chunk| scope.spawn(move || chunk.into_iter().map(work).collect::<Vec<R>>()))
            .collect();
        running
            .into_iter()
            .flat_map(|thread| thread.join().expect("a listing thread panicked"))
            .collect()
    })
}

/// The entries of every bucket of `vms_dir`, as paths. The buckets are opened and read on
/// blocking threads, with `after_open` between the two: a test seam for the window a
/// rename has to land in to be missed by a btrfs scan (`list_vms_after_open`). Every bucket
/// is open before the first is read, so a state that existed when the scan began is in a
/// directory this scan already holds.
async fn scan_vm_buckets(
    vms_dir: &Path,
    after_open: impl FnOnce() + Send + 'static,
) -> Result<Vec<PathBuf>> {
    let vms_dir = vms_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<Vec<PathBuf>> {
        let mut buckets = Vec::new();
        match std::fs::read_dir(&vms_dir) {
            Ok(entries) => {
                for bucket in entries {
                    let bucket = bucket?;
                    if bucket.file_name().to_str().is_some_and(is_bucket) {
                        buckets.push(bucket.path());
                    }
                }
            }
            // No VM was ever saved here.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let open = on_listing_threads(buckets, |bucket| match std::fs::read_dir(bucket) {
            Ok(entries) => Ok(Some(entries)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        })
        .into_iter()
        .collect::<std::io::Result<Vec<_>>>()?;
        after_open();
        let read = on_listing_threads(open.into_iter().flatten().collect(), |entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<PathBuf>>>()
        });
        let mut paths = Vec::new();
        for bucket in read {
            paths.extend(bucket?);
        }
        Ok(paths)
    })
    .await
    .context("scanning state directory")?
    .context("reading state directory")
}

impl StateManager {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            layout_readable: std::sync::OnceLock::new(),
            layout_written: std::sync::OnceLock::new(),
        }
    }

    /// Initialize state directory
    pub async fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir)
            .await
            .context("creating state directory")?;
        self.check_layout_for_write()
    }

    /// The bucket directory of VM `vm_id`'s state, lock and temporary file.
    fn vm_bucket(&self, vm_id: &str) -> PathBuf {
        self.state_dir.join(VMS_DIR).join(bucket_of(vm_id))
    }

    /// Where VM `vm_id`'s state file is, whether or not it exists. For a caller that
    /// watches the file; everything else goes through `load_state` and `update_state`.
    pub fn state_file(&self, vm_id: &str) -> PathBuf {
        self.vm_bucket(vm_id).join(format!("{vm_id}.json"))
    }

    fn temp_file(&self, vm_id: &str) -> PathBuf {
        self.vm_bucket(vm_id).join(format!("{vm_id}.json.tmp"))
    }

    /// Where VM `vm_id`'s lock file is: made before its first save, kept until its state
    /// is deleted, never renamed.
    #[doc(hidden)]
    pub fn lock_file(&self, vm_id: &str) -> PathBuf {
        self.vm_bucket(vm_id).join(format!("{vm_id}.json.lock"))
    }

    /// The slot of `name`: the directory that holds the token of the VM that has the name.
    #[doc(hidden)]
    pub fn name_slot(&self, name: &str) -> PathBuf {
        self.state_dir
            .join(NAMES_DIR)
            .join(bucket_of(name))
            .join(slot_name(name))
    }

    /// The state directory's own entries, sorted into this layout's marker and what
    /// layout 1 keeps there. `Ok(None)` when there is no state directory.
    fn scan_layout(&self) -> Result<Option<LayoutScan>> {
        let entries = match std::fs::read_dir(&self.state_dir) {
            Ok(entries) => entries,
            // Nothing was ever written here; the first write makes it.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading the state directory {}", self.state_dir.display())
                })
            }
        };
        let mut scan = LayoutScan::default();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name == LAYOUT_FILE {
                let marker = std::fs::read_to_string(entry.path()).with_context(|| {
                    format!("reading the layout marker {}", entry.path().display())
                })?;
                scan.marker = Some(marker);
            } else if name.ends_with(".json") {
                scan.old_states.push(entry.path());
            } else if name.ends_with(".json.lock") || name.ends_with(".json.tmp") {
                scan.old_files.push(entry.path());
            } else if name == "name" || name == "pid" {
                scan.old_dirs.push(entry.path());
            }
        }
        Ok(Some(scan))
    }

    /// Fail when the state directory is not one this build may use as `scan` found it: its
    /// marker names another layout (or is empty or cut short), or a VM of layout 1 is
    /// running in it.
    ///
    /// A layout 1 store keeps each VM's state as `<vm_id>.json` in the state directory. The
    /// fcvm process that owns such a VM keeps writing that path, and claims names under
    /// `names.lock`, so this build cannot take the VM over: moving its state would leave
    /// two copies, each written by one of the two builds. While any of them is running the
    /// store is refused, and the error names them. An older build pointed at this layout
    /// sees no VMs and cannot tell.
    fn refuse_other_layouts(&self, scan: &LayoutScan) -> Result<()> {
        if let Some(marker) = &scan.marker {
            if marker.trim_end_matches('\n') != LAYOUT {
                bail!(
                    "the state directory {} is marked as layout {:?}, and this fcvm reads and \
                     writes layout {LAYOUT:?} only; use the fcvm that wrote it",
                    self.state_dir.display(),
                    marker.trim_end_matches('\n')
                );
            }
        }
        let mut running = Vec::new();
        for path in &scan.old_states {
            let Some(state) = std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            else {
                continue;
            };
            let pid = state
                .get("pid")
                .and_then(|pid| pid.as_u64())
                .and_then(|pid| u32::try_from(pid).ok());
            let start = state.get("pid_start_time").and_then(|start| start.as_u64());
            // One whose liveness cannot be read counts as running.
            if let Some(pid) = pid.filter(|pid| process_is_live(*pid, start)) {
                let name = state
                    .get("name")
                    .and_then(|name| name.as_str())
                    .unwrap_or("(unnamed)");
                running.push(format!("'{name}' (pid {pid})"));
            }
        }
        if !running.is_empty() {
            running.sort();
            bail!(
                "the state directory {} holds {} running VM(s) started by an older fcvm, whose \
                 state this fcvm does not read or write: {}. Stop them with the fcvm binary \
                 that started them, then run this command again",
                self.state_dir.display(),
                running.len(),
                running.join(", ")
            );
        }
        Ok(())
    }

    /// The layout check of everything that reads or changes states that exist: refuse or
    /// proceed, and write nothing. A command that only reads (`fcvm ls`) works without
    /// write access to the state directory, and removes nothing an older build's launch is
    /// in the middle of writing. What layout 1 left with no VM running is not a state of
    /// this layout and is simply not listed; a writer removes it
    /// (`check_layout_for_write`). Once per manager.
    fn check_layout_for_read(&self) -> Result<()> {
        if self.layout_readable.get().is_some() {
            return Ok(());
        }
        let Some(scan) = self.scan_layout()? else {
            return Ok(());
        };
        self.refuse_other_layouts(&scan)?;
        let _ = self.layout_readable.set(());
        Ok(())
    }

    /// The layout check of everything that creates a state (`init`, `save_state`,
    /// `claim_name`): refuse as a reader does, then leave the state directory marked with
    /// this layout and free of layout 1's files. Once per manager, and one read of the
    /// directory's own entries when it is already so.
    ///
    /// Layout 1's files are removed only under layout 1's own `names.lock`, held
    /// exclusively across a second scan and the removal. A launch of the older build holds
    /// that lock from before it writes its name entry until its state is in place, so it is
    /// either finished, and its running VM refuses the store, or it has not begun, and
    /// then writes into a store this build refuses from then on. Nothing of a launch in
    /// progress is removed. The lock file itself stays: unlinking it would give the next
    /// conversion a lock that excludes nobody.
    fn check_layout_for_write(&self) -> Result<()> {
        if self.layout_written.get().is_some() {
            return Ok(());
        }
        let Some(mut scan) = self.scan_layout()? else {
            return Ok(());
        };
        self.refuse_other_layouts(&scan)?;
        let mut _names = None;
        if scan.has_old_entries() {
            let file = open_lock_file(&self.state_dir.join("names.lock"))
                .context("opening the older layout's name lock")?;
            _names = Some(
                Flock::lock(file, FlockArg::LockExclusive)
                    .map_err(|(_, error)| error)
                    .context("acquiring the older layout's name lock")?,
            );
            // What an older launch that held the lock wrote meanwhile.
            scan = self
                .scan_layout()?
                .context("the state directory vanished during the layout check")?;
            self.refuse_other_layouts(&scan)?;
            for path in scan.old_states.iter().chain(&scan.old_files) {
                remove_if_exists(path)
                    .with_context(|| format!("removing {} of an older layout", path.display()))?;
            }
            for path in &scan.old_dirs {
                match std::fs::remove_dir_all(path) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(error).with_context(|| {
                            format!("removing {} of an older layout", path.display())
                        })
                    }
                    _ => {}
                }
            }
        }
        if scan.marker.is_none() {
            // Under a name of its own and renamed, so no reader finds it half written.
            let temp = self.state_dir.join(format!(
                "{INDEX_STAGING_PREFIX}{LAYOUT_FILE}.{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::write(&temp, format!("{LAYOUT}\n"))
                .and_then(|()| std::fs::rename(&temp, self.state_dir.join(LAYOUT_FILE)))
                .with_context(|| {
                    let _ = std::fs::remove_file(&temp);
                    format!("marking the layout of {}", self.state_dir.display())
                })?;
        }
        let _ = self.layout_written.set(());
        let _ = self.layout_readable.set(());
        Ok(())
    }

    /// Save VM state atomically (write to temp file, then rename)
    /// Uses file locking to prevent concurrent writes
    ///
    /// This overwrites the entire state file with the caller's copy. It is
    /// intended for initial creation and for saves where the in-memory state
    /// is authoritative (single writer). To change individual fields after the
    /// VM is running (when the health monitor or another process may also be
    /// writing), use `update_state` so concurrent updates are not clobbered.
    ///
    /// It reads no other VM's state, so its cost does not grow with the number of VMs. A
    /// state another VM left with the same PID (that process died and the OS reused its
    /// PID) is told apart by its start time (`pid_identity_matches`) and removed by the
    /// periodic sweep (`maybe_sweep`).
    ///
    /// With a PID, the state is also entered in the PID index (`pid/<pid>.<vm_id>`), under
    /// the same lock, which is what lets `load_state_by_pid` find it without a scan.
    pub async fn save_state(&self, state: &VmState) -> Result<()> {
        tracing::debug!(
            vm_id = %state.vm_id,
            pid = ?state.pid,
            state_dir = %self.state_dir.display(),
            "save_state: starting save"
        );

        self.check_layout_for_write()?;
        self.bucket_dir(VMS_DIR, &bucket_of(&state.vm_id))
            .context("making the VM's state bucket")?;
        let state_file = self.state_file(&state.vm_id);
        let temp_file = self.temp_file(&state.vm_id);
        let lock_file = self.lock_file(&state.vm_id);

        // Acquire exclusive lock (blocks if another process has lock).
        // NOTE: Flock::lock() is technically blocking I/O in an async context, but
        // the lock is held for microseconds with near-zero contention (only this
        // process writes its own state file). Using spawn_blocking would add more
        // overhead than the lock itself. If contention becomes an issue, switch to
        // FlockArg::LockExclusiveNonblock with retry + tokio::task::yield_now().
        let flock = lock_state_file(&lock_file, FlockArg::LockExclusive)
            .context("acquiring exclusive lock on state file")?;

        // Now we have exclusive access, perform the write
        let result = async {
            // Update last_updated timestamp before saving
            let mut state = state.clone();
            state.last_updated = jiff::Timestamp::now();
            // Record the start time of the process at `pid` so later lookups
            // can detect PID reuse by an unrelated process (see pid_identity_matches).
            state.pid_start_time = state.pid.and_then(process_start_time);

            let state_json = serde_json::to_string_pretty(&state)?;

            // Write to temp file first
            fs::write(&temp_file, &state_json)
                .await
                .context("writing temp state file")?;

            // Set file permissions to 0644 (world-readable) so non-root can list VMs
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(0o644);
                tokio::fs::set_permissions(&temp_file, permissions)
                    .await
                    .context("setting file permissions on state file")?;
            }

            // Published whole: by exchange with the state it replaces, or by rename when
            // this is the VM's first (`publish_state`).
            publish_state(&temp_file, &state_file).context("renaming temp state file")?;

            // The state is published; the index only speeds up `load_state_by_pid`, which
            // scans when it finds nothing, so a failure to index is logged and the save
            // stands.
            if let Some(pid) = state.pid {
                if let Err(error) = self.enter_pid_index(pid, &state.vm_id) {
                    tracing::warn!(
                        vm_id = %state.vm_id,
                        pid,
                        error = format!("{error:#}"),
                        "save_state: cannot enter the VM in the PID index; lookups by PID scan"
                    );
                }
            }

            tracing::debug!(
                vm_id = %state.vm_id,
                pid = ?state.pid,
                path = %state_file.display(),
                "save_state: successfully saved state"
            );

            Ok::<(), anyhow::Error>(())
        }
        .await;

        // Release lock (happens automatically when flock is dropped, but being explicit)
        // NOTE: We intentionally do NOT delete lock files - see allocate_loopback_ip comment
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock on state file")?;

        result
    }

    /// Write a new VM's first state, under its name, while no other live VM has that name.
    ///
    /// A name in use is a directory, its slot (`names/<b>/<sha256(name)>`), holding one
    /// file, the token of the VM that has it. A claim builds a directory of its own with its
    /// token inside and renames it onto the slot. The kernel lets a directory replace
    /// another only when the other is absent or empty, so of any number of launches with one
    /// name exactly one rename succeeds, with no lock. That holds for racing claimers of a
    /// free slot whatever mount each reaches the store through, where a flock, granted per
    /// kernel, does not.
    ///
    /// Whether the holder of a taken slot is still running is read from the claimer's own
    /// /proc (`process_liveness`), so a state directory is used from one PID namespace: a
    /// claimer in another would take a running holder for a dead one. Each nesting level
    /// has its own data directory (`paths.data_dir`), which is what keeps it so.
    ///
    /// A slot that is taken says who has it in the token's file name: the owner's PID and
    /// start time. A live owner keeps the name. A dead one's state is deleted here, so
    /// lookups by name never see it beside the new VM, and its token is unlinked by its own
    /// name, which cannot remove a token another claimer put there since; the rename is then
    /// tried again. A VM holds its name from the rename until `delete_state` unlinks its
    /// token or its process exits, so a SIGKILLed VM frees its name without any cleanup.
    ///
    /// The state is written after the name is held, with this process's PID and start time.
    /// Nothing here reads another VM's state unless that VM holds the name and is dead, so
    /// the cost does not grow with the number of VMs.
    pub async fn claim_name(&self, vm_state: &mut VmState) -> Result<()> {
        self.claim_name_with_hooks(vm_state, || {}, || {}).await
    }

    /// `claim_name` with a seam for tests: `before_rename` runs once the claim directory
    /// and its token exist, before the rename onto the slot.
    #[doc(hidden)]
    pub async fn claim_name_before_rename(
        &self,
        vm_state: &mut VmState,
        before_rename: impl FnOnce(),
    ) -> Result<()> {
        self.claim_name_with_hooks(vm_state, before_rename, || {})
            .await
    }

    /// `claim_name` with both of its test seams: `before_rename` as in
    /// `claim_name_before_rename`, and `after_rename` once the name is held and before the
    /// state is written.
    #[doc(hidden)]
    pub async fn claim_name_with_hooks(
        &self,
        vm_state: &mut VmState,
        before_rename: impl FnOnce(),
        after_rename: impl FnOnce(),
    ) -> Result<()> {
        self.check_layout_for_write()?;
        let name = vm_state
            .name
            .clone()
            .context("a VM must have a name before it claims one")?;
        let pid = std::process::id();
        let start = process_start_time(pid)
            .context("reading this process's start time, which its name token carries")?;
        let token = NameToken {
            pid,
            start,
            vm_id: vm_state.vm_id.clone(),
        };
        let bucket = self
            .bucket_dir(NAMES_DIR, &bucket_of(&name))
            .context("making the name's bucket")?;
        let slot = bucket.join(slot_name(&name));
        let claim = bucket.join(format!("{CLAIM_PREFIX}{}", token.file_name()));

        let held = self
            .rename_claim_onto_slot(&name, &token, &claim, &slot, before_rename)
            .await;
        if held.is_err() {
            remove_claim_dir(&claim);
        }
        held?;
        after_rename();

        vm_state.pid = Some(pid);
        vm_state.pid_start_time = Some(start);
        if let Err(error) = self.save_state(vm_state).await {
            // The name is held by a VM with no state; give it back.
            let _ = remove_if_exists(&slot.join(token.file_name()));
            let _ = std::fs::remove_dir(&slot);
            return Err(error);
        }

        self.maybe_sweep().await;
        Ok(())
    }

    /// Build `claim` with `token` in it and rename it onto `slot`, removing a dead holder
    /// first. `Ok` means `slot` holds `token`.
    async fn rename_claim_onto_slot(
        &self,
        name: &str,
        token: &NameToken,
        claim: &Path,
        slot: &Path,
        before_rename: impl FnOnce(),
    ) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::create_dir(claim) {
            Ok(()) => {}
            // This process's own, from an earlier attempt: no other process has this
            // PID, start time and vm_id.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("making the claim directory {}", claim.display()))
            }
        }
        if nix::unistd::geteuid().is_root() {
            // The state directory's owner must be able to remove the token once this
            // process is gone, as root can remove the owner's.
            hand_to_state_dir_owner(&self.state_dir, claim);
        }
        let token_path = claim.join(token.file_name());
        let content = serde_json::json!({
            "name": name,
            "vm_id": token.vm_id,
            "pid": token.pid,
            "pid_start_time": token.start,
        });
        std::fs::write(&token_path, serde_json::to_vec(&content)?)
            .and_then(|()| {
                std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o644))
            })
            .with_context(|| format!("writing the name token {}", token_path.display()))?;
        before_rename();

        for _ in 0..CLAIM_ATTEMPTS {
            match std::fs::rename(claim, slot) {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ENOTEMPTY) | Some(libc::EEXIST)
                    ) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("renaming the claim onto the name slot {}", slot.display())
                    })
                }
            }
            let holders: Vec<std::ffi::OsString> = match std::fs::read_dir(slot) {
                Ok(entries) => entries
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<std::io::Result<_>>()
                    .with_context(|| format!("reading the name slot {}", slot.display()))?,
                // Released between the rename and this read.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading the name slot {}", slot.display()))
                }
            };
            for file_name in holders {
                let Some(holder) = file_name.to_str().and_then(NameToken::parse) else {
                    bail!(
                        "the name slot {} holds {:?}, which is not a name token",
                        slot.display(),
                        file_name
                    );
                };
                if holder == *token {
                    // An earlier attempt of this claim already holds the slot.
                    remove_claim_dir(claim);
                    return Ok(());
                }
                if holder.vm_id != token.vm_id {
                    let live = |pid: u32, vm_id: &str| {
                        anyhow::anyhow!(
                            "a live VM is already named '{}' (pid {}, vm_id {}); VM names \
                             are unique among running VMs, so stop that VM or choose \
                             another --name",
                            name,
                            pid,
                            vm_id
                        )
                    };
                    // Only a holder known to be gone gives up its name: a /proc read
                    // that failed says nothing, and the claim fails.
                    match holder.owner_liveness() {
                        Ok(false) => {}
                        Ok(true) => return Err(live(holder.pid, &holder.vm_id)),
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!(
                                    "cannot tell whether pid {} (vm_id {}), which holds the \
                                     name '{}', is still running",
                                    holder.pid, holder.vm_id, name
                                )
                            })
                        }
                    }
                    // The dead holder's state goes with its name, so a lookup by name
                    // never finds it beside the new VM. Only the state that still names
                    // this name and whose own owner is gone: the token says who claimed
                    // the name, not what the state file under that vm_id holds now.
                    match self.load_state(&holder.vm_id).await {
                        Ok(state) if state.name.as_deref() == Some(name) => {
                            if owner_is_live(&state) {
                                return Err(live(state.pid.unwrap_or(holder.pid), &state.vm_id));
                            }
                            self.delete_state(&holder.vm_id).await.with_context(|| {
                                format!(
                                    "deleting the state of exited VM {} that was named '{}'",
                                    holder.vm_id, name
                                )
                            })?;
                        }
                        // Gone, renamed, or unreadable: it holds no name.
                        Ok(_) | Err(_) => {}
                    }
                }
                // A dead holder's token, or this VM's own from another process of it. By
                // its own name: a token put there since has another.
                remove_if_exists(&slot.join(&file_name)).with_context(|| {
                    format!("removing the name token of exited VM {}", holder.vm_id)
                })?;
            }
        }
        bail!(
            "could not take the name '{name}': its slot was taken again {CLAIM_ATTEMPTS} times \
             while this claim removed exited holders"
        )
    }

    /// Give back the name `state` holds: unlink this VM's token, by its own name, and
    /// remove the slot if that left it empty. A claim that renamed onto the slot in between
    /// keeps it, because a directory with a token in it is not removed. The token is found
    /// by the vm_id in its file name, so a state that lost its PID still gives its name
    /// back.
    fn release_name(&self, state: &VmState) {
        let Some(name) = state.name.as_deref() else {
            return;
        };
        let slot = self.name_slot(name);
        let Ok(tokens) = std::fs::read_dir(&slot) else {
            return;
        };
        for token in tokens.flatten() {
            let mine = token
                .file_name()
                .to_str()
                .and_then(NameToken::parse)
                .is_some_and(|token| token.vm_id == state.vm_id);
            if mine {
                if let Err(error) = remove_if_exists(&token.path()) {
                    tracing::warn!(
                        vm_id = %state.vm_id,
                        slot = %slot.display(),
                        %error,
                        "cannot remove the VM's name token; the sweep removes it once its \
                         owner is gone"
                    );
                }
            }
        }
        let _ = std::fs::remove_dir(&slot);
    }

    /// The PID index entry of VM `vm_id`, owned by process `pid`.
    fn pid_entry(&self, pid: u32, vm_id: &str) -> PathBuf {
        self.state_dir
            .join(PIDS_DIR)
            .join(bucket_of(&pid.to_string()))
            .join(format!("{pid}.{vm_id}"))
    }

    /// Enter VM `vm_id` of process `pid` in the PID index. An entry that exists is never
    /// reopened: it may be another user's file.
    fn enter_pid_index(&self, pid: u32, vm_id: &str) -> Result<()> {
        let entry = self.pid_entry(pid, vm_id);
        self.bucket_dir(PIDS_DIR, &bucket_of(&pid.to_string()))?;
        if std::fs::symlink_metadata(&entry).is_err() {
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&entry)
                .with_context(|| format!("entering {} in the PID index", entry.display()))?;
        }
        Ok(())
    }

    /// The bucket `bucket` of `kind` (`VMS_DIR`, `NAMES_DIR` or `PIDS_DIR`), made when
    /// missing along with `kind` itself.
    fn bucket_dir(&self, kind: &str, bucket: &str) -> Result<PathBuf> {
        let dir = self.state_dir.join(kind).join(bucket);
        // The common case, one stat: the bucket exists.
        if std::fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.is_dir()) {
            return Ok(dir);
        }
        self.shared_dir(&self.state_dir, kind)?;
        self.shared_dir(&self.state_dir.join(kind), bucket)
    }

    /// The directory `name` in `parent`, made when missing.
    ///
    /// Root and the state directory's owner share one state directory (CI runs the
    /// privileged tests as root beside the unprivileged ones in one data directory), and
    /// every launch writes into these directories, so they must stay writable by the
    /// owner whoever made them. One made by root is handed to the owner of the state
    /// directory before it appears at its path: it is made under a name of its own, given
    /// away, and renamed into place without replacing one another process made first. No
    /// other file here is handed away, and nothing is made world-writable.
    fn shared_dir(&self, parent: &Path, name: &str) -> Result<PathBuf> {
        let dir = parent.join(name);
        match std::fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => return Ok(dir),
            Ok(_) => bail!("{} is not a directory", dir.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", dir.display()))
            }
        }
        let staging = parent.join(format!(
            "{INDEX_STAGING_PREFIX}{name}.{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&staging)
            .with_context(|| format!("making the directory {}", dir.display()))?;
        if nix::unistd::geteuid().is_root() {
            hand_to_state_dir_owner(&self.state_dir, &staging);
        }
        let published = rename_noreplace(&staging, &dir);
        // Gone after a rename; left when another process published the directory first.
        let _ = std::fs::remove_dir(&staging);
        match published {
            Ok(()) => Ok(dir),
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => Ok(dir),
            Err(error) => Err(error).with_context(|| format!("publishing {}", dir.display())),
        }
    }

    /// Sweep the state directory (`cleanup_stale_state`) at most once a `SWEEP_EVERY` per
    /// state directory, from the one launch that wins `sweep.lock`, after its claim has
    /// released the name lock. A claim no longer scans every state, so this is what removes the states
    /// SIGKILLed VMs leave and the index entries that outlive them; until it runs they
    /// hold nothing (a dead owner is not live, and a stale index entry names a state that
    /// is gone). Best effort: a sweep that cannot take its lock leaves it to the next.
    async fn maybe_sweep(&self) {
        let stamp = self.state_dir.join("sweep.stamp");
        let fresh = |stamp: &Path| {
            std::fs::metadata(stamp)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age < SWEEP_EVERY)
        };
        if fresh(&stamp) {
            return;
        }
        let Ok(file) = open_lock_file(&self.state_dir.join("sweep.lock")) else {
            return;
        };
        let Ok(_guard) = Flock::lock(file, FlockArg::LockExclusiveNonblock) else {
            return;
        };
        if fresh(&stamp) {
            return;
        }
        // Opened like a lock file (mode 0666), so root and the state directory's owner can
        // each rewrite a stamp the other wrote.
        let now = jiff::Timestamp::now().to_string();
        let written = open_lock_file(&stamp).and_then(|mut file| {
            use std::io::Write;
            file.set_len(0)?;
            file.write_all(now.as_bytes()).map_err(anyhow::Error::from)
        });
        if let Err(error) = written {
            tracing::warn!(%error, "maybe_sweep: cannot write the sweep stamp");
            return;
        }
        self.cleanup_stale_state().await;
    }

    /// Load VM state
    pub async fn load_state(&self, vm_id: &str) -> Result<VmState> {
        self.check_layout_for_read()?;
        let state_file = self.state_file(vm_id);
        let state_json = fs::read_to_string(&state_file)
            .await
            .context("reading VM state")?;
        let state: VmState = serde_json::from_str(&state_json).context("parsing VM state")?;
        Ok(state)
    }

    /// Delete VM state and associated lock/temp files
    ///
    /// Holds the same per-VM lock used by `save_state`/`update_state` while
    /// removing the state file. A concurrent locked read-modify-write (e.g.
    /// the health monitor's `update_health_status`) therefore either completes
    /// before the deletion (and its result is removed), or acquires the lock
    /// afterwards, finds no state file, and becomes a no-op — it can never
    /// resurrect the state file of a deleted VM.
    ///
    /// The VM's PID index entry goes with its state, under that lock. Its name is given
    /// back after the lock is released (`release_name`), which takes no lock: a lookup by
    /// name never finds the name free while the VM is still listed.
    pub async fn delete_state(&self, vm_id: &str) -> Result<()> {
        self.check_layout_for_read()?;
        LOOPBACK_LEASES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(id, _)| id != vm_id);
        let state_file = self.state_file(vm_id);
        let lock_file = self.lock_file(vm_id);
        let temp_file = self.temp_file(vm_id);
        if !self.vm_bucket(vm_id).is_dir() {
            // No state was ever saved in this bucket, so this VM has none, and no lock to
            // retire.
            return Ok(());
        }

        // Who the VM is, for its index entries. A VM's name and PID are set by its claim
        // and never change, so this read needs no lock. With no readable state there is
        // nothing to look up by; the sweep removes any entry left behind.
        let known = fs::read_to_string(&state_file)
            .await
            .ok()
            .and_then(|json| serde_json::from_str::<VmState>(&json).ok());
        tracing::debug!(
            vm_id = vm_id,
            path = %state_file.display(),
            "delete_state: deleting state file"
        );

        // Acquire the per-VM lock so deletion is serialized against in-flight
        // locked writes (save_state / update_state / update_health_status).
        let flock = lock_state_file(&lock_file, FlockArg::LockExclusive)
            .context("acquiring exclusive lock for state delete")?;

        let result = async {
            // Delete state file - ignore NotFound (concurrent cleanup)
            match fs::remove_file(&state_file).await {
                Ok(()) => {
                    tracing::debug!(
                        vm_id = vm_id,
                        path = %state_file.display(),
                        "delete_state: successfully deleted state file"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        vm_id = vm_id,
                        path = %state_file.display(),
                        "delete_state: state file already gone (NotFound)"
                    );
                }
                Err(e) => return Err(e).context("deleting VM state"),
            }

            remove_if_exists(&temp_file).context("deleting temporary VM state")?;
            // Independent of what follows: a failure is logged and the sweep retries it,
            // since the lock and the name entry must still go.
            if let Some(pid) = known.as_ref().and_then(|state| state.pid) {
                let entry = self.pid_entry(pid, vm_id);
                if let Err(error) = remove_if_exists(&entry) {
                    tracing::warn!(
                        vm_id,
                        path = %entry.display(),
                        %error,
                        "delete_state: cannot remove the VM's PID index entry"
                    );
                }
            }
            // Retire the lock while still holding it, after the last payload
            // operation. Waiters must validate their inode before proceeding.
            remove_if_exists(&lock_file).context("deleting VM state lock")?;

            Ok(())
        }
        .await;

        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock after state delete")?;
        result?;

        // The state is gone, so the VM gives its name back. After the state, so a lookup
        // by name never finds the name free while the VM is still listed.
        if let Some(state) = known.as_ref() {
            self.release_name(state);
        }
        Ok(())
    }

    /// Clean up stale state files from processes that no longer exist.
    ///
    /// This frees up loopback IPs that were allocated but not properly cleaned up
    /// (e.g., due to crashes or SIGKILL). Called lazily during IP allocation.
    async fn cleanup_stale_state(&self) {
        tracing::debug!(
            state_dir = %self.state_dir.display(),
            "cleanup_stale_state: starting scan"
        );

        let mut examined = 0;
        let mut removed = 0;

        let mut seen = std::collections::HashSet::new();
        'entries: for entry in self.bucket_entries(VMS_DIR) {
            let mut path = entry.path();
            if path.extension().is_some_and(|extension| extension == "tmp") {
                path = path.with_extension("");
            }
            if path.extension().is_none_or(|extension| extension != "json")
                || !seen.insert(path.clone())
            {
                continue;
            }
            let lock_path = path.with_extension("json.lock");
            let _guard = match lock_state_file(&lock_path, FlockArg::LockExclusiveNonblock) {
                Ok(guard) => guard,
                Err(error) => {
                    if error.downcast_ref::<nix::errno::Errno>()
                        != Some(&nix::errno::Errno::EWOULDBLOCK)
                    {
                        tracing::warn!(path = %path.display(), %error, "cleanup_stale_state: lock failed");
                    }
                    continue;
                }
            };

            // Read identity only after acquiring the writer's lock. A missing
            // sibling makes a temp orphaned; an unreadable sibling proves nothing.
            let stale = match std::fs::read_to_string(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => continue,
                Ok(content) => 'stale: {
                    let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) else {
                        // Empty or cut short: a host crash caught a write. One written
                        // before this boot belongs to no running VM and goes, with its
                        // lock and temporary file here and its PID entry in the index
                        // sweep below. A younger one may be a VM's and stays.
                        if written_before_this_boot(&path) {
                            break 'stale true;
                        }
                        continue 'entries;
                    };
                    let Some(pid) = state
                        .get("pid")
                        .and_then(|p| p.as_u64())
                        .and_then(|p| u32::try_from(p).ok())
                    else {
                        continue 'entries;
                    };
                    let Ok(exists) = Path::new(&format!("/proc/{pid}")).try_exists() else {
                        continue 'entries;
                    };
                    let replaced = state
                        .get("pid_start_time")
                        .and_then(|t| t.as_u64())
                        .zip(process_start_time(pid))
                        .is_some_and(|(recorded, current)| recorded != current);
                    !exists || replaced
                }
            };
            examined += 1;
            if !stale {
                continue;
            }

            let temp_path = path.with_extension("json.tmp");
            // Keep the guard through the final unlink. If removing either
            // payload fails, retain the lock and retry on a later sweep.
            let result = [&path, &temp_path, &lock_path]
                .into_iter()
                .try_for_each(|path| remove_if_exists(path));
            match result {
                Ok(()) => removed += 1,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "cleanup_stale_state: removal failed")
                }
            }
        }

        tracing::debug!(
            examined = examined,
            removed = removed,
            "cleanup_stale_state: scan complete"
        );

        // Last, the index entries whose state is now gone: removed above, or deleted by a
        // VM that died between removing its state and its entries.
        self.sweep_pid_index();
        self.sweep_name_index();
        self.sweep_index_staging();
    }

    /// Remove index directories a process died making (`index_dir`) before it renamed
    /// them into place. Only ones older than a sweep interval: a younger one may be about
    /// to be renamed. They are empty, and `remove_dir` removes nothing else.
    fn sweep_index_staging(&self) {
        let mut dirs = vec![self.state_dir.clone()];
        dirs.extend([VMS_DIR, NAMES_DIR, PIDS_DIR].map(|kind| self.state_dir.join(kind)));
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let is_staging = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(INDEX_STAGING_PREFIX));
                let old = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > SWEEP_EVERY);
                if is_staging && old {
                    // A directory being made, or the layout marker's temporary file.
                    if std::fs::remove_dir(entry.path()).is_err() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    /// The path of every state file there is now, in every bucket. For tests and tools
    /// that inspect the store without loading it; `list_vms` is the listing to act on.
    #[doc(hidden)]
    pub fn state_files(&self) -> Vec<PathBuf> {
        self.bucket_entries(VMS_DIR)
            .into_iter()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect()
    }

    /// The entries of every bucket of `kind`. A bucket that cannot be read is passed over:
    /// the sweeps that use this run again.
    fn bucket_entries(&self, kind: &str) -> Vec<std::fs::DirEntry> {
        let Ok(buckets) = std::fs::read_dir(self.state_dir.join(kind)) else {
            return Vec::new();
        };
        buckets
            .flatten()
            .filter(|bucket| bucket.file_name().to_str().is_some_and(is_bucket))
            .filter_map(|bucket| std::fs::read_dir(bucket.path()).ok())
            .flat_map(|entries| entries.flatten())
            .collect()
    }

    /// Remove PID index entries whose state is gone or names another PID. Each is checked
    /// and removed under its VM's state lock, the lock `save_state` enters it under, so an
    /// entry of a state being saved is never removed. A lock held by someone else skips
    /// that entry until the next sweep.
    fn sweep_pid_index(&self) {
        for entry in self.bucket_entries(PIDS_DIR) {
            let file_name = entry.file_name();
            let Some((pid, vm_id)) = file_name.to_str().and_then(|name| name.split_once('.'))
            else {
                continue;
            };
            let Ok(pid) = pid.parse::<u32>() else {
                continue;
            };
            let state_path = self.state_file(vm_id);
            let lock_path = self.lock_file(vm_id);
            if !self.vm_bucket(vm_id).is_dir() {
                // No VM of that bucket was ever saved, so there is no state for the entry.
                let _ = remove_if_exists(&entry.path());
                continue;
            }
            let Ok(_guard) = lock_state_file(&lock_path, FlockArg::LockExclusiveNonblock) else {
                continue;
            };
            let current = match std::fs::read_to_string(&state_path) {
                Ok(json) => match serde_json::from_str::<VmState>(&json) {
                    Ok(state) => Some(state.pid),
                    Err(_) => continue,
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => continue,
            };
            match current {
                Some(owner) if owner == Some(pid) => {}
                Some(_) => {
                    let _ = remove_if_exists(&entry.path());
                }
                None => {
                    // No state: the lock is this sweep's own (or an orphan); retire it with
                    // the entry, still under the guard, as `cleanup_stale_state` does.
                    let _ = remove_if_exists(&entry.path());
                    let _ = remove_if_exists(&lock_path);
                }
            }
        }
    }

    /// Remove what dead processes left in the name slots: the token of an owner that is
    /// gone, a slot that is empty, and a claim directory whose builder is gone. Every
    /// removal is one a claim of that name could make itself: a token by its own name, and
    /// a directory only if empty, so a sweep and a claim never undo each other.
    fn sweep_name_index(&self) {
        for entry in self.bucket_entries(NAMES_DIR) {
            let path = entry.path();
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(builder) = name.strip_prefix(CLAIM_PREFIX) {
                if NameToken::parse(builder)
                    .is_some_and(|builder| matches!(builder.owner_liveness(), Ok(false)))
                {
                    remove_claim_dir(&path);
                }
                continue;
            }
            if name.starts_with(INDEX_STAGING_PREFIX) {
                continue;
            }
            let Ok(tokens) = std::fs::read_dir(&path) else {
                continue;
            };
            for token in tokens.flatten() {
                let dead = token
                    .file_name()
                    .to_str()
                    .and_then(NameToken::parse)
                    .is_some_and(|token| matches!(token.owner_liveness(), Ok(false)));
                if dead {
                    let _ = remove_if_exists(&token.path());
                }
            }
            // Fails while a token is in it.
            let _ = std::fs::remove_dir(&path);
        }
    }

    /// Load VM state by name
    ///
    /// A state whose owner is not live does not count when one proven live has the name: a
    /// claim removes the dead holder its name entry names, but a dead state written before
    /// the name index existed has no entry and stays until the next sweep.
    pub async fn load_state_by_name(&self, name: &str) -> Result<VmState> {
        let vms = self.list_vms().await?;
        let mut matches: Vec<_> = vms
            .into_iter()
            .filter(|vm| vm.name.as_deref() == Some(name))
            .collect();
        // Live here means proven live: a recorded start time that matches, which every
        // claimed state has. A PID alone may be anyone's.
        let proven_live = |vm: &VmState| vm.pid_start_time.is_some() && owner_is_live(vm);
        if matches.len() > 1 && matches.iter().filter(|vm| proven_live(vm)).count() == 1 {
            matches.retain(proven_live);
        }
        match matches.len() {
            0 => anyhow::bail!("VM not found: {}", name),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => {
                let pids: Vec<String> = matches
                    .iter()
                    .map(|vm| {
                        vm.pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "none".to_string())
                    })
                    .collect();
                anyhow::bail!(
                    "Multiple VMs named '{}' ({}). Use --pid to specify: {}",
                    name,
                    n,
                    pids.join(", ")
                )
            }
        }
    }

    /// Load VM state by PID
    ///
    /// The PID index (`save_state`) names the states saved with `pid`, so a live VM is
    /// found by reading its own state and nothing else. Only when the index names no state
    /// of the process at `pid` are all states read, as before the index: a state written
    /// without `save_state` is found that way, and so is the sweep it triggers.
    pub async fn load_state_by_pid(&self, pid: u32) -> Result<VmState> {
        self.check_layout_for_read()?;
        tracing::debug!(pid = pid, "load_state_by_pid: searching for VM");

        if let Some(vm) = self.indexed_state_of_pid(pid).await {
            return Ok(vm);
        }

        let vms = self.list_vms().await?;
        let vm_count = vms.len();

        tracing::debug!(
            pid = pid,
            vm_count = vm_count,
            "load_state_by_pid: found {} VMs to search",
            vm_count
        );

        // Log each VM we're checking
        for vm in &vms {
            tracing::trace!(
                search_pid = pid,
                vm_pid = ?vm.pid,
                vm_id = %vm.vm_id,
                vm_name = ?vm.name,
                "load_state_by_pid: checking VM"
            );
        }

        // A state file matching the PID is only trusted if the process at that
        // PID is still the process that wrote it (start time matches). A stale
        // file whose PID was reused by an unrelated process is skipped here and
        // removed by cleanup_stale_state below.
        if let Some(vm) = vms
            .into_iter()
            .find(|vm| vm.pid == Some(pid) && pid_identity_matches(vm))
        {
            tracing::debug!(
                pid = pid,
                vm_id = %vm.vm_id,
                vm_name = ?vm.name,
                "load_state_by_pid: found matching VM"
            );
            return Ok(vm);
        }

        // PID not found. Clean stale state files (dead or replaced PIDs) and
        // retry once. Stale files from killed VMs can shadow the target if the
        // stale PID was reused by the OS — save_state deletes the collision,
        // but cleanup_stale_state handles the general case.
        self.cleanup_stale_state().await;
        let vms = self.list_vms().await?;
        let available_pids: Vec<u32> = vms.iter().filter_map(|v| v.pid).collect();
        if let Some(vm) = vms
            .into_iter()
            .find(|vm| vm.pid == Some(pid) && pid_identity_matches(vm))
        {
            tracing::debug!(
                pid = pid,
                vm_id = %vm.vm_id,
                "load_state_by_pid: found VM after stale cleanup"
            );
            return Ok(vm);
        }

        // Still not found after cleanup

        tracing::error!(
            search_pid = pid,
            available_pids = ?available_pids,
            state_dir = %self.state_dir.display(),
            "load_state_by_pid: VM not found - no state file has this PID"
        );
        Err(anyhow::anyhow!("No VM found with PID: {}", pid))
    }

    /// The state the PID index names for `pid` whose process is still the one that wrote
    /// it, the lowest vm_id of them as the scan would pick. `None` when there is none.
    async fn indexed_state_of_pid(&self, pid: u32) -> Option<VmState> {
        self.indexed_states_of_pid(pid)
            .await
            .into_iter()
            .find(pid_identity_matches)
    }

    /// Every state the PID index names for `pid` that still says `pid`, in vm_id order,
    /// whichever process wrote it. This reads the index directory and those states; no
    /// other VM's state is opened. Empty when the index names none: a state written
    /// without `save_state`, or one `save_state` could not enter, is not in it.
    pub async fn indexed_states_of_pid(&self, pid: u32) -> Vec<VmState> {
        if self.check_layout_for_read().is_err() {
            return Vec::new();
        }
        let prefix = format!("{pid}.");
        let bucket = self
            .state_dir
            .join(PIDS_DIR)
            .join(bucket_of(&pid.to_string()));
        let Ok(entries) = std::fs::read_dir(bucket) else {
            return Vec::new();
        };
        let mut vm_ids: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_prefix(&prefix))
                    .map(str::to_string)
            })
            .collect();
        vm_ids.sort();
        let mut states = Vec::new();
        for vm_id in vm_ids {
            if let Ok(state) = self.load_state(&vm_id).await {
                if state.pid == Some(pid) {
                    states.push(state);
                }
            }
        }
        states
    }

    /// List all VMs
    pub async fn list_vms(&self) -> Result<Vec<VmState>> {
        self.list_vms_after_open(|| {}).await
    }

    /// `list_vms` with a seam for tests: `after_open` runs once the state directory is
    /// open and before any of its entries are read, which is the window a state-file
    /// rename has to land in to be missed by a btrfs directory scan.
    #[doc(hidden)]
    pub async fn list_vms_after_open(
        &self,
        after_open: impl FnOnce() + Send + 'static,
    ) -> Result<Vec<VmState>> {
        self.check_layout_for_read()?;
        let mut vms = Vec::new();

        if !self.state_dir.exists() {
            tracing::trace!(
                state_dir = %self.state_dir.display(),
                "list_vms: state directory does not exist"
            );
            return Ok(vms);
        }

        tracing::trace!(
            state_dir = %self.state_dir.display(),
            "list_vms: scanning directory"
        );

        let paths = scan_vm_buckets(&self.state_dir.join(VMS_DIR), after_open).await?;

        // A state file is replaced by rename on every update. On btrfs a scan that
        // overlaps the rename can miss the name: since Linux 6.5 btrfs readdir stops at
        // the last directory index that existed when the scan began, and the rename
        // gives the file a new, higher index. A VM's lock file (`<vm_id>.json.lock`) is
        // made before its first save, kept until its state is deleted, and never
        // renamed, so every VM that existed when the scan began shows up through its
        // state file, its lock file, or both. Each state file is then read by path,
        // which always finds the current one.
        let mut vm_ids = std::collections::BTreeSet::new();
        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Some(vm_id) = name
                .strip_suffix(".json.lock")
                .or_else(|| name.strip_suffix(".json"))
            {
                vm_ids.insert(vm_id.to_string());
            }
        }

        // Read and parsed on blocking threads; reported here, on the caller's own.
        let paths: Vec<PathBuf> = vm_ids.iter().map(|vm_id| self.state_file(vm_id)).collect();
        let read = tokio::task::spawn_blocking(move || {
            on_listing_threads(paths, |path| {
                let state = std::fs::read_to_string(&path)
                    .map(|json| serde_json::from_str::<VmState>(&json));
                (path, state)
            })
        })
        .await
        .context("reading state files")?;
        for (path, state) in read {
            match state {
                Ok(Ok(state)) => {
                    tracing::trace!(
                        path = %path.display(),
                        vm_id = %state.vm_id,
                        pid = ?state.pid,
                        "list_vms: parsed state file"
                    );
                    vms.push(state);
                }
                Ok(Err(e)) => warn_unparsable_state(&path, &e, "list_vms"),
                // A VM that exits deletes its state file before its lock file, and a new
                // VM makes its lock file before its state file, so a name from the scan
                // can have no state file by the time it is read. Not an error.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        path = %path.display(),
                        "list_vms: no state file for this name (VM starting or gone)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "list_vms: failed to read state file"
                    );
                }
            }
        }

        tracing::trace!(vm_count = vms.len(), "list_vms: scan complete");

        Ok(vms)
    }

    /// Update VM state atomically by holding the per-VM lock across read-modify-write.
    ///
    /// Loads the current on-disk state, applies `mutate`, and writes the result
    /// back while holding the per-VM flock for the entire operation. This is the
    /// safe way to change individual fields once a VM is running: a whole-state
    /// `save_state` from a stale in-memory copy would silently revert fields
    /// written by other tasks/processes (e.g. the health monitor) in the meantime.
    ///
    /// Returns `Ok(None)` without writing anything if the state file does not
    /// exist (e.g. the VM was deleted concurrently by `delete_state`), so a
    /// late update cannot resurrect a deleted VM's state file.
    pub async fn update_state<F>(&self, vm_id: &str, mutate: F) -> Result<Option<VmState>>
    where
        F: FnOnce(&mut VmState),
    {
        self.check_layout_for_read()?;
        let state_file = self.state_file(vm_id);
        let temp_file = self.temp_file(vm_id);
        let lock_file = self.lock_file(vm_id);

        // Fast path: if the state file is already gone (VM deleted), don't
        // recreate a lock file just to discover that under the lock.
        if !state_file.exists() {
            tracing::debug!(
                vm_id = vm_id,
                path = %state_file.display(),
                "update_state: state file does not exist, skipping update"
            );
            return Ok(None);
        }

        // Acquire exclusive lock (blocks if another process has lock)
        let flock = lock_state_file(&lock_file, FlockArg::LockExclusive)
            .context("acquiring exclusive lock for state update")?;

        // CRITICAL: Hold lock across entire read-modify-write
        let result: Result<Option<VmState>> = async {
            // Load current state. The file may have been deleted while we
            // waited for the lock (delete_state holds the same lock) — treat
            // that as "nothing to update" rather than recreating it.
            let state_json = match fs::read_to_string(&state_file).await {
                Ok(json) => json,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // We may have reopened the lock after a deleter retired
                    // its inode. Retire this no-op lock under the same guard.
                    remove_if_exists(&temp_file).context("deleting orphan temporary state")?;
                    remove_if_exists(&lock_file).context("deleting unused state lock")?;
                    tracing::debug!(
                        vm_id = vm_id,
                        path = %state_file.display(),
                        "update_state: state file deleted concurrently, skipping update"
                    );
                    return Ok(None);
                }
                Err(e) => return Err(e).context("reading VM state for update"),
            };
            let mut state: VmState =
                serde_json::from_str(&state_json).context("parsing VM state for update")?;

            // Apply caller's modification
            mutate(&mut state);
            state.last_updated = jiff::Timestamp::now();

            // Write to temp file
            let state_json = serde_json::to_string_pretty(&state)?;
            fs::write(&temp_file, &state_json)
                .await
                .context("writing temp state file for update")?;

            // Set permissions (world-readable so non-root can list VMs)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(0o644);
                tokio::fs::set_permissions(&temp_file, permissions)
                    .await
                    .context("setting file permissions on state file")?;
            }

            // Published whole, by exchange with the state it replaces (`publish_state`).
            publish_state(&temp_file, &state_file)
                .context("renaming temp state file for update")?;

            Ok(Some(state))
        }
        .await;

        // Release lock (held until this point)
        // NOTE: We intentionally do NOT delete lock files - see allocate_loopback_ip comment
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock after state update")?;

        result
    }

    /// Read VM state under the per-VM lock and hand it to `inspect`, still under the lock.
    ///
    /// For a decision another process's locked write must not slip between: `inspect` sees
    /// the state as it is on disk, and no `update_state` of this VM can land until it
    /// returns. Nothing is written, not even a lock. Returns `Ok(None)` when the state, or its
    /// lock, is gone.
    pub async fn with_locked_state<T, F>(&self, vm_id: &str, inspect: F) -> Result<Option<T>>
    where
        F: FnOnce(&VmState) -> T,
    {
        self.check_layout_for_read()?;
        let state_file = self.state_file(vm_id);
        let lock_file = self.lock_file(vm_id);
        let Some(flock) = lock_existing_state_file(&lock_file, FlockArg::LockShared)
            .context("acquiring shared lock to read VM state")?
        else {
            return Ok(None);
        };
        let result: Result<Option<T>> = async {
            let state_json = match fs::read_to_string(&state_file).await {
                Ok(json) => json,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e).context("reading VM state under its lock"),
            };
            let state: VmState =
                serde_json::from_str(&state_json).context("parsing VM state under its lock")?;
            Ok(Some(inspect(&state)))
        }
        .await;
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock after reading VM state")?;
        result
    }

    /// Update health status atomically by holding lock across read-modify-write.
    ///
    /// This prevents the race condition where concurrent health monitor updates
    /// could overwrite each other's changes. The lock is held from load through save.
    ///
    /// # Arguments
    /// * `vm_id` - VM identifier
    /// * `health_status` - New health status to set
    /// * `exit_code` - Optional exit code (for Stopped status)
    ///
    /// # Returns
    /// The previous health status before update, or None if the state file no
    /// longer exists (e.g. the VM was deleted concurrently — nothing is written).
    pub async fn update_health_status(
        &self,
        vm_id: &str,
        health_status: super::HealthStatus,
        exit_code: Option<i32>,
    ) -> Result<Option<super::HealthStatus>> {
        let mut previous_status = None;
        self.update_state(vm_id, |state| {
            previous_status = Some(state.health_status);
            state.health_status = health_status;
            if exit_code.is_some() {
                state.exit_code = exit_code;
            }
        })
        .await?;
        Ok(previous_status)
    }

    /// Record a host-side vsock transport reset for this VM by bumping the
    /// persisted `vsock_epoch` (locked read-modify-write via `update_state`).
    ///
    /// Ordering contract: call this AFTER a snapshot pause/save and BEFORE the
    /// VM is resumed. Exec clients capture the epoch right after their vsock
    /// connection is established — which is only possible against a running
    /// guest — so bumping while the VM is still paused guarantees that every
    /// connection from before the pause observes the change (loud orphan abort
    /// instead of an indefinite hang) and every post-resume connection reads
    /// the already-bumped value (no false abort).
    ///
    /// Returns the new epoch, or `None` if the state file no longer exists
    /// (VM mid-teardown — its exec sessions get a socket error when the
    /// hypervisor exits, so no epoch signal is needed).
    pub async fn bump_vsock_epoch(&self, vm_id: &str) -> Result<Option<u64>> {
        let updated = self
            .update_state(vm_id, |state| {
                state.vsock_epoch += 1;
            })
            .await?;
        Ok(updated.map(|state| state.vsock_epoch))
    }

    /// Allocate a unique loopback IP for rootless networking and persist it with the state
    ///
    /// The address is leased host-wide (`lease_loopback_ip`): a bind of its abstract
    /// socket, which exactly one process wins, held until `delete_state` removes the VM or
    /// the process exits however it exits. Every VM that holds an address holds its lease,
    /// across data directories and users (#1882), so the lease alone decides: no lock, and
    /// no other VM's state is read. The search starts at an address picked from the
    /// vm_id, so with a thousand VMs up a new one tries about one address, not a thousand.
    ///
    /// Returns an IP in the 127.0.0.2 - 127.0.255.254 range.
    ///
    /// # Arguments
    /// * `vm_state` - The VM state to update and persist with the allocated IP
    pub async fn allocate_loopback_ip(&self, vm_state: &mut VmState) -> Result<String> {
        // We don't check if a port is free on the address because wildcard binds
        // (0.0.0.0:8080) would cause false negatives. Real port conflicts are detected at
        // pasta bind time.
        let (ip, lease) = loopback_candidates(&vm_state.vm_id)
            .find_map(|ip| match lease_loopback_ip(&ip) {
                Ok(Some(lease)) => Some(Ok((ip, lease))),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .unwrap_or_else(|| Err(anyhow::anyhow!("all loopback IPs exhausted (64,000+ VMs)")))?;
        LOOPBACK_LEASES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((vm_state.vm_id.clone(), lease));

        vm_state.config.network.loopback_ip = Some(ip.clone());
        self.save_state(vm_state).await?;
        Ok(ip)
    }
}

/// Every loopback address a VM may be given, 127.0.0.2 to 127.0.255.254 skipping each
/// block's .0, .1 and .255, starting from one picked from `vm_id` and wrapping around.
fn loopback_candidates(vm_id: &str) -> impl Iterator<Item = String> {
    const PER_BLOCK: u32 = 253;
    const COUNT: u32 = 256 * PER_BLOCK;
    // FNV-1a: stable across builds and processes, which `DefaultHasher` does not promise.
    let hash = vm_id.bytes().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    let start = hash % COUNT;
    (0..COUNT).map(move |offset| {
        let index = (start + offset) % COUNT;
        format!("127.0.{}.{}", index / PER_BLOCK, 2 + index % PER_BLOCK)
    })
}

#[cfg(test)]
mod tests {

    /// A locked read creates no lock. A state deleted between a reader's look and its
    /// lock used to leave the reader's freshly created lock behind for good; with no lock
    /// at the path the state is gone and the read says so. A saved state is read as usual.
    #[tokio::test]
    async fn a_locked_read_never_creates_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        manager.init().await.unwrap();
        let orphan = VmState::new("vm-orphan".to_string(), "alpine".to_string(), 1, 128);
        write_state_file(dir.path(), &orphan);
        let read = manager
            .with_locked_state("vm-orphan", |state| state.vm_id.clone())
            .await
            .unwrap();
        assert_eq!(read, None);
        assert!(
            !manager.lock_file("vm-orphan").exists(),
            "a locked read created a lock"
        );

        let mut saved = VmState::new("vm-saved".to_string(), "alpine".to_string(), 1, 128);
        saved.pid = Some(std::process::id());
        manager.save_state(&saved).await.unwrap();
        let read = manager
            .with_locked_state("vm-saved", |state| state.vm_id.clone())
            .await
            .unwrap();
        assert_eq!(read.as_deref(), Some("vm-saved"));
    }

    use super::*;

    /// Everything `list_vms` logs at WARN or above while scanning `dir`.
    fn warnings_from_listing(dir: &std::path::Path) -> (usize, String) {
        #[derive(Clone, Default)]
        struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Captured;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let manager = StateManager::new(dir.to_path_buf());
        let listed = tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(manager.list_vms())
                .expect("list_vms")
                .len()
        });
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        (listed, log)
    }

    /// A VM that exits deletes its state file, and another process may be
    /// listing the directory at that moment. That is not worth a warning: the
    /// default log level prints warnings, and `fcvm exec` shares its stderr
    /// with the command it runs.
    #[test]
    fn a_state_file_that_vanishes_during_a_listing_is_not_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        // Listed by read_dir, gone by the time it is read.
        let manager = StateManager::new(dir.path().to_path_buf());
        let gone = manager.state_file("vm-gone");
        std::fs::create_dir_all(gone.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(dir.path().join("deleted"), gone).unwrap();
        let (listed, log) = warnings_from_listing(dir.path());
        assert_eq!(listed, 0);
        assert_eq!(log, "", "a vanished state file was reported");
    }

    /// Control for the test above: the capture does see `list_vms` warn.
    #[test]
    fn an_unparsable_state_file_is_still_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let bad = StateManager::new(dir.path().to_path_buf()).state_file("vm-bad");
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(bad, "not json").unwrap();
        let (listed, log) = warnings_from_listing(dir.path());
        assert_eq!(listed, 0);
        assert!(log.contains("failed to parse state file"), "{log:?}");
    }

    #[test]
    fn state_lock_waiter_rejects_unlinked_inode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.json.lock");
        let holder = lock_state_file(&path, FlockArg::LockExclusive).unwrap();
        // A waiter has opened A but has not acquired its lock when cleanup
        // unlinks it. A new writer then opens and locks B at the same path.
        let waiter = open_lock_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = lock_state_file(&path, FlockArg::LockExclusive).unwrap();
        drop(holder);
        let result = lock_opened_state_file(&path, waiter, FlockArg::LockExclusiveNonblock);
        assert!(
            result.is_err(),
            "waiter acquired retired inode A while B was held"
        );
        assert_eq!(
            result.unwrap_err().downcast_ref::<nix::errno::Errno>(),
            Some(&nix::errno::Errno::EWOULDBLOCK)
        );
        drop(replacement);
        let current = lock_state_file(&path, FlockArg::LockExclusiveNonblock).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            current.metadata().unwrap().ino(),
            std::fs::metadata(path).unwrap().ino()
        );
    }

    fn named_state(vm_id: &str, name: &str) -> VmState {
        let mut state = VmState::new(vm_id.to_string(), "alpine:latest".to_string(), 1, 512);
        state.name = Some(name.to_string());
        state
    }

    /// Write a state file as it is, bypassing `save_state`, which records the start time
    /// of whatever process is at the PID now.
    fn write_state_file(dir: &Path, state: &VmState) {
        let path = StateManager::new(dir.to_path_buf()).state_file(&state.vm_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string(state).unwrap()).unwrap();
    }

    /// The tokens in `name`'s slot, as file names.
    fn tokens_of(manager: &StateManager, name: &str) -> Vec<String> {
        let mut tokens: Vec<String> = std::fs::read_dir(manager.name_slot(name))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        tokens.sort();
        tokens
    }

    /// Put a token in `name`'s slot as a claimer with this identity would have left it:
    /// `content` is what it had written when it was killed.
    fn plant_token(manager: &StateManager, name: &str, token: &NameToken, content: &[u8]) {
        let slot = manager.name_slot(name);
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join(token.file_name()), content).unwrap();
    }

    /// An identity no running process has: a PID above the kernel's PID_MAX_LIMIT.
    fn dead_token(vm_id: &str) -> NameToken {
        NameToken {
            pid: 4_000_000_000,
            start: 1,
            vm_id: vm_id.to_string(),
        }
    }

    /// The bucket of a key is the first hex digit of its SHA-256, in every process and every
    /// build. A hasher keyed per process would send two fcvm processes to two directories
    /// for one VM.
    #[test]
    fn a_key_has_one_bucket_in_every_process() {
        for (key, bucket) in [
            ("vm-1", "2"),
            ("vm-0123456789abcdef0123456789abcdef", "a"),
            ("web", "4"),
            ("4242", "0"),
            ("", "e"),
        ] {
            assert_eq!(bucket_of(key), bucket, "bucket of {key:?}");
        }
        let manager = StateManager::new(PathBuf::from("/s"));
        assert_eq!(
            manager.state_file("vm-1"),
            PathBuf::from("/s/vms/2/vm-1.json")
        );
        assert_eq!(
            manager.lock_file("vm-1"),
            PathBuf::from("/s/vms/2/vm-1.json.lock")
        );
        assert_eq!(
            manager.pid_entry(4242, "vm-1"),
            PathBuf::from("/s/pids/0/4242.vm-1")
        );
        assert_eq!(
            manager.name_slot("web"),
            PathBuf::from(
                "/s/names/4/4b5e57f6eb2f42b9039b3d1e13929295f231749c510cbe341cd68036d9af97e2"
            )
        );
    }

    /// A token's file name is its owner's identity, whatever its content.
    #[test]
    fn a_token_is_read_from_its_file_name() {
        let token = NameToken {
            pid: 71,
            start: 9_000_000_001,
            vm_id: "vm-a.b".to_string(),
        };
        assert_eq!(token.file_name(), "71.9000000001.vm-a.b");
        assert_eq!(NameToken::parse(&token.file_name()), Some(token));
        for not_a_token in [
            "",
            "71",
            "71.9",
            "71.9.",
            "x.9.vm",
            "71.y.vm",
            ".claim.71.9.vm",
        ] {
            assert_eq!(NameToken::parse(not_a_token), None, "{not_a_token:?}");
        }
    }

    #[tokio::test]
    async fn a_live_vm_keeps_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        let mut first = named_state("vm-first", "slot-3");
        manager.claim_name(&mut first).await.unwrap();
        assert_eq!(first.pid, Some(std::process::id()));

        let mut second = named_state("vm-second", "slot-3");
        let error = format!("{:#}", manager.claim_name(&mut second).await.unwrap_err());
        assert!(
            error.contains("'slot-3'")
                && error.contains(&format!("pid {}", std::process::id()))
                && error.contains("vm-first"),
            "the refusal must name the live VM and its pid: {error}"
        );
        assert!(
            !manager.state_file("vm-second").exists(),
            "a refused claim must write no state"
        );
        assert_eq!(
            tokens_of(&manager, "slot-3"),
            vec![format!(
                "{}.{}.vm-first",
                std::process::id(),
                first.pid_start_time.unwrap()
            )],
            "a refused claim must leave the holder's token alone"
        );
        let bucket = manager.name_slot("slot-3");
        let left: Vec<_> = std::fs::read_dir(bucket.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().into_string().unwrap())
            .filter(|name| name.starts_with(CLAIM_PREFIX))
            .collect();
        assert!(left.is_empty(), "a refused claim left {left:?}");
        let mut other = named_state("vm-other", "slot-4");
        manager.claim_name(&mut other).await.unwrap();
    }

    /// Two data directories on one host share its 127.0.0.0/8: a VM of one must not be
    /// handed the loopback address a VM of the other holds (#1882). Before the lease each
    /// manager saw only its own state files, and both returned 127.0.0.2.
    #[tokio::test]
    async fn two_data_directories_get_different_loopback_addresses() {
        let (first_dir, second_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let first_manager = StateManager::new(first_dir.path().to_path_buf());
        let second_manager = StateManager::new(second_dir.path().to_path_buf());
        let mut first = named_state("vm-data-dir-a", "dir-a");
        let mut second = named_state("vm-data-dir-b", "dir-b");
        let first_ip = first_manager
            .allocate_loopback_ip(&mut first)
            .await
            .unwrap();
        let second_ip = second_manager
            .allocate_loopback_ip(&mut second)
            .await
            .unwrap();
        assert_ne!(
            first_ip, second_ip,
            "a VM in another data directory was handed the loopback address {first_ip} a \
             running VM holds"
        );
        first_manager.delete_state("vm-data-dir-a").await.unwrap();
        second_manager.delete_state("vm-data-dir-b").await.unwrap();
    }

    /// Several VMs of one process (`fcvm serve`, the library API) share a PID. Claiming
    /// one must not delete another's state, which holds its name and its loopback IP.
    #[tokio::test]
    async fn vms_of_one_process_keep_their_names_and_loopback_ips() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        let mut first = named_state("vm-first", "slot-1");
        manager.claim_name(&mut first).await.unwrap();
        let first_ip = manager.allocate_loopback_ip(&mut first).await.unwrap();
        let mut second = named_state("vm-second", "slot-2");
        manager.claim_name(&mut second).await.unwrap();
        let second_ip = manager.allocate_loopback_ip(&mut second).await.unwrap();

        assert_ne!(first_ip, second_ip);
        assert_eq!(
            manager
                .load_state("vm-first")
                .await
                .unwrap()
                .name
                .as_deref(),
            Some("slot-1"),
            "claiming slot-2 deleted slot-1's state"
        );
        let mut again = named_state("vm-again", "slot-1");
        assert!(manager.claim_name(&mut again).await.is_err());
    }

    /// A VM whose process exited, or whose PID now belongs to another process, does not
    /// hold its name, and its state is removed so a lookup by name finds one VM. Each holder
    /// claims the name the way a launch does; its token is then renamed to the identity of
    /// a process that is gone, which is what the slot holds once its owner has died, and the
    /// next claim takes the name from it.
    #[tokio::test]
    async fn an_exited_vm_frees_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        type MakeStale = fn(&mut NameToken);
        let cases: [(&str, MakeStale); 2] = [
            // Above the kernel's PID_MAX_LIMIT (4194304), so no process has it.
            ("vm-exited", |token| token.pid = 4_000_000_000),
            ("vm-replaced", |token| token.start += 1),
        ];
        for (vm_id, make_stale) in cases {
            let mut holder = named_state(vm_id, "slot-3");
            manager.claim_name(&mut holder).await.unwrap();
            let live = NameToken {
                pid: holder.pid.unwrap(),
                start: holder.pid_start_time.unwrap(),
                vm_id: vm_id.to_string(),
            };
            let mut stale = live.clone();
            make_stale(&mut stale);
            let slot = manager.name_slot("slot-3");
            std::fs::rename(slot.join(live.file_name()), slot.join(stale.file_name())).unwrap();
            holder.pid = Some(stale.pid);
            holder.pid_start_time = Some(stale.start);
            write_state_file(dir.path(), &holder);
        }

        let mut new = named_state("vm-new", "slot-3");
        manager.claim_name(&mut new).await.unwrap();
        let named: Vec<String> = manager
            .list_vms()
            .await
            .unwrap()
            .into_iter()
            .filter(|vm| vm.name.as_deref() == Some("slot-3"))
            .map(|vm| vm.vm_id)
            .collect();
        assert_eq!(named, vec!["vm-new".to_string()]);
        assert_eq!(
            manager.load_state_by_name("slot-3").await.unwrap().vm_id,
            "vm-new"
        );
        assert_eq!(tokens_of(&manager, "slot-3").len(), 1);
    }

    /// A claimer killed between creating its token and writing it leaves an empty file.
    /// Its file name still says who owned it, so the slot is not wedged: a dead owner's is
    /// taken over, a live owner's keeps the name, and neither needs the content.
    #[tokio::test]
    async fn a_token_left_empty_by_a_kill_still_names_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        plant_token(&manager, "torn", &dead_token("vm-killed"), b"");
        let mut next = named_state("vm-next", "torn");
        manager.claim_name(&mut next).await.unwrap();
        assert_eq!(
            tokens_of(&manager, "torn"),
            vec![format!(
                "{}.{}.vm-next",
                std::process::id(),
                next.pid_start_time.unwrap()
            )]
        );

        let live = NameToken {
            pid: std::process::id(),
            start: process_start_time(std::process::id()).unwrap(),
            vm_id: "vm-writing".to_string(),
        };
        plant_token(&manager, "being-written", &live, b"{\"name\":");
        let mut late = named_state("vm-late", "being-written");
        let error = format!("{:#}", manager.claim_name(&mut late).await.unwrap_err());
        assert!(
            error.contains("vm-writing") && error.contains(&format!("pid {}", live.pid)),
            "{error}"
        );
    }

    /// The steps of a claim a SIGKILL can land after (`claim_holder_child`): the claim
    /// directory and its token built, the name held, the state written.
    const CLAIM_STEPS: [&str; 3] = ["built", "held", "claimed"];

    /// Not a test of its own: the child `a_claim_holder_killed_at_any_step_blocks_nothing`
    /// runs. With `FCVM_TEST_CLAIM_HOLDER_DIR` set it claims the name `held` in that
    /// directory, stops at step `FCVM_TEST_CLAIM_HOLDER_STEP`, prints `READY <ip>` and waits
    /// to be killed; without it, it returns at once.
    #[tokio::test]
    async fn claim_holder_child() {
        let Ok(dir) = std::env::var("FCVM_TEST_CLAIM_HOLDER_DIR") else {
            return;
        };
        let step = std::env::var("FCVM_TEST_CLAIM_HOLDER_STEP").unwrap();
        assert!(
            lease_prefix().starts_with("fcvm-test-"),
            "the holder's leases are not in the test's private namespace"
        );
        let ready = |ip: &str| {
            use std::io::Write;
            println!("READY {ip}");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(600));
        };
        let manager = StateManager::new(PathBuf::from(dir));
        // The address is leased first, by an unnamed VM of the same process, so the holder
        // holds a lease at every step it can be killed at.
        let mut lease = VmState::new("vm-lease".to_string(), "alpine:latest".to_string(), 1, 512);
        let ip = manager.allocate_loopback_ip(&mut lease).await.unwrap();
        let mut holder = named_state("vm-killed", "held");
        manager
            .claim_name_with_hooks(
                &mut holder,
                || {
                    if step == "built" {
                        ready(&ip)
                    }
                },
                || {
                    if step == "held" {
                        ready(&ip)
                    }
                },
            )
            .await
            .unwrap();
        ready(&ip);
    }

    /// A process SIGKILLed after each step of a claim, holding a leased loopback address,
    /// leaves nothing that blocks: the next claim of its name does not wait and succeeds,
    /// the killed VM's state is gone, its address can be leased again, and after a sweep
    /// nothing of it is left among the names.
    ///
    /// The holder and this test lease in a namespace of their own (a lease prefix named
    /// after this process), so between the kill and the re-lease no other test on the host
    /// can take the address: they contend only with each other, on one real socket.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_claim_holder_killed_at_any_step_blocks_nothing() {
        use std::io::BufRead;
        let private = format!("fcvm-test-{}-loopback-", std::process::id());
        *TEST_LEASE_PREFIX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(private.clone());
        assert_eq!(
            lease_prefix(),
            private,
            "this test's leases are not private"
        );
        for step in CLAIM_STEPS {
            let dir = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "state::manager::tests::claim_holder_child",
                    "--nocapture",
                ])
                .env("FCVM_TEST_LEASE_PREFIX", &private)
                .env("FCVM_TEST_CLAIM_HOLDER_DIR", dir.path())
                .env("FCVM_TEST_CLAIM_HOLDER_STEP", step)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
            let held_ip = loop {
                let line = lines
                    .next()
                    .unwrap_or_else(|| panic!("the holder exited before step {step}"))
                    .unwrap();
                if let Some(ip) = line.strip_prefix("READY ") {
                    break ip.to_string();
                }
            };
            child.kill().unwrap();
            child.wait().unwrap();

            let manager = StateManager::new(dir.path().to_path_buf());
            let mut next = named_state("vm-next", "held");
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                manager.claim_name(&mut next),
            )
            .await
            .unwrap_or_else(|_| panic!("a holder killed at step {step} still blocks the claim"))
            .unwrap_or_else(|error| {
                panic!("a holder killed at step {step} still holds the name: {error:#}")
            });
            assert!(
                manager.load_state("vm-killed").await.is_err(),
                "the state of the holder killed at step {step} was kept beside the new VM"
            );
            // What the dead claimer built and did not rename goes with the sweep.
            manager.cleanup_stale_state().await;
            let bucket = manager.name_slot("held");
            let mut left: Vec<String> = std::fs::read_dir(bucket.parent().unwrap())
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name().into_string().unwrap())
                .collect();
            left.sort();
            assert_eq!(
                left,
                vec![slot_name("held")],
                "a holder killed at step {step} left something among the names"
            );
            assert_eq!(
                tokens_of(&manager, "held"),
                vec![format!(
                    "{}.{}.vm-next",
                    std::process::id(),
                    next.pid_start_time.unwrap()
                )],
                "after a holder killed at step {step}"
            );
            // The same vm_id starts its search at the same address, so it is handed the
            // killed holder's address exactly when that lease is free.
            let mut again =
                VmState::new("vm-lease".to_string(), "alpine:latest".to_string(), 1, 512);
            assert_eq!(
                manager.allocate_loopback_ip(&mut again).await.unwrap(),
                held_ip,
                "the address of the holder killed at step {step} is still leased"
            );
            manager.delete_state("vm-lease").await.unwrap();
            manager.delete_state("vm-next").await.unwrap();
            assert!(
                !manager.name_slot("held").exists(),
                "deleting the last holder left its slot"
            );
        }
    }

    /// One claim running inside another's window, at each seam.
    fn claim_on_a_thread(
        state_dir: PathBuf,
        vm_id: &'static str,
        name: &'static str,
    ) -> std::thread::JoinHandle<Result<()>> {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut state = named_state(vm_id, name);
                    StateManager::new(state_dir).claim_name(&mut state).await
                })
        })
    }

    /// Two claims of one name at once, the second run to its end inside the first's window
    /// at each seam. Before the first's rename the second takes the name and the first is
    /// refused; after it the first has the name and the second is refused. Exactly one
    /// succeeds either way, and no lock decides it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn of_two_concurrent_claims_of_one_name_exactly_one_wins() {
        // The second claim runs between the first's token and its rename.
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let manager = StateManager::new(state_dir.clone());
        let mut first = named_state("vm-first", "slot-3");
        let mut second = None;
        let first_result = manager
            .claim_name_before_rename(&mut first, || {
                second = Some(claim_on_a_thread(state_dir, "vm-second", "slot-3").join());
            })
            .await;
        second
            .unwrap()
            .unwrap()
            .expect("the claim that renamed first must have the name");
        let error = format!("{:#}", first_result.unwrap_err());
        assert!(error.contains("vm-second"), "{error}");
        assert_eq!(tokens_of(&manager, "slot-3").len(), 1);
        assert!(
            !manager.state_file("vm-first").exists(),
            "the refused claim wrote a state"
        );
        assert_eq!(
            manager.load_state_by_name("slot-3").await.unwrap().vm_id,
            "vm-second"
        );

        // The second claim runs between the first's rename and its state write.
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let manager = StateManager::new(state_dir.clone());
        let mut first = named_state("vm-first", "slot-3");
        let mut second = None;
        manager
            .claim_name_with_hooks(
                &mut first,
                || {},
                || {
                    second = Some(claim_on_a_thread(state_dir, "vm-second", "slot-3").join());
                },
            )
            .await
            .expect("the claim that renamed first must have the name");
        let error = format!("{:#}", second.unwrap().unwrap().unwrap_err());
        assert!(error.contains("vm-first"), "{error}");
        assert_eq!(tokens_of(&manager, "slot-3").len(), 1);
        assert_eq!(
            manager.load_state_by_name("slot-3").await.unwrap().vm_id,
            "vm-first"
        );
    }

    /// Many claims of one name released together: exactly one has it. Run on a free name
    /// and on one whose holder is dead, where every claimer also removes the dead token.
    #[test]
    fn of_many_claims_released_together_exactly_one_wins() {
        const CLAIMERS: usize = 16;
        for dead_holder in [false, true] {
            for round in 0..20 {
                let dir = tempfile::tempdir().unwrap();
                let manager = StateManager::new(dir.path().to_path_buf());
                if dead_holder {
                    let mut gone = named_state("vm-gone", "contested");
                    let token = dead_token("vm-gone");
                    gone.pid = Some(token.pid);
                    gone.pid_start_time = Some(token.start);
                    write_state_file(dir.path(), &gone);
                    plant_token(&manager, "contested", &token, b"{}");
                }
                let barrier = std::sync::Arc::new(std::sync::Barrier::new(CLAIMERS));
                let claimers: Vec<_> = (0..CLAIMERS)
                    .map(|n| {
                        let (barrier, state_dir) = (barrier.clone(), dir.path().to_path_buf());
                        std::thread::spawn(move || {
                            let runtime = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .unwrap();
                            let mut state = named_state(&format!("vm-{n}"), "contested");
                            barrier.wait();
                            runtime
                                .block_on(StateManager::new(state_dir).claim_name(&mut state))
                                .map(|()| state.vm_id)
                        })
                    })
                    .collect();
                let results: Vec<_> = claimers
                    .into_iter()
                    .map(|claimer| claimer.join().unwrap())
                    .collect();
                let winners: Vec<&String> = results.iter().flatten().collect();
                assert_eq!(
                    winners.len(),
                    1,
                    "round {round}, dead holder {dead_holder}: {results:?}"
                );
                for refused in results.iter().filter_map(|result| result.as_ref().err()) {
                    let error = format!("{refused:#}");
                    assert!(
                        error.contains(winners[0].as_str()),
                        "a refused claim must name the VM that has the name: {error}"
                    );
                }
                let tokens = tokens_of(&manager, "contested");
                assert_eq!(tokens.len(), 1, "{tokens:?}");
                assert!(
                    tokens[0].ends_with(&format!(".{}", winners[0])),
                    "{tokens:?}"
                );
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let named: Vec<String> = runtime
                    .block_on(manager.list_vms())
                    .unwrap()
                    .into_iter()
                    .filter(|vm| vm.name.as_deref() == Some("contested"))
                    .map(|vm| vm.vm_id)
                    .collect();
                assert_eq!(&named, &[winners[0].clone()]);
            }
        }
    }

    /// Deleting a VM gives its name back while another claim of that name runs: the slot
    /// ends holding the new claimer's token and nothing else, or nothing at all when the
    /// claim was refused because the old holder was still there.
    #[test]
    fn a_delete_racing_a_claim_of_its_name_leaves_one_holder_or_none() {
        for round in 0..50 {
            let dir = tempfile::tempdir().unwrap();
            let state_dir = dir.path().to_path_buf();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let manager = StateManager::new(state_dir.clone());
            let mut old = named_state("vm-old", "moving");
            runtime.block_on(manager.claim_name(&mut old)).unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let claimer = {
                let (barrier, state_dir) = (barrier.clone(), state_dir.clone());
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let mut new = named_state("vm-new", "moving");
                    barrier.wait();
                    runtime.block_on(StateManager::new(state_dir).claim_name(&mut new))
                })
            };
            barrier.wait();
            runtime.block_on(manager.delete_state("vm-old")).unwrap();
            let claimed = claimer.join().unwrap();
            let tokens = tokens_of(&manager, "moving");
            match claimed {
                Ok(()) => {
                    assert_eq!(tokens.len(), 1, "round {round}: {tokens:?}");
                    assert!(tokens[0].ends_with(".vm-new"), "round {round}: {tokens:?}");
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    assert!(error.contains("vm-old"), "round {round}: {error}");
                    assert!(tokens.is_empty(), "round {round}: {tokens:?}");
                }
            }
            assert!(
                runtime.block_on(manager.load_state("vm-old")).is_err(),
                "round {round}: the deleted state is back"
            );
        }
    }

    /// The claim takes no file lock for the name: a flock is granted per kernel, and a store
    /// mapped into guests through fuse-pipe is written from several. What decides a claim
    /// is the rename of a directory onto the slot.
    #[test]
    fn a_claim_takes_no_lock_for_the_name() {
        let source = include_str!("manager.rs");
        let body = |function: &str| {
            let start = source
                .find(&format!("    async fn {function}("))
                .or_else(|| source.find(&format!("    pub async fn {function}(")))
                .or_else(|| source.find(&format!("    fn {function}(")))
                .unwrap_or_else(|| panic!("no {function}"));
            let end = source[start..]
                .find("\n    }\n")
                .unwrap_or_else(|| panic!("no end of {function}"));
            &source[start..start + end]
        };
        for function in [
            "claim_name_with_hooks",
            "rename_claim_onto_slot",
            "release_name",
            "sweep_name_index",
        ] {
            let body = body(function);
            for lock in ["Flock", "flock", "lock_state_file", "open_lock_file"] {
                assert!(
                    !body.contains(lock),
                    "{function} uses {lock}, which does not hold across a fuse-pipe mount"
                );
            }
        }
        assert!(
            body("rename_claim_onto_slot").contains("std::fs::rename(claim, slot)"),
            "the claim no longer rests on renaming a directory onto the slot"
        );
    }

    /// A store written by the layout before this one, with a VM of it still running, is
    /// refused by every command: that VM's fcvm keeps writing the old paths, so this build
    /// would neither see its state nor keep its name. The refusal names the VM and its
    /// PID. With no such VM running, what the old layout left is removed.
    #[tokio::test]
    async fn a_store_with_running_vms_of_the_old_layout_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let mut running = named_state("vm-old-running", "old-web");
        running.pid = Some(me);
        running.pid_start_time = process_start_time(me);
        let mut exited = named_state("vm-old-exited", "old-db");
        exited.pid = Some(4_000_000_000);
        exited.pid_start_time = Some(1);
        for state in [&running, &exited] {
            std::fs::write(
                dir.path().join(format!("{}.json", state.vm_id)),
                serde_json::to_string(state).unwrap(),
            )
            .unwrap();
            std::fs::write(dir.path().join(format!("{}.json.lock", state.vm_id)), "").unwrap();
        }
        std::fs::write(dir.path().join("names.lock"), "").unwrap();
        std::fs::create_dir(dir.path().join("name")).unwrap();
        std::fs::write(dir.path().join("name").join("abc.json"), "{}").unwrap();
        std::fs::create_dir(dir.path().join("pid")).unwrap();

        let refusals = [
            format!(
                "{:#}",
                StateManager::new(dir.path().to_path_buf())
                    .list_vms()
                    .await
                    .unwrap_err()
            ),
            format!(
                "{:#}",
                StateManager::new(dir.path().to_path_buf())
                    .init()
                    .await
                    .unwrap_err()
            ),
            format!(
                "{:#}",
                StateManager::new(dir.path().to_path_buf())
                    .claim_name(&mut named_state("vm-new", "new"))
                    .await
                    .unwrap_err()
            ),
            format!(
                "{:#}",
                StateManager::new(dir.path().to_path_buf())
                    .load_state_by_pid(me)
                    .await
                    .unwrap_err()
            ),
        ];
        for refusal in &refusals {
            assert!(
                refusal.contains("'old-web'")
                    && refusal.contains(&format!("pid {me}"))
                    && refusal.contains("older fcvm")
                    && !refusal.contains("old-db"),
                "{refusal}"
            );
        }
        assert!(
            dir.path().join("vm-old-running.json").exists() && dir.path().join("name").exists(),
            "a refused store must be left as it was"
        );

        // The running VM is stopped with the binary that started it, which removes its
        // state; here only its file goes.
        std::fs::remove_file(dir.path().join("vm-old-running.json")).unwrap();
        // A reader lists nothing of the old layout and removes nothing.
        let before = top_level(dir.path());
        let manager = StateManager::new(dir.path().to_path_buf());
        assert!(manager.list_vms().await.unwrap().is_empty());
        assert_eq!(top_level(dir.path()), before, "a listing changed the store");
        // A launch converts: the old layout's files go, its lock file stays.
        let mut new = named_state("vm-new", "old-web");
        manager.claim_name(&mut new).await.unwrap();
        assert_eq!(
            top_level(dir.path()),
            [
                LAYOUT_FILE,
                "names",
                "names.lock",
                "pids",
                "sweep.lock",
                "sweep.stamp",
                "vms"
            ],
            "the old layout's files stay, or its lock file went"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(LAYOUT_FILE)).unwrap(),
            format!("{LAYOUT}\n")
        );
    }

    /// The names in `dir`, sorted.
    fn top_level(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Every path under `dir` with its size, sorted: what a reader must leave as it was.
    fn tree(dir: &Path) -> Vec<(PathBuf, u64)> {
        let mut found = Vec::new();
        let mut dirs = vec![dir.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let metadata = entry.metadata().unwrap();
                if metadata.is_dir() {
                    dirs.push(entry.path());
                    found.push((entry.path(), 0));
                } else {
                    found.push((entry.path(), metadata.len()));
                }
            }
        }
        found.sort();
        found
    }

    /// An older build's launch that has taken `names.lock`, written its name entry and its
    /// state's lock and temporary file, and not yet renamed its state into place. Returns
    /// the held lock.
    fn old_launch_in_progress(dir: &Path) -> Flock<std::fs::File> {
        let names = Flock::lock(
            open_lock_file(&dir.join("names.lock")).unwrap(),
            FlockArg::LockExclusive,
        )
        .unwrap();
        std::fs::create_dir(dir.join("name")).unwrap();
        std::fs::write(
            dir.join("name").join("abc.json"),
            r#"{"name":"old-web","vm_id":"vm-old"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("vm-old.json.lock"), "").unwrap();
        let mut state = named_state("vm-old", "old-web");
        state.pid = Some(std::process::id());
        state.pid_start_time = process_start_time(std::process::id());
        std::fs::write(
            dir.join("vm-old.json.tmp"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
        names
    }

    /// A launch of this build, on a thread of its own: it may have to wait for a lock.
    fn init_on_a_thread(dir: PathBuf) -> std::sync::mpsc::Receiver<Result<()>> {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(StateManager::new(dir).init());
            let _ = done_tx.send(result);
        });
        done_rx
    }

    /// While an older build's launch is between taking `names.lock` and publishing its
    /// state, no command of this build removes anything of it: a reader proceeds and
    /// writes nothing, and a launch waits for the lock and then finds the running VM. The
    /// first layout check removed the older layout's files whenever no state named a
    /// running process, from any command, `fcvm ls` included, and took no lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_older_launch_in_progress_loses_nothing_to_this_build() {
        let dir = tempfile::tempdir().unwrap();
        let names = old_launch_in_progress(dir.path());
        let in_progress = tree(dir.path());

        let reader = StateManager::new(dir.path().to_path_buf());
        assert!(reader.list_vms().await.unwrap().is_empty());
        assert!(reader.load_state("vm-old").await.is_err());
        assert_eq!(tree(dir.path()), in_progress, "a reader changed the store");

        let launch = init_on_a_thread(dir.path().to_path_buf());
        assert!(
            launch
                .recv_timeout(std::time::Duration::from_millis(500))
                .is_err(),
            "a launch did not wait for the older launch that holds names.lock"
        );
        assert_eq!(
            tree(dir.path()),
            in_progress,
            "a launch changed the store before it held names.lock"
        );

        // The older launch publishes its state and releases the lock.
        std::fs::rename(
            dir.path().join("vm-old.json.tmp"),
            dir.path().join("vm-old.json"),
        )
        .unwrap();
        let published = tree(dir.path());
        drop(names);
        let refusal = launch
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the launch never returned")
            .unwrap_err();
        let refusal = format!("{refusal:#}");
        assert!(
            refusal.contains("'old-web'") && refusal.contains("older fcvm"),
            "{refusal}"
        );
        assert_eq!(
            tree(dir.path()),
            published,
            "the refused launch changed the store"
        );
    }

    /// An older launch that died holding `names.lock` (the kernel released it) left files
    /// that belong to no VM: the next launch of this build removes them, under that lock,
    /// and keeps the lock file, so a later older launch and a later conversion still
    /// exclude each other.
    #[tokio::test]
    async fn what_a_dead_older_launch_left_is_removed_by_a_launch() {
        let dir = tempfile::tempdir().unwrap();
        drop(old_launch_in_progress(dir.path()));
        let manager = StateManager::new(dir.path().to_path_buf());
        manager.init().await.unwrap();
        assert_eq!(top_level(dir.path()), [LAYOUT_FILE, "names.lock"]);
        let mut vm = named_state("vm-new", "old-web");
        manager.claim_name(&mut vm).await.unwrap();
    }

    /// A command that only reads writes nothing into the state directory, whatever is in
    /// it, and lists a store it cannot write to.
    #[tokio::test]
    async fn a_reader_writes_nothing_and_needs_no_write_access() {
        use std::os::unix::fs::PermissionsExt;
        let read = |dir: PathBuf| async move {
            let reader = StateManager::new(dir);
            let listed = reader.list_vms().await.unwrap().len();
            let _ = reader.load_state("vm-none").await;
            let _ = reader.with_locked_state("vm-none", |_| ()).await.unwrap();
            assert!(reader.indexed_states_of_pid(1).await.is_empty());
            listed
        };

        // A directory nothing was written to: not even the marker appears.
        let fresh = tempfile::tempdir().unwrap();
        assert_eq!(read(fresh.path().to_path_buf()).await, 0);
        assert!(
            top_level(fresh.path()).is_empty(),
            "a reader marked the store"
        );

        // What a dead older launch left: listed as nothing, left as it is.
        let old = tempfile::tempdir().unwrap();
        drop(old_launch_in_progress(old.path()));
        let before = tree(old.path());
        assert_eq!(read(old.path().to_path_buf()).await, 0);
        assert_eq!(tree(old.path()), before);

        // A store of this layout with a VM in it, with every directory read-only.
        let store = tempfile::tempdir().unwrap();
        let writer = StateManager::new(store.path().to_path_buf());
        let mut vm = named_state("vm-listed", "listed");
        writer.claim_name(&mut vm).await.unwrap();
        let before = tree(store.path());
        let mut dirs: Vec<PathBuf> = before
            .iter()
            .map(|(path, _)| path.clone())
            .filter(|path| path.is_dir())
            .collect();
        dirs.push(store.path().to_path_buf());
        for dir in &dirs {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let listed = read(store.path().to_path_buf()).await;
        for dir in &dirs {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(listed, 1);
        assert_eq!(tree(store.path()), before);
    }

    /// The marker's content is read: this layout's value is accepted, with or without its
    /// newline, and anything else is refused by readers and writers alike, a newer
    /// layout's number and an empty or cut-short file included.
    #[tokio::test]
    async fn a_marker_that_is_not_this_layouts_is_refused() {
        for other in ["3\n", "", "\n", "2x\n", "22", " 2\n", "1\n"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(LAYOUT_FILE), other).unwrap();
            let refusals = [
                format!(
                    "{:#}",
                    StateManager::new(dir.path().to_path_buf())
                        .list_vms()
                        .await
                        .unwrap_err()
                ),
                format!(
                    "{:#}",
                    StateManager::new(dir.path().to_path_buf())
                        .init()
                        .await
                        .unwrap_err()
                ),
                format!(
                    "{:#}",
                    StateManager::new(dir.path().to_path_buf())
                        .claim_name(&mut named_state("vm-new", "new"))
                        .await
                        .unwrap_err()
                ),
            ];
            for refusal in refusals {
                assert!(
                    refusal.contains("is marked as layout")
                        && refusal.contains("layout \"2\" only"),
                    "marker {other:?}: {refusal}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(dir.path().join(LAYOUT_FILE)).unwrap(),
                other,
                "a refused store's marker was rewritten"
            );
            assert_eq!(top_level(dir.path()), [LAYOUT_FILE]);
        }
        for this in ["2\n", "2"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(LAYOUT_FILE), this).unwrap();
            let manager = StateManager::new(dir.path().to_path_buf());
            assert!(manager.list_vms().await.unwrap().is_empty());
            manager.init().await.unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join(LAYOUT_FILE)).unwrap(),
                this
            );
        }
    }

    /// A holder whose /proc entry cannot be read (out of descriptors, no permission) is not
    /// known to be gone: the claim fails, and the holder keeps its name, its token and its
    /// state. Only "no such process" frees a name.
    #[tokio::test]
    async fn a_holder_whose_liveness_cannot_be_read_keeps_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        let mut holder = named_state("vm-holder", "kept");
        manager.claim_name(&mut holder).await.unwrap();
        // One whose owner is gone as well: a failed read must not take that for granted.
        let mut gone = named_state("vm-gone", "kept-too");
        let token = dead_token("vm-gone");
        gone.pid = Some(token.pid);
        gone.pid_start_time = Some(token.start);
        write_state_file(dir.path(), &gone);
        plant_token(&manager, "kept-too", &token, b"{}");

        for errno in [libc::EMFILE, libc::EACCES, libc::EIO] {
            PROC_STAT_ERROR.with(|error| error.set(Some(errno)));
            let live = manager
                .claim_name(&mut named_state("vm-late", "kept"))
                .await;
            let dead = manager
                .claim_name(&mut named_state("vm-later", "kept-too"))
                .await;
            PROC_STAT_ERROR.with(|error| error.set(None));
            for refused in [live, dead] {
                let refusal = format!("{:#}", refused.unwrap_err());
                assert!(
                    refusal.contains("cannot tell whether pid"),
                    "errno {errno}: {refusal}"
                );
            }
            assert_eq!(tokens_of(&manager, "kept").len(), 1);
            assert_eq!(tokens_of(&manager, "kept-too"), [token.file_name()]);
            assert!(manager.load_state("vm-holder").await.is_ok());
            assert!(manager.load_state("vm-gone").await.is_ok());
            assert!(manager.load_state("vm-late").await.is_err());
        }
        // "No such process" still frees the name.
        PROC_STAT_ERROR.with(|error| error.set(Some(libc::ESRCH)));
        let taken = manager
            .claim_name(&mut named_state("vm-later", "kept-too"))
            .await;
        PROC_STAT_ERROR.with(|error| error.set(None));
        taken.unwrap();
    }

    /// The state deleted with a dead holder's name is the one that still carries that name
    /// and whose own owner is gone. A token says who claimed the name; the state under its
    /// vm_id may by now be another's (another name), or have a running owner.
    #[tokio::test]
    async fn a_dead_token_does_not_delete_a_state_that_is_not_its_holders() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        // The state under the dead token's vm_id carries another name and a live owner.
        let mut other = named_state("vm-reused", "db");
        manager.claim_name(&mut other).await.unwrap();
        plant_token(&manager, "web", &dead_token("vm-reused"), b"{}");
        let mut new = named_state("vm-new", "web");
        manager.claim_name(&mut new).await.unwrap();
        assert_eq!(
            manager
                .load_state("vm-reused")
                .await
                .expect("the claim deleted a state that does not hold the name")
                .name
                .as_deref(),
            Some("db")
        );
        assert_eq!(tokens_of(&manager, "web").len(), 1);
        assert_eq!(tokens_of(&manager, "db").len(), 1);

        // The state under the dead token's vm_id carries the name and has a live owner:
        // a live VM has the name, whatever the token says.
        let mut live = named_state("vm-live", "api");
        live.pid = Some(std::process::id());
        live.pid_start_time = process_start_time(std::process::id());
        write_state_file(dir.path(), &live);
        plant_token(&manager, "api", &dead_token("vm-live"), b"{}");
        let refusal = manager
            .claim_name(&mut named_state("vm-late", "api"))
            .await
            .unwrap_err();
        assert!(format!("{refusal:#}").contains("vm-live"), "{refusal:#}");
        assert!(manager.load_state("vm-live").await.is_ok());
    }

    /// A host crash can leave a state file empty or cut short. The sweep removes one
    /// written before this boot, which no running VM can own, with its lock and temporary
    /// file; one written since may be a VM's and stays. Without this an unparsable state
    /// was skipped by every sweep and reported by every listing for good.
    #[tokio::test]
    async fn an_unparsable_state_from_before_this_boot_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        manager.init().await.unwrap();
        let before_boot =
            boot_time().expect("reading the boot time") - std::time::Duration::from_secs(3600);
        for (vm_id, content) in [("vm-cut-old", "{\"vm_id\": \"vm-cu"), ("vm-empty-old", "")] {
            let path = manager.state_file(vm_id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            std::fs::write(manager.temp_file(vm_id), "older").unwrap();
            std::fs::write(manager.lock_file(vm_id), "").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(before_boot)
                .unwrap();
        }
        let young = manager.state_file("vm-cut-young");
        std::fs::create_dir_all(young.parent().unwrap()).unwrap();
        std::fs::write(&young, "{\"vm_id\": \"vm-cu").unwrap();

        manager.cleanup_stale_state().await;
        for vm_id in ["vm-cut-old", "vm-empty-old"] {
            for path in [
                manager.state_file(vm_id),
                manager.temp_file(vm_id),
                manager.lock_file(vm_id),
            ] {
                assert!(!path.exists(), "the sweep left {}", path.display());
            }
        }
        assert!(
            young.exists(),
            "the sweep removed a state written in this boot"
        );
    }

    /// An unparsable state is reported once per process, not by every listing.
    #[test]
    fn an_unparsable_state_is_warned_about_once() {
        let dir = tempfile::tempdir().unwrap();
        let bad = StateManager::new(dir.path().to_path_buf()).state_file("vm-bad-twice");
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(bad, "not json").unwrap();
        let (_, first) = warnings_from_listing(dir.path());
        let (_, second) = warnings_from_listing(dir.path());
        assert_eq!(
            first.matches("failed to parse state file").count(),
            1,
            "{first}"
        );
        assert_eq!(second, "", "the second listing warned again");
    }

    /// A writer killed between the exchange and the unlink leaves the new state published
    /// and the state it replaced at the temporary name. No reader takes that for the
    /// current state; the next write removes it, and so does the sweep once the VM's
    /// owner is gone.
    #[tokio::test]
    async fn a_writer_killed_after_the_exchange_leaves_the_old_state_where_no_reader_looks() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        let mut vm = named_state("vm-swapped", "swapped");
        manager.claim_name(&mut vm).await.unwrap();
        let (state, temp) = (
            manager.state_file("vm-swapped"),
            manager.temp_file("vm-swapped"),
        );
        let old = std::fs::read_to_string(&state).unwrap();
        let mut newer = vm.clone();
        newer.exit_code = Some(7);
        std::fs::write(&temp, serde_json::to_string_pretty(&newer).unwrap()).unwrap();
        let exchanged = exchange_or_rename(&temp, &state, |temp, state| {
            rename_with_flags(temp, state, libc::RENAME_EXCHANGE)
        })
        .unwrap();
        assert!(exchanged);
        assert_eq!(
            std::fs::read_to_string(&temp).unwrap(),
            old,
            "after the exchange the temporary name holds the replaced state"
        );
        let me = std::process::id();
        for (reader, read) in [
            (
                "load_state",
                manager.load_state("vm-swapped").await.unwrap(),
            ),
            (
                "load_state_by_pid",
                manager.load_state_by_pid(me).await.unwrap(),
            ),
            (
                "load_state_by_name",
                manager.load_state_by_name("swapped").await.unwrap(),
            ),
            ("list_vms", manager.list_vms().await.unwrap().remove(0)),
        ] {
            assert_eq!(read.exit_code, Some(7), "{reader} read the replaced state");
        }
        assert_eq!(manager.list_vms().await.unwrap().len(), 1);

        // The next write removes what the killed one left.
        manager
            .update_state("vm-swapped", |state| state.exit_code = Some(8))
            .await
            .unwrap();
        assert!(!temp.exists(), "the next write left the replaced state");
        assert_eq!(
            manager.load_state("vm-swapped").await.unwrap().exit_code,
            Some(8)
        );

        // With its owner gone, the sweep removes the state and the leftover with it.
        let mut dead = manager.load_state("vm-swapped").await.unwrap();
        dead.pid = Some(4_000_000_000);
        dead.pid_start_time = Some(1);
        write_state_file(dir.path(), &dead);
        std::fs::write(&temp, &old).unwrap();
        manager.cleanup_stale_state().await;
        assert!(!state.exists() && !temp.exists() && !manager.lock_file("vm-swapped").exists());
    }

    /// Where the filesystem has no exchange (EINVAL, ENOSYS, EOPNOTSUPP), or there is no
    /// state yet (ENOENT), a state is published by a plain rename. Any other failure of
    /// the exchange fails the write and leaves the published state as it was.
    #[test]
    fn a_state_is_published_by_rename_where_there_is_no_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let (state, temp) = (dir.path().join("vm.json"), dir.path().join("vm.json.tmp"));
        for errno in [libc::EINVAL, libc::ENOSYS, libc::EOPNOTSUPP, libc::ENOENT] {
            std::fs::write(&state, "old").unwrap();
            std::fs::write(&temp, format!("new after {errno}")).unwrap();
            let exchanged = exchange_or_rename(&temp, &state, |_, _| {
                Err(std::io::Error::from_raw_os_error(errno))
            })
            .unwrap();
            assert!(!exchanged, "errno {errno}");
            assert_eq!(
                std::fs::read_to_string(&state).unwrap(),
                format!("new after {errno}")
            );
            assert!(!temp.exists(), "errno {errno}: the temporary file stayed");
        }
        for errno in [libc::EACCES, libc::EIO, libc::ENOSPC] {
            std::fs::write(&state, "old").unwrap();
            std::fs::write(&temp, "new").unwrap();
            let error = exchange_or_rename(&temp, &state, |_, _| {
                Err(std::io::Error::from_raw_os_error(errno))
            })
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
            assert_eq!(std::fs::read_to_string(&state).unwrap(), "old");
        }
        // The real exchange, on this filesystem, and the first write of a state.
        std::fs::write(&state, "old").unwrap();
        std::fs::write(&temp, "new").unwrap();
        publish_state(&temp, &state).unwrap();
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "new");
        assert!(!temp.exists());
        std::fs::remove_file(&state).unwrap();
        std::fs::write(&temp, "first").unwrap();
        publish_state(&temp, &state).unwrap();
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "first");
        assert!(!temp.exists());
    }

    #[test]
    fn a_zombie_is_not_a_live_owner() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let start = crate::utils::process_start_time(pid);
        let mut state = named_state("vm-zombie", "slot-3");
        state.pid = Some(pid);
        state.pid_start_time = start;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let state_field = stat.rsplit_once(')').unwrap().1.split_whitespace().next();
            if state_field == Some("Z") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "`true` did not exit");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !owner_is_live(&state),
            "an exited, unreaped process is not live"
        );
        let mut child = child;
        child.wait().unwrap();

        state.pid = Some(std::process::id());
        state.pid_start_time = crate::utils::process_start_time(std::process::id());
        assert!(owner_is_live(&state), "this test process is live");
    }
}
