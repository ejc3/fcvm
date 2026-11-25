use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
use std::path::{Path, PathBuf};
use tokio::fs;

use super::types::VmState;

/// The abstract socket name that leases loopback address `ip` (`allocate_loopback_ip`).
pub const LOOPBACK_LEASE_PREFIX: &str = "fcvm-loopback-";

/// The name index, under the state directory: one entry per name, recording which VM
/// claimed it last (`StateManager::claim_name`).
const NAME_INDEX_DIR: &str = "name";

/// The PID index, under the state directory: an empty file `<pid>.<vm_id>` per saved state
/// with a PID (`StateManager::save_state`, `StateManager::load_state_by_pid`).
const PID_INDEX_DIR: &str = "pid";

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

/// `rename(from, to)` that fails with EEXIST instead of replacing what is at `to`.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
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
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A name's entry in the name index.
#[derive(serde::Serialize, serde::Deserialize)]
struct NameEntry {
    name: String,
    vm_id: String,
}

/// Read a name entry, `Ok(None)` when there is none. A torn or unparsable entry is an
/// error: entries are only ever published by rename, so one would mean something else
/// wrote there.
fn read_name_entry(path: &Path) -> Result<Option<NameEntry>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parsing the name entry {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("reading the name entry {}", path.display()))
        }
    }
}

/// Publish a name entry by rename, into the name index (`StateManager::index_dir`). The
/// caller holds the name lock, so no other writer uses the temporary name; one a claim
/// that died left behind, maybe another user's (root's, in a user's state directory), is
/// removed first rather than reopened.
fn write_name_entry(path: &Path, entry: &NameEntry) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let temp = path.with_extension("json.tmp");
    remove_if_exists(&temp).context("removing a temporary name entry a claim left")?;
    std::fs::write(&temp, serde_json::to_vec(entry)?).context("writing the name entry")?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644))
        .context("setting the name entry's permissions")?;
    std::fs::rename(&temp, path).context("publishing the name entry")
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
/// writing and changes the file. Every start and stop of a VM opens `names.lock`, so at the
/// teardown of 1000 clones that queued 1,075 waiters for 6 s each.
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
    let Some(pid) = state.pid else {
        return false;
    };
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Fields after the last ')' start at field 3 (state); starttime is field 22.
    let Some((_, after_comm)) = stat.rsplit_once(')') else {
        return false;
    };
    let mut fields = after_comm.split_whitespace();
    if matches!(fields.next(), None | Some("Z") | Some("X")) {
        return false;
    }
    match state.pid_start_time {
        Some(recorded) => fields.nth(18).and_then(|t| t.parse::<u64>().ok()) == Some(recorded),
        None => true,
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
}

/// The state directory's entries, as paths. The directory is opened and read on one
/// blocking thread, with `after_open` between the two: a test seam for the window a
/// rename has to land in to be missed by a btrfs scan (`list_vms_after_open`).
async fn scan_state_dir(
    dir: &Path,
    after_open: impl FnOnce() + Send + 'static,
) -> Result<Vec<PathBuf>> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<Vec<PathBuf>> {
        let entries = std::fs::read_dir(&dir)?;
        after_open();
        entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    })
    .await
    .context("scanning state directory")?
    .context("reading state directory")
}

impl StateManager {
    pub fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    /// Initialize state directory
    pub async fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir)
            .await
            .context("creating state directory")?;
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

        let state_file = self.state_dir.join(format!("{}.json", state.vm_id));
        let temp_file = self.state_dir.join(format!("{}.json.tmp", state.vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", state.vm_id));

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

            // Atomic rename (this is an atomic operation on Unix)
            fs::rename(&temp_file, &state_file)
                .await
                .context("renaming temp state file")?;

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
    /// The check and the write happen under one host-wide lock (`names.lock` in the state
    /// directory), and every launch writes its first state here, so of two launches with
    /// one name exactly one gets it. The state is written with this process's PID and
    /// start time, which is what makes it live to the next claim; it stops being live when
    /// the VM's state is deleted or its process exits, so a SIGKILLed VM frees its name
    /// without any cleanup.
    ///
    /// Which VM claimed a name last is recorded in the name's entry (`name/<sha256>.json`),
    /// written here under the same lock, so the check reads that entry and that VM's state
    /// and nothing else: its cost does not grow with the number of VMs. That VM holds the
    /// name while its state exists, names it, and its owner is live; otherwise its state
    /// is deleted here, so lookups by name never see it beside the new VM. A state written
    /// without a claim has no entry and holds no name.
    pub async fn claim_name(&self, vm_state: &mut VmState) -> Result<()> {
        self.claim_name_with_hooks(vm_state, || {}, || {}).await
    }

    /// `claim_name` with a seam for tests: `after_check` runs once the name is known to be
    /// free and before the state is written, with the name lock held.
    #[doc(hidden)]
    pub async fn claim_name_after_check(
        &self,
        vm_state: &mut VmState,
        after_check: impl FnOnce(),
    ) -> Result<()> {
        self.claim_name_with_hooks(vm_state, after_check, || {})
            .await
    }

    /// `claim_name` with both of its test seams: `after_check` as in
    /// `claim_name_after_check`, and `after_entry` once the name's entry is written and
    /// before the state is, still with the name lock held.
    #[doc(hidden)]
    pub async fn claim_name_with_hooks(
        &self,
        vm_state: &mut VmState,
        after_check: impl FnOnce(),
        after_entry: impl FnOnce(),
    ) -> Result<()> {
        let name = vm_state
            .name
            .clone()
            .context("a VM must have a name before it claims one")?;
        let lock = self.lock_names().await?;

        let entry_path = self.name_entry(&name);
        match read_name_entry(&entry_path)? {
            Some(entry) if entry.name != name => bail!(
                "the name entry {} records '{}', not '{}'",
                entry_path.display(),
                entry.name,
                name
            ),
            Some(entry) if entry.vm_id != vm_state.vm_id => {
                match self.load_state(&entry.vm_id).await {
                    Ok(holder) if holder.name.as_deref() == Some(name.as_str()) => {
                        if owner_is_live(&holder) {
                            bail!(
                                "a live VM is already named '{}' (pid {}, vm_id {}); VM names \
                                 are unique among running VMs, so stop that VM or choose \
                                 another --name",
                                name,
                                holder
                                    .pid
                                    .map_or_else(|| "none".to_string(), |pid| pid.to_string()),
                                holder.vm_id
                            );
                        }
                        self.delete_state_inner(&holder.vm_id, true)
                            .await
                            .with_context(|| {
                                format!(
                                    "deleting the state of exited VM {} that was named '{}'",
                                    holder.vm_id, name
                                )
                            })?;
                    }
                    // Gone, renamed, or unreadable: it holds no name, as a state `list_vms`
                    // cannot parse never did.
                    Ok(_) | Err(_) => {}
                }
            }
            Some(_) | None => {}
        }
        after_check();

        self.index_dir(NAME_INDEX_DIR)?;
        write_name_entry(
            &entry_path,
            &NameEntry {
                name: name.clone(),
                vm_id: vm_state.vm_id.clone(),
            },
        )?;
        after_entry();

        let pid = std::process::id();
        vm_state.pid = Some(pid);
        vm_state.pid_start_time = process_start_time(pid);
        self.save_state(vm_state).await?;

        // Released only after the state is on disk: the next claim must see it.
        lock.unlock()
            .map_err(|(_, error)| error)
            .context("releasing the VM name lock")?;

        self.maybe_sweep().await;
        Ok(())
    }

    /// The host-wide name lock, `names.lock` in the state directory. Every writer of a
    /// name entry holds it, and takes it before any VM's own state lock.
    async fn lock_names(&self) -> Result<Flock<std::fs::File>> {
        let lock_path = self.state_dir.join("names.lock");
        tokio::task::spawn_blocking(move || -> Result<Flock<std::fs::File>> {
            let file = open_lock_file(&lock_path).context("opening the VM name lock")?;
            Flock::lock(file, FlockArg::LockExclusive)
                .map_err(|(_, error)| error)
                .context("acquiring the VM name lock")
        })
        .await
        .context("waiting for the VM name lock")?
    }

    /// The entry of `name`: which VM claimed it last.
    fn name_entry(&self, name: &str) -> PathBuf {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(name.as_bytes());
        self.state_dir
            .join(NAME_INDEX_DIR)
            .join(format!("{}.json", hex::encode(digest)))
    }

    /// The PID index entry of VM `vm_id`, owned by process `pid`.
    fn pid_entry(&self, pid: u32, vm_id: &str) -> PathBuf {
        self.state_dir
            .join(PID_INDEX_DIR)
            .join(format!("{pid}.{vm_id}"))
    }

    /// Enter VM `vm_id` of process `pid` in the PID index. An entry that exists is never
    /// reopened: it may be another user's file.
    fn enter_pid_index(&self, pid: u32, vm_id: &str) -> Result<()> {
        let entry = self.pid_entry(pid, vm_id);
        self.index_dir(PID_INDEX_DIR)?;
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

    /// The index directory `name` in the state directory, made when missing.
    ///
    /// Root and the state directory's owner share one state directory (CI runs the
    /// privileged tests as root beside the unprivileged ones in one data directory), and
    /// every launch writes into both indexes, so the directories must stay writable by the
    /// owner whoever made them. One made by root is handed to the owner of the state
    /// directory before it appears at its path: it is made under a name of its own, given
    /// away, and renamed into place without replacing one another process made first. No
    /// other file here is handed away, and nothing is made world-writable.
    fn index_dir(&self, name: &str) -> Result<PathBuf> {
        let dir = self.state_dir.join(name);
        match std::fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => return Ok(dir),
            Ok(_) => bail!("{} is not a directory", dir.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", dir.display()))
            }
        }
        let staging = self.state_dir.join(format!(
            "{INDEX_STAGING_PREFIX}{name}.{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&staging)
            .with_context(|| format!("making the index directory {}", dir.display()))?;
        if nix::unistd::geteuid().is_root() {
            hand_to_state_dir_owner(&self.state_dir, &staging);
        }
        let published = rename_noreplace(&staging, &dir);
        // Gone after a rename; left when another process published the index first.
        let _ = std::fs::remove_dir(&staging);
        match published {
            Ok(()) => Ok(dir),
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => Ok(dir),
            Err(error) => {
                Err(error).with_context(|| format!("publishing the index {}", dir.display()))
            }
        }
    }

    /// Remove `name`'s entry when it records `vm_id`. The caller holds the name lock.
    fn remove_name_entry_of(&self, name: &str, vm_id: &str) -> Result<()> {
        let path = self.name_entry(name);
        match read_name_entry(&path)? {
            Some(entry) if entry.vm_id == vm_id => {
                remove_if_exists(&path).context("removing the VM's name entry")
            }
            _ => Ok(()),
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
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
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
    /// The VM's PID index entry goes with its state, under that lock, and its name entry
    /// after it, under the name lock, which is taken first (the order every claim takes
    /// the two locks in).
    pub async fn delete_state(&self, vm_id: &str) -> Result<()> {
        self.delete_state_inner(vm_id, false).await
    }

    /// `delete_state`, for a caller that may already hold the name lock (`names_held`).
    async fn delete_state_inner(&self, vm_id: &str, names_held: bool) -> Result<()> {
        LOOPBACK_LEASES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(id, _)| id != vm_id);
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", vm_id));
        let temp_file = self.state_dir.join(format!("{}.json.tmp", vm_id));

        // Who the VM is, for its index entries. A VM's name and PID are set by its claim
        // and never change, so this read needs no lock. With no readable state there is
        // nothing to look up by; the sweep removes any entry left behind.
        let known = fs::read_to_string(&state_file)
            .await
            .ok()
            .and_then(|json| serde_json::from_str::<VmState>(&json).ok());
        let names_lock = match known.as_ref().and_then(|state| state.name.as_ref()) {
            Some(_) if !names_held => Some(self.lock_names().await?),
            _ => None,
        };

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

        // The state is gone, so the entry no longer names a holder; removing it, under the
        // name lock, keeps the name index from growing with every name ever used.
        if let Some(name) = known.as_ref().and_then(|state| state.name.as_deref()) {
            self.remove_name_entry_of(name, vm_id)?;
        }
        if let Some(lock) = names_lock {
            lock.unlock()
                .map_err(|(_, error)| error)
                .context("releasing the VM name lock")?;
        }
        Ok(())
    }

    /// Remove the state files of processes that no longer exist, now. For a caller that
    /// has just killed a set of VMs and must leave nothing of them behind.
    /// Takes the sweep's lock, waiting up to 30 s for a launch's own sweep to finish, so two
    /// sweeps never scan at once.
    pub async fn sweep_stale(&self) {
        let Ok(mut file) = open_lock_file(&self.state_dir.join("sweep.lock")) else {
            return;
        };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let _guard = loop {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(guard) => break guard,
                Err((returned, _)) => {
                    if std::time::Instant::now() >= until {
                        return;
                    }
                    file = returned;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        };
        self.cleanup_stale_state().await;
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

        let entries = match std::fs::read_dir(&self.state_dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::debug!(
                    state_dir = %self.state_dir.display(),
                    error = %e,
                    "cleanup_stale_state: failed to read directory"
                );
                return;
            }
        };

        let mut examined = 0;
        let mut removed = 0;

        let mut seen = std::collections::HashSet::new();
        for entry in entries.flatten() {
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
                Ok(content) => {
                    let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) else {
                        continue;
                    };
                    let Some(pid) = state
                        .get("pid")
                        .and_then(|p| p.as_u64())
                        .and_then(|p| u32::try_from(p).ok())
                    else {
                        continue;
                    };
                    let Ok(exists) = Path::new(&format!("/proc/{pid}")).try_exists() else {
                        continue;
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
        let Ok(entries) = std::fs::read_dir(&self.state_dir) else {
            return;
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
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }

    /// Remove PID index entries whose state is gone or names another PID. Each is checked
    /// and removed under its VM's state lock, the lock `save_state` enters it under, so an
    /// entry of a state being saved is never removed. A lock held by someone else skips
    /// that entry until the next sweep.
    fn sweep_pid_index(&self) {
        let Ok(entries) = std::fs::read_dir(self.state_dir.join(PID_INDEX_DIR)) else {
            return;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some((pid, vm_id)) = file_name.to_str().and_then(|name| name.split_once('.'))
            else {
                continue;
            };
            let Ok(pid) = pid.parse::<u32>() else {
                continue;
            };
            let state_path = self.state_dir.join(format!("{vm_id}.json"));
            let lock_path = state_path.with_extension("json.lock");
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

    /// Remove name entries whose VM's state is gone, and temporary entries a writer that
    /// died left, under the name lock. A busy lock skips the sweep until the next one.
    fn sweep_name_index(&self) {
        let dir = self.state_dir.join(NAME_INDEX_DIR);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let Ok(file) = open_lock_file(&self.state_dir.join("names.lock")) else {
            return;
        };
        let Ok(_names) = Flock::lock(file, FlockArg::LockExclusiveNonblock) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "tmp") {
                let _ = remove_if_exists(&path);
                continue;
            }
            let Ok(Some(named)) = read_name_entry(&path) else {
                continue;
            };
            let state_path = self.state_dir.join(format!("{}.json", named.vm_id));
            if matches!(std::fs::metadata(&state_path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                let _ = remove_if_exists(&path);
            }
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
        let prefix = format!("{pid}.");
        let Ok(entries) = std::fs::read_dir(self.state_dir.join(PID_INDEX_DIR)) else {
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

        let paths = scan_state_dir(&self.state_dir, after_open).await?;

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

        for vm_id in vm_ids {
            let path = self.state_dir.join(format!("{vm_id}.json"));
            {
                tracing::trace!(
                    path = %path.display(),
                    "list_vms: reading state file"
                );

                match fs::read_to_string(&path).await {
                    Ok(state_json) => match serde_json::from_str::<VmState>(&state_json) {
                        Ok(state) => {
                            tracing::trace!(
                                path = %path.display(),
                                vm_id = %state.vm_id,
                                pid = ?state.pid,
                                "list_vms: parsed state file"
                            );
                            vms.push(state);
                        }
                        Err(e) => {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "list_vms: failed to parse state file"
                            );
                        }
                    },
                    // A VM that exits deletes its state file before its lock file,
                    // and a new VM makes its lock file before its state file, so a
                    // name from the scan can have no state file by the time it is
                    // read. Not an error.
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
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let temp_file = self.state_dir.join(format!("{}.json.tmp", vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", vm_id));

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

            // Atomic rename
            fs::rename(&temp_file, &state_file)
                .await
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
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", vm_id));
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
        std::fs::write(
            dir.path().join("vm-orphan.json"),
            serde_json::to_string(&orphan).unwrap(),
        )
        .unwrap();
        let read = manager
            .with_locked_state("vm-orphan", |state| state.vm_id.clone())
            .await
            .unwrap();
        assert_eq!(read, None);
        assert!(
            !dir.path().join("vm-orphan.json.lock").exists(),
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
        std::os::unix::fs::symlink(dir.path().join("deleted"), dir.path().join("vm-gone.json"))
            .unwrap();
        let (listed, log) = warnings_from_listing(dir.path());
        assert_eq!(listed, 0);
        assert_eq!(log, "", "a vanished state file was reported");
    }

    /// Control for the test above: the capture does see `list_vms` warn.
    #[test]
    fn an_unparsable_state_file_is_still_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vm-bad.json"), "not json").unwrap();
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
        std::fs::write(
            dir.join(format!("{}.json", state.vm_id)),
            serde_json::to_string(state).unwrap(),
        )
        .unwrap();
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
            !dir.path().join("vm-second.json").exists(),
            "a refused claim must write no state"
        );
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

    /// A VM whose process exited, whose PID now belongs to another process, or whose state
    /// names no process, does not hold its name, and its state is removed so a lookup by
    /// name finds one VM. Each holder claims the name the way a launch does and is then
    /// turned into one of those cases, and the next claim takes the name from it.
    #[tokio::test]
    async fn an_exited_vm_frees_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        type MakeStale = fn(&mut VmState);
        let cases: [(&str, MakeStale); 3] = [
            // Above the kernel's PID_MAX_LIMIT (4194304), so no process has it.
            ("vm-exited", |state| state.pid = Some(4_000_000_000)),
            ("vm-replaced", |state| {
                state.pid_start_time = state.pid_start_time.map(|start| start + 1)
            }),
            ("vm-unowned", |state| state.pid = None),
        ];
        for (vm_id, make_stale) in cases {
            let mut holder = named_state(vm_id, "slot-3");
            manager.claim_name(&mut holder).await.unwrap();
            make_stale(&mut holder);
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
    }

    /// The steps of a claim a SIGKILL can land after (`claim_holder_child`).
    const CLAIM_STEPS: [&str; 3] = ["checked", "entry", "claimed"];

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
                    if step == "checked" {
                        ready(&ip)
                    }
                },
                || {
                    if step == "entry" {
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
    /// the killed VM's state is gone, and its address can be leased again.
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
        }
    }

    /// Two claims of one name at once: the second runs entirely inside the first's
    /// window between its check and its write. Exactly one may succeed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn of_two_concurrent_claims_of_one_name_exactly_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let manager = StateManager::new(state_dir.clone());
        let mut first = named_state("vm-first", "slot-3");
        let mut second_thread = None;
        let mut second_finished_inside_window = false;
        manager
            .claim_name_after_check(&mut first, || {
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                second_thread = Some(std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let result = runtime.block_on(async {
                        let mut second = named_state("vm-second", "slot-3");
                        StateManager::new(state_dir).claim_name(&mut second).await
                    });
                    let _ = done_tx.send(());
                    result
                }));
                // Correct: the second claim waits on the name lock and cannot finish
                // here. Without the lock it finishes within milliseconds.
                second_finished_inside_window = done_rx
                    .recv_timeout(std::time::Duration::from_millis(500))
                    .is_ok();
            })
            .await
            .unwrap();
        let second = second_thread.unwrap().join().unwrap();
        assert!(
            !second_finished_inside_window,
            "the second claim finished while the first held the name between check and write"
        );
        let error = format!("{:#}", second.unwrap_err());
        assert!(error.contains("vm-first"), "{error}");
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
