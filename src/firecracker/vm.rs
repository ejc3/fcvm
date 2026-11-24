use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::utils::{spawn_streaming, strip_firecracker_prefix};

/// Seconds a guest's memory stays out of automatic NUMA balancing after Firecracker creates
/// or restores it (`--numa-balancing-after-secs`, ejc3/firecracker).
///
/// A balancing scan unmaps guest memory from stage 2 and the guest faults it back in, and a
/// UFFD clone's freshly copied pages are hinted and migrated. On a two-node c8g.metal-48xl,
/// with balancing on, a UFFD clone took 6,906 hinting faults and 3,317 migrations to start at
/// C=128 to 1000 clones, and 7.5 host CPU-s; out of balancing, 32, 15 and 2.6 CPU-s.
/// A guest that lives on gains from balancing: a 32-vCPU 200 GiB guest running a memory
/// bandwidth loop for 10 min reached 700 GB/s once balancing had placed its memory, against a
/// steady 434 GB/s without it, and 1000 idle clones cost 20% less CPU with it. So the
/// exclusion covers a clone's start (a C=128 ramp to 1000 clones takes 20 to 35 s) and ends.
///
/// Firecracker binds anonymous guest memory and a private mapping of a snapshot's memory
/// file. It leaves guest memory that maps a memfd as it is, which is what a clone of a
/// `--uffd-mode minor` serve has: a shmem mapping keeps its policy in the file, where every
/// clone of the serve reads it, so one clone's window would begin and end every clone's.
/// Such a clone has no window and needs none, because the kernel does not scan a shmem
/// mapping whose file has no policy (`vma_policy_mof`).
pub const NUMA_BALANCING_AFTER_SECS: u64 = 60;

use super::FirecrackerClient;

/// Socket/device wait timeout (total wait time = RETRY_COUNT * RETRY_DELAY)
const SOCKET_WAIT_RETRY_COUNT: u32 = 500;
const SOCKET_WAIT_RETRY_DELAY: Duration = Duration::from_millis(10);
/// Total budget for the API socket to accept connections (matches the old
/// 500 × 10ms retry ladder).
const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Number of recent Firecracker stderr lines kept for error reporting.
const STDERR_TAIL_LINES: usize = 10;
/// Upper bound on waiting for the stderr reader to reach EOF after Firecracker
/// exits, so the failure error can include what it actually printed. The wait
/// ends on EOF (normally microseconds — the exit closed the write end); this
/// only bounds the pathological case of an inherited, still-open pipe.
const STDERR_EOF_TIMEOUT: Duration = Duration::from_secs(2);

/// Environment variable that names the level Firecracker logs at into the per-VM
/// `firecracker.log`.
const FIRECRACKER_LOG_LEVEL_ENV: &str = "FCVM_FIRECRACKER_LOG_LEVEL";
/// The levels Firecracker's `--level` lists as its possible values.
const FIRECRACKER_LOG_LEVELS: [&str; 4] = ["Error", "Warning", "Info", "Debug"];
/// The level used when the environment names none.
const DEFAULT_FIRECRACKER_LOG_LEVEL: &str = "Info";

/// The `--level` Firecracker is started with: `Info` unless `requested` names another level.
///
/// At `Debug` Firecracker's vsock muxer writes one line per packet, so a guest whose volumes
/// are FUSE over vsock fills the per-VM `firecracker.log`. A 128 GiB guest wrote 5.10 GiB in
/// its first 65 minutes and 9.99 GiB by minute 84, onto the filesystem its snapshot was about
/// to need, and teardown copies the file to `/tmp`. `Info` keeps Firecracker's API requests,
/// warnings and errors.
fn firecracker_log_level(requested: Option<&str>) -> Result<&'static str> {
    let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_FIRECRACKER_LOG_LEVEL);
    };
    FIRECRACKER_LOG_LEVELS
        .iter()
        .copied()
        .find(|level| level.eq_ignore_ascii_case(requested))
        .ok_or_else(|| {
            anyhow!(
                "{FIRECRACKER_LOG_LEVEL_ENV}={requested:?} is not a Firecracker log level; use one of {}",
                FIRECRACKER_LOG_LEVELS.join(", ")
            )
        })
}

/// Fails when `FCVM_FIRECRACKER_LOG_LEVEL` names no Firecracker log level. `main` calls this
/// before a command prepares anything, and [`VmManager::start`] checks again for callers of the
/// library.
pub fn check_log_level_env() -> Result<()> {
    firecracker_log_level(std::env::var(FIRECRACKER_LOG_LEVEL_ENV).ok().as_deref()).map(|_| ())
}

/// Connection probe boundary for the API-socket readiness state machine.
///
/// Production uses Tokio's Unix stream. Tests provide exact NotFound,
/// ConnectionRefused, and ready sequences so every check-to-park interleave is
/// exercised without scheduler timing or a retry loop.
trait ApiSocketProbe {
    async fn connect(&mut self, socket_path: &Path) -> std::io::Result<()>;
}

struct TokioApiSocketProbe;

impl ApiSocketProbe for TokioApiSocketProbe {
    async fn connect(&mut self, socket_path: &Path) -> std::io::Result<()> {
        let stream = tokio::net::UnixStream::connect(socket_path).await?;
        drop(stream);
        Ok(())
    }
}

/// Firecracker's own ERROR lines repeated in fcvm's log per VMM process; later ones are only counted.
const REPEATED_FIRECRACKER_ERRORS: usize = 20;
/// How often [`FirecrackerLogFollower`] reads what Firecracker appended to its log.
const FIRECRACKER_LOG_POLL: Duration = Duration::from_secs(1);
/// The most [`FirecrackerLogErrors::read_new`] reads in one call.
const FIRECRACKER_LOG_READ_MAX: u64 = 16 << 20;
/// The longest unterminated line [`FirecrackerLogErrors`] keeps waiting for its newline.
const FIRECRACKER_LOG_LINE_MAX: usize = 64 << 10;

/// Whether `line` is one Firecracker wrote at level ERROR. With `--show-level --show-log-origin`
/// a line is `<time> [<instance>:<thread>:<LEVEL>:<file>:<line>] <message>`.
fn is_firecracker_error_line(line: &str) -> bool {
    line.split_once(" [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .is_some_and(|(origin, _)| origin.split(':').nth(2) == Some("ERROR"))
}

/// Picks Firecracker's ERROR lines out of what it appends to its own log (`--log-path`).
///
/// Firecracker reports a failure it answers the guest for, such as a block write the host
/// refused, only in that file, which fcvm otherwise reads only at teardown. A guest whose disk
/// write failed with ENOSPC on the host remounted its filesystem read-only, and the one record
/// of why was `Failed to execute Out virtio block request ... No space left on device` there.
#[derive(Debug, Default)]
struct FirecrackerLogErrors {
    /// Bytes of the file already read.
    offset: u64,
    /// The end of the last read, after its last newline.
    partial: Vec<u8>,
    /// ERROR lines returned so far.
    repeated: usize,
    /// ERROR lines found after [`REPEATED_FIRECRACKER_ERRORS`] were returned.
    counted: u64,
    /// Bytes the last [`Self::read_new`] read.
    last_read: u64,
}

impl FirecrackerLogErrors {
    /// Reads what was appended since the last call and returns the ERROR lines to repeat. A
    /// line is returned once it is complete, and only the first [`REPEATED_FIRECRACKER_ERRORS`]
    /// are; later ones are counted in `counted`. A file shorter than what was read is read again
    /// from its start.
    fn read_new<F: std::io::Read + std::io::Seek>(
        &mut self,
        file: &mut F,
    ) -> std::io::Result<Vec<String>> {
        use std::io::{Read, SeekFrom};
        let len = file.seek(SeekFrom::End(0))?;
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut appended = Vec::new();
        file.take(FIRECRACKER_LOG_READ_MAX)
            .read_to_end(&mut appended)?;
        self.last_read = appended.len() as u64;
        self.offset += self.last_read;
        self.partial.extend_from_slice(&appended);
        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(end) = self.partial[start..].iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.partial[start..start + end]);
            start += end + 1;
            if !is_firecracker_error_line(&line) {
                continue;
            }
            if self.repeated < REPEATED_FIRECRACKER_ERRORS {
                self.repeated += 1;
                lines.push(line.into_owned());
            } else {
                self.counted += 1;
            }
        }
        self.partial.drain(..start);
        if self.partial.len() > FIRECRACKER_LOG_LINE_MAX {
            self.partial.clear();
        }
        Ok(lines)
    }

    /// [`Self::read_new`] until the end of the file, in reads of at most
    /// [`FIRECRACKER_LOG_READ_MAX`], for the last read when the VMM has stopped.
    fn read_rest<F: std::io::Read + std::io::Seek>(
        &mut self,
        file: &mut F,
    ) -> std::io::Result<Vec<String>> {
        let mut lines = Vec::new();
        loop {
            lines.extend(self.read_new(file)?);
            if self.last_read < FIRECRACKER_LOG_READ_MAX {
                return Ok(lines);
            }
        }
    }
}

/// Empties the log `path` names before a launch. Firecracker opens `--log-path` without
/// truncating it and writes from its start, so a relaunch over the log an earlier process wrote
/// would leave that process's lines after its own, and the follower would repeat that
/// process's errors as the new one's. A missing log, or one that is not a regular file, is left
/// as it is. A log fcvm cannot open for writing fails the launch: Firecracker, run as the same
/// user, opens it for writing too.
fn clear_log_before_launch(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("reading {} before launching Firecracker", path.display())
            })
        }
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(0))
        .with_context(|| format!("emptying {} before launching Firecracker", path.display()))
}

/// Repeats Firecracker's ERROR lines in fcvm's log while the VMM runs, under fcvm's own target,
/// so they survive a filter on fcvm's targets (`RUST_LOG=fcvm=info`).
struct FirecrackerLogFollower {
    path: PathBuf,
    vm_id: String,
    file: Option<std::fs::File>,
    errors: FirecrackerLogErrors,
    /// Whether a read failed, after which nothing more is read.
    failed: bool,
    /// Whether the last poll ran. A read the follower's task queued before it was stopped can
    /// run after it, when the log may already hold the next process's lines.
    stopped: bool,
}

impl FirecrackerLogFollower {
    fn new(path: PathBuf, vm_id: String) -> Self {
        Self {
            path,
            vm_id,
            file: None,
            errors: FirecrackerLogErrors::default(),
            failed: false,
            stopped: false,
        }
    }

    /// Reads what Firecracker appended and logs its ERROR lines; with `last`, to the end of the
    /// file, and then how many were only counted since the last report. A failed read stops the
    /// reading, not that report. After the last poll it reads nothing. Returns the messages it
    /// logged.
    fn poll(&mut self, last: bool) -> Vec<String> {
        let mut messages = Vec::new();
        if self.stopped {
            return messages;
        }
        self.stopped = last;
        if !self.failed && self.file.is_none() {
            self.file = std::fs::File::open(&self.path).ok();
        }
        if let (false, Some(file)) = (self.failed, self.file.as_mut()) {
            let read = if last {
                self.errors.read_rest(file)
            } else {
                self.errors.read_new(file)
            };
            match read {
                Ok(lines) => {
                    messages.extend(
                        lines
                            .into_iter()
                            .map(|line| format!("Firecracker logged an error: {line}")),
                    );
                }
                Err(error) => {
                    self.failed = true;
                    messages.push(format!(
                        "cannot read Firecracker's log ({error}), so its errors are no longer repeated here"
                    ));
                }
            }
        }
        if last && self.errors.counted > 0 {
            messages.push(format!(
                "Firecracker logged {} more errors than are repeated here; they are in its log until a relaunch empties it",
                self.errors.counted
            ));
        }
        for message in &messages {
            warn!(vm_id = %self.vm_id, log = %self.path.display(), "{message}");
        }
        messages
    }
}

/// Manages a Firecracker VM process
///
/// IMPORTANT: PID Tracking Architecture
/// -------------------------------------
/// fcvm tracks its OWN process ID (via std::process::id()), not the Firecracker child process ID.
///
/// When `fcvm podman run` or `fcvm snapshot run` is executed, the fcvm process itself
/// stays running to manage the VM lifecycle. The PID stored in VmState is the fcvm
/// process PID, which allows:
/// - External tools to send signals to the correct process
/// - Health monitors to verify the manager process is still running
/// - Tests to track spawned fcvm processes without parsing stdout
///
/// The Firecracker process is a child of fcvm and is managed via the Child handle
/// stored in self.process. When fcvm exits (via signal or normally), it ensures
/// the Firecracker child process is also terminated.
pub struct VmManager {
    vm_id: String,
    vm_name: Option<String>,
    socket_path: PathBuf,
    log_path: Option<PathBuf>,
    /// What `FCVM_FIRECRACKER_LOG_LEVEL` held when this manager was made, checked in `start`.
    firecracker_log_level: Option<String>,
    namespace_id: Option<String>,
    holder_pid: Option<u32>, // namespace holder PID for rootless mode (use nsenter to run FC)
    user_namespace_path: Option<PathBuf>, // User namespace path for rootless clones (enter via setns in pre_exec)
    net_namespace_path: Option<PathBuf>, // Net namespace path for rootless clones (enter via setns in pre_exec)
    mount_redirects: Option<crate::utils::MountRedirects>, // mount namespace for a restored clone
    keep_fds: Vec<std::os::fd::RawFd>,   // descriptors left open across the VMM's exec
    decision: crate::confine::Decision,  // confined or not, from confine::resolve
    tap_owner: Option<String>,           // confined: the TAP its pre_exec makes the VMM's own
    process: Option<Child>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>, // last few Firecracker stderr lines, for launch failure errors
    // Reader task for the child's stderr pipe. Awaited (bounded) before rendering
    // `stderr_tail` on a launch failure, so the tail is complete rather than
    // whatever happened to have been logged by then.
    stderr_reader: Option<tokio::task::JoinHandle<()>>,
    /// Guest serial console lines observed on the Firecracker child's stdout
    /// since spawn. Watched by the restore path to detect a dead serial
    /// console (snapshot captured the UART mid-transmit).
    console_lines: Arc<std::sync::atomic::AtomicU64>,
    client: Option<FirecrackerClient>,
    /// The snapshot memory file the running VMM maps itself (a File-backed restore), marked
    /// in use (`uffd::release`) so that a memory server starting for another snapshot leaves
    /// its page cache alone. It belongs to the VMM process, not to fcvm: it is dropped when
    /// that process exits or is reaped, and when [`Self::start`] launches another one.
    memory_in_use: Option<std::fs::File>,
    /// Repeats the running VMM's ERROR lines from its log ([`FirecrackerLogFollower`]); stopped
    /// with the process, after one last read.
    log_follower: Option<(
        Arc<Mutex<FirecrackerLogFollower>>,
        tokio::task::JoinHandle<()>,
    )>,
}

impl VmManager {
    /// `decision` comes from the caller's [`crate::confine::resolve`]; every launch of this
    /// manager, a guest-reboot relaunch included, spawns the VMM under it.
    pub fn new(
        vm_id: String,
        socket_path: PathBuf,
        log_path: Option<PathBuf>,
        decision: crate::confine::Decision,
    ) -> Self {
        Self {
            vm_id,
            vm_name: None,
            socket_path,
            log_path,
            firecracker_log_level: std::env::var(FIRECRACKER_LOG_LEVEL_ENV).ok(),
            namespace_id: None,
            holder_pid: None,
            user_namespace_path: None,
            net_namespace_path: None,
            mount_redirects: None,
            keep_fds: Vec::new(),
            decision,
            tap_owner: None,
            process: None,
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            stderr_reader: None,
            console_lines: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            client: None,
            memory_in_use: None,
            log_follower: None,
        }
    }

    /// Mark the snapshot memory file this VMM maps in use for as long as this VMM process
    /// runs (`uffd::release`).
    ///
    /// A File-backed restore maps `memory.bin` in the VMM and starts no memory server, so
    /// nothing else holds the mark a release pass looks for. The mark is a shared `flock` on
    /// a descriptor this manager owns; it ends when the process exits, is killed and reaped,
    /// or is replaced by [`Self::start`], so neither a relaunch after a guest reboot nor a
    /// cold boot after a rejected load keeps it, or keeps the file open.
    pub async fn mark_memory_in_use(&mut self, memory_path: &Path) -> Result<()> {
        anyhow::ensure!(
            self.process.is_some(),
            "no VMM process to mark {} in use for",
            memory_path.display()
        );
        let file = std::fs::File::open(memory_path)
            .with_context(|| format!("opening {} to mark it in use", memory_path.display()))?;
        crate::uffd::mark_in_use(&file)
            .await
            .with_context(|| format!("marking {} in use", memory_path.display()))?;
        self.memory_in_use = Some(file);
        Ok(())
    }

    /// Counter of guest serial console lines seen since the Firecracker child
    /// spawned (see [`crate::hypervisor::Hypervisor::console_line_counter`]).
    pub fn console_line_counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.console_lines)
    }

    /// Set the VM name for logging purposes
    pub fn set_vm_name(&mut self, vm_name: String) {
        self.vm_name = Some(vm_name);
    }

    /// Set the network namespace for this VM
    ///
    /// When set, Firecracker will be launched inside the specified network namespace
    /// using setns() before exec. This isolates the VM's network stack.
    pub fn set_namespace(&mut self, namespace_id: String) {
        self.namespace_id = Some(namespace_id);
    }

    /// Set namespace holder PID for rootless networking
    ///
    /// When set, Firecracker will be launched inside an existing user+net namespace
    /// via nsenter. The holder process (`fcvm __netns-agent`)
    /// keeps the namespace alive while Firecracker runs.
    ///
    /// UID/GID mappings are written externally by `setup_namespace_mappings()`, which
    /// maps the current user (UID 1000) to UID 0 inside the namespace. Combined with
    /// `--preserve-credentials` when using nsenter, no chmod is needed on sockets or
    /// files - the user has access both inside and outside.
    pub fn set_holder_pid(&mut self, pid: u32) {
        self.holder_pid = Some(pid);
    }

    /// Descriptors the VMM keeps open across its exec
    /// ([`crate::utils::NamespaceParams::keep_fds`]).
    pub fn set_keep_fds(&mut self, fds: Vec<std::os::fd::RawFd>) {
        self.keep_fds = fds;
    }

    /// The unpooled TAP a confined VMM's pre_exec makes its own
    /// ([`crate::utils::NamespaceParams::tap_owner`]). Kept across relaunches.
    pub fn set_tap_owner(&mut self, tap: Option<String>) {
        self.tap_owner = tap;
    }

    /// Set user namespace path for rootless clones
    ///
    /// When set along with mount_redirects, pre_exec will enter this user namespace
    /// first (via setns) before doing mount operations. This gives CAP_SYS_ADMIN
    /// inside the user namespace, allowing unshare(CLONE_NEWNS) to succeed.
    ///
    /// Use this instead of set_holder_pid when mount namespace isolation is needed,
    /// since nsenter wrapper runs AFTER pre_exec.
    pub fn set_user_namespace_path(&mut self, path: PathBuf) {
        self.user_namespace_path = Some(path);
    }

    /// Set network namespace path for rootless clones
    ///
    /// When set, pre_exec will enter this network namespace (via setns) after
    /// completing mount operations. Use with set_user_namespace_path for
    /// rootless clones that need mount namespace isolation.
    pub fn set_net_namespace_path(&mut self, path: PathBuf) {
        self.net_namespace_path = Some(path);
    }

    /// Set mount redirects for mount namespace isolation
    ///
    /// When set, Firecracker will be launched in a new mount namespace with
    /// clone_dir bind-mounted over each baseline_dir. This allows multiple clones
    /// from the same snapshot to each have their own vsock socket and disk file bindings,
    /// even though vmstate.bin stores the baseline's paths.
    ///
    /// Each `(mountpoint, source)` pair bind-mounts `source` over `mountpoint`,
    /// in order (a later mountpoint may resolve inside an earlier pair's mount).
    /// Multiple pairs are needed because:
    /// - Vsock paths in vmstate.bin reference the original cache VM's directory
    /// - Disk paths in vmstate.bin reference the snapshotted VM's directory (after patch_drive)
    /// - A custom `--vsock-dir` retargets only the vsock directory, with the
    ///   clone's `disks/` bound back on top
    ///
    /// The bind mounts make Firecracker see the clone's (or the caller's vsock)
    /// directory contents when accessing any baseline path, so each clone binds
    /// its own socket files.
    pub fn set_mount_redirects(&mut self, redirects: crate::utils::MountRedirects) {
        self.mount_redirects = Some(redirects);
    }

    /// Start the Firecracker process
    ///
    /// `firecracker_args` provides extra CLI arguments (e.g., "--enable-nv2") passed
    /// explicitly from RuntimeConfig instead of reading the FCVM_FIRECRACKER_ARGS env var.
    pub async fn start(
        &mut self,
        firecracker_bin: &Path,
        config_override: Option<&Path>,
        firecracker_args: Option<&str>,
    ) -> Result<()> {
        // A confined VMM never launches with arguments that turn its seccomp off or replace
        // its filter, whatever the caller's resolve saw: checked on the arguments this
        // launch uses, before anything is cleared or removed.
        if self.decision.confinement().is_some() {
            let refused = firecracker_args
                .map(crate::confine::overriding_args)
                .unwrap_or_default();
            anyhow::ensure!(
                refused.is_empty(),
                "the VMM of {} is confined: {}",
                self.vm_id,
                refused.join("; ")
            );
        }
        // A relaunch is another VMM process, and it maps no snapshot the previous one did.
        self.memory_in_use = None;
        self.stop_log_follower();
        if let Some(log_path) = &self.log_path {
            clear_log_before_launch(log_path)?;
        }
        let log_level = firecracker_log_level(self.firecracker_log_level.as_deref())?;
        if let Some(ref name) = self.vm_name {
            info!(target: "vm", vm_name = %name, vm_id = %self.vm_id, "starting Firecracker process");
        } else {
            info!(target: "vm", vm_id = %self.vm_id, "starting Firecracker process");
        }

        // Remove existing socket (ignore errors if not exists - avoids TOCTOU race)
        let _ = std::fs::remove_file(&self.socket_path);

        // Build command based on mode:
        // 1. user_namespace_path set: direct Firecracker (namespaces entered via pre_exec setns)
        // 2. holder_pid set (no user_namespace_path): use nsenter to enter existing namespace (rootless baseline)
        // 3. neither: direct Firecracker (privileged/bridged mode)
        //
        // For rootless clones with mount_redirects, we MUST use pre_exec setns instead of nsenter,
        // because pre_exec runs BEFORE nsenter would enter the namespace, and we need CAP_SYS_ADMIN
        // from the user namespace to do mount operations.
        let mut cmd = if self.user_namespace_path.is_some() {
            // Use direct Firecracker - namespaces will be entered via setns in pre_exec.
            // Used for ALL rootless VMs (baselines + clones) because nsenter's internal
            // setns(CLONE_NEWUSER) clears PR_SET_PDEATHSIG, leaving Firecracker orphaned
            // if fcvm is SIGKILL'd. The pre_exec path sets pdeathsig AFTER setns.
            info!(target: "vm", vm_id = %self.vm_id, "using pre_exec setns for rootless VM");
            let mut c = Command::new(firecracker_bin);
            c.arg("--api-sock").arg(&self.socket_path);
            c
        } else if let Some(holder_pid) = self.holder_pid {
            // Fallback: nsenter to enter user+network namespace.
            // NOTE: This path is not normally reached — rootless VMs now use pre_exec
            // setns (above) which correctly preserves PR_SET_PDEATHSIG. nsenter's
            // internal setns(CLONE_NEWUSER) clears pdeathsig. Kept as safety net.
            info!(target: "vm", vm_id = %self.vm_id, holder_pid = holder_pid, "using nsenter for rootless networking");
            let mut c = Command::new("nsenter");
            c.args([
                "-t",
                &holder_pid.to_string(),
                "-U",
                "-n",
                "--preserve-credentials",
                "--",
            ]);
            c.arg(firecracker_bin)
                .arg("--api-sock")
                .arg(&self.socket_path);
            c
        } else {
            // Direct Firecracker invocation (privileged mode)
            let mut c = Command::new(firecracker_bin);
            c.arg("--api-sock").arg(&self.socket_path);
            c
        };

        if let Some(config) = config_override {
            cmd.arg("--config-file").arg(config);
        }

        // Setup logging
        if let Some(log_path) = &self.log_path {
            cmd.arg("--log-path").arg(log_path);
            cmd.arg("--level").arg(log_level);
            cmd.arg("--show-level");
            cmd.arg("--show-log-origin");
        }

        // Firecracker installs its default seccomp policy unless told otherwise. fcvm's
        // build is for the gnu target, whose default policy allows every syscall of the
        // native architecture (rootfs-config.toml, [firecracker]).
        // Set FCVM_NO_SECCOMP=1 to install none, for debugging.
        if self.decision.confinement().is_some()
            && (std::env::var_os("FCVM_NO_SECCOMP").is_some()
                || std::env::var_os("FCVM_STRACE_FC").is_some())
        {
            anyhow::bail!(
                "the VMM of {} is confined, and FCVM_NO_SECCOMP or FCVM_STRACE_FC would \
                 undo that; unset them",
                self.vm_id
            );
        }
        if std::env::var("FCVM_NO_SECCOMP").is_ok() {
            warn!("Seccomp disabled via FCVM_NO_SECCOMP - reduced security");
            cmd.arg("--no-seccomp");
        }

        // The guest's memory stays out of NUMA balancing for its first minute.
        if crate::commands::common::firecracker_takes_numa_window(firecracker_bin) {
            cmd.arg("--numa-balancing-after-secs")
                .arg(NUMA_BALANCING_AFTER_SECS.to_string());
        }

        // Additional firecracker args from RuntimeConfig (passed explicitly by caller)
        if let Some(extra) = firecracker_args {
            for arg in extra.split_whitespace() {
                cmd.arg(arg);
            }
        }

        // FCVM_STRACE_FC: wrap Firecracker with strace for debugging.
        // Output goes to /tmp/fcvm-strace-fc-{vm_id}.log on the host.
        // Traces ioctl, mmap, and signal syscalls by default.
        // Set FCVM_STRACE_FC=all for full trace.
        if let Ok(trace_filter) = std::env::var("FCVM_STRACE_FC") {
            let strace_log = format!("/tmp/fcvm-strace-fc-{}.log", &self.vm_id);
            let filter = if trace_filter == "all" {
                String::new()
            } else if trace_filter.contains('=') || trace_filter.contains(',') {
                // Custom filter like "ioctl,mmap" or "trace=ioctl"
                format!("-e {}", trace_filter)
            } else {
                "-e trace=ioctl,mmap,madvise,mprotect".to_string()
            };
            warn!(
                strace_log = %strace_log,
                filter = %filter,
                "FCVM_STRACE_FC: wrapping Firecracker with strace"
            );
            // Prepend strace to the command
            let program = cmd.as_std().get_program().to_owned();
            let args: Vec<_> = cmd.as_std().get_args().map(|a| a.to_owned()).collect();
            cmd = Command::new("strace");
            cmd.arg("-f").arg("-tt").arg("-o").arg(&strace_log);
            if !filter.is_empty() {
                for part in filter.split_whitespace() {
                    cmd.arg(part);
                }
            }
            cmd.arg(program);
            for arg in args {
                cmd.arg(arg);
            }
        }

        // Apply namespace/mount isolation + pdeathsig (shared with the Cloud Hypervisor
        // backend so both VMMs get identical isolation + parent-death behavior).
        crate::utils::install_namespace_pre_exec(
            &mut cmd,
            &crate::utils::NamespaceParams {
                vm_id: self.vm_id.clone(),
                namespace_id: self.namespace_id.clone(),
                user_namespace_path: self.user_namespace_path.clone(),
                net_namespace_path: self.net_namespace_path.clone(),
                mount_redirects: self.mount_redirects.clone(),
                keep_fds: self.keep_fds.clone(),
                decision: self.decision.clone(),
                tap_owner: self.tap_owner.clone(),
            },
        )?;

        // Spawn process with streaming output
        let stderr_tail = Arc::clone(&self.stderr_tail);
        let console_lines = Arc::clone(&self.console_lines);
        let spawned = spawn_streaming(cmd, move |line, is_stderr| {
            let clean = strip_firecracker_prefix(line);
            if !is_stderr {
                // Firecracker writes the guest serial console to its stdout
                // (its own logs go to --log-path), so every stdout line is
                // guest console output.
                console_lines.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if is_stderr {
                // Keep the last few stderr lines so launch failures can report
                // what Firecracker actually printed (its errors only appear at
                // DEBUG level in the normal log stream).
                if let Ok(mut tail) = stderr_tail.lock() {
                    if tail.len() >= STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(clean.to_string());
                }
            }
            log_firecracker_line(clean, is_stderr);
        })
        .context("spawning Firecracker process")?;

        self.process = Some(spawned.child);
        self.stderr_reader = Some(spawned.stderr_reader);
        self.start_log_follower();

        // Wait for socket to be ready
        self.wait_for_socket().await?;

        // Create API client
        self.client = Some(FirecrackerClient::new(self.socket_path.clone())?);

        Ok(())
    }

    /// Wait for the Firecracker API server to accept connections
    ///
    /// The socket file becoming visible is not enough: there is a window between
    /// Firecracker's bind() (file exists) and listen() where connect() fails with
    /// ECONNREFUSED. Probe by actually connecting so the first real API request
    /// never races that window.
    ///
    /// Event-driven: an inotify watch on the socket's parent directory (registered
    /// BEFORE the first probe, so a file appearing mid-check always produces an
    /// event) wakes the probe the instant Firecracker binds — the old fixed 10ms
    /// poll cost most of that interval on every launch/clone. The bind→listen
    /// window has no filesystem event, so ECONNREFUSED retries at 1ms. A coarse
    /// safety tick re-checks even without events, and the deadline bounds all
    /// paths, so a lost event can only add latency, never hang.
    ///
    /// Also watches the Firecracker child process on every iteration (including
    /// the 1ms ECONNREFUSED path): if it exits before the API server accepts
    /// connections, fail with its exit status and recent stderr output instead
    /// of timing out with a generic socket error.
    async fn wait_for_socket(&mut self) -> Result<()> {
        let mut watch =
            self.socket_path
                .parent()
                .and_then(|dir| match crate::utils::DirWatch::new(dir) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        warn!(
                            error = %e,
                            "inotify unavailable for API-socket wait; degrading to 10ms polling"
                        );
                        None
                    }
                });
        let mut probe = TokioApiSocketProbe;

        self.wait_for_socket_with(&mut probe, &mut watch).await
    }

    async fn wait_for_socket_with<P, E>(&mut self, probe: &mut P, watch: &mut E) -> Result<()>
    where
        P: ApiSocketProbe,
        E: crate::utils::DirEventSource,
    {
        use tokio::time::sleep;

        let deadline = tokio::time::Instant::now() + SOCKET_WAIT_TIMEOUT;
        loop {
            // ECONNREFUSED retries on a tight 1ms cadence (set below) instead of
            // waiting for a directory event — but still falls through to the
            // child-liveness check first: if Firecracker binds the socket and
            // then dies, the socket FILE persists and refuses connections
            // forever, and skipping try_wait would spin out the whole deadline
            // and report a generic timeout instead of the exit status + stderr.
            let mut refused = false;
            match probe.connect(&self.socket_path).await {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Socket file not created yet — wait for a directory event below.
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    // bind() done, listen() imminent (sub-millisecond window
                    // with no fs event).
                    refused = true;
                }
                Err(e) => bail!(
                    "unexpected error connecting to Firecracker API socket {}: {}",
                    self.socket_path.display(),
                    e
                ),
            }

            if let Some(status) = self.try_wait()? {
                // Wait for the stderr reader to hit EOF so the error includes
                // everything Firecracker printed. The exit closed the write end,
                // so this returns as soon as the reader drains the pipe.
                crate::utils::wait_for_stderr_eof(&mut self.stderr_reader, STDERR_EOF_TIMEOUT)
                    .await;
                bail!(
                    "Firecracker exited with {} before its API socket became ready{}",
                    status,
                    self.stderr_tail_message()
                );
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }

            if refused {
                // Liveness verified above — retry the connect at 1ms.
                sleep(Duration::from_millis(1)).await;
                continue;
            }

            if watch.is_available() {
                tokio::select! {
                    event = watch.next_event() => {
                        // Directory activity — loop re-probes the socket.
                        // A (persistent) watch error degrades to the old
                        // fixed cadence via this sleep, bounded by deadline.
                        if event.is_err() {
                            sleep(SOCKET_WAIT_RETRY_DELAY).await;
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {}
                    _ = sleep(Duration::from_millis(100)) => {
                        // Safety tick: re-check child liveness + socket
                        // even if no event arrives.
                    }
                }
            } else {
                sleep(SOCKET_WAIT_RETRY_DELAY).await;
            }
        }

        bail!(
            "Firecracker socket not ready after {} seconds",
            SOCKET_WAIT_TIMEOUT.as_secs()
        )
    }

    /// Render the captured Firecracker stderr tail for error messages.
    fn stderr_tail_message(&self) -> String {
        let lines: Vec<String> = self
            .stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default();
        if lines.is_empty() {
            "; no stderr output captured (run with RUST_LOG=debug for full Firecracker output)"
                .to_string()
        } else {
            format!("; last stderr output:\n  {}", lines.join("\n  "))
        }
    }

    /// Get the API client
    pub fn client(&self) -> Result<&FirecrackerClient> {
        self.client.as_ref().context("VM not started")
    }

    /// Get the VM process PID
    pub fn pid(&self) -> Result<u32> {
        if let Some(process) = &self.process {
            let pid_opt = process.id();
            info!("Firecracker Child.id() returned: {:?}", pid_opt);
            pid_opt.ok_or_else(|| anyhow!("process ID not available from tokio::process::Child"))
        } else {
            bail!("VM process not running")
        }
    }

    /// Non-blocking check if the VM process has exited.
    /// Returns Some(ExitStatus) if the process has exited, None if still running.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if let Some(ref mut process) = self.process {
            match process
                .try_wait()
                .context("checking Firecracker process status")?
            {
                Some(status) => {
                    self.process = None;
                    self.memory_in_use = None;
                    self.stop_log_follower();
                    Ok(Some(status))
                }
                None => Ok(None),
            }
        } else {
            bail!("VM process not running")
        }
    }

    /// Wait for the VM process to exit
    ///
    /// NOTE: This does NOT take ownership of the process handle.
    /// Use `kill()` to terminate and cleanup the process.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        if let Some(ref mut process) = self.process {
            let status = process
                .wait()
                .await
                .context("waiting for Firecracker process")?;
            // Process exited naturally, clear the handle
            self.process = None;
            self.memory_in_use = None;
            self.stop_log_follower();
            Ok(status)
        } else {
            bail!("VM process not running")
        }
    }

    /// Starts repeating the VMM's ERROR lines from its log, if it has one. `start` has
    /// stopped the previous process's follower.
    fn start_log_follower(&mut self) {
        let Some(path) = self.log_path.clone() else {
            return;
        };
        let follower = Arc::new(Mutex::new(FirecrackerLogFollower::new(
            path,
            self.vm_id.clone(),
        )));
        let polled = Arc::clone(&follower);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(FIRECRACKER_LOG_POLL).await;
                let polled = Arc::clone(&polled);
                let read = tokio::task::spawn_blocking(move || {
                    if let Ok(mut follower) = polled.lock() {
                        follower.poll(false);
                    }
                });
                if read.await.is_err() {
                    return;
                }
            }
        });
        self.log_follower = Some((follower, task));
    }

    /// Stops the follower and reads the log once more, so an error logged just before the VMM
    /// exited is repeated too.
    fn stop_log_follower(&mut self) {
        if let Some((follower, task)) = self.log_follower.take() {
            task.abort();
            if let Ok(mut follower) = follower.lock() {
                follower.poll(true);
            }
        }
    }

    /// SIGKILL the Firecracker process without waiting for it to exit.
    ///
    /// Pairs with [`Self::reap`]. Keeping the handle here (rather than taking it) is what
    /// lets the caller signal other processes before coming back to wait on this one.
    pub fn start_kill(&mut self) -> Result<()> {
        if let Some(ref mut process) = self.process {
            info!(vm_id = %self.vm_id, "SIGKILL to Firecracker process");
            process
                .start_kill()
                .context("sending SIGKILL to Firecracker process")?;
        }
        Ok(())
    }

    /// Wait for the Firecracker process to exit and reap it.
    ///
    /// The bulk of the wait is the kernel reclaiming the guest's address space, so this
    /// returning means the guest memory is genuinely released — not just signalled.
    pub async fn reap(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.wait().await;
        }
        self.memory_in_use = None;
        self.stop_log_follower();
    }

    /// SIGKILL the Firecracker process and reap it. For callers that have nothing else to
    /// tear down concurrently; teardown proper uses the two phases separately.
    pub async fn kill(&mut self) -> Result<()> {
        self.start_kill()?;
        self.reap().await;
        Ok(())
    }

    /// Stream serial console output
    pub async fn stream_console(&self, console_path: &Path) -> Result<mpsc::Receiver<String>> {
        let (tx, rx) = mpsc::channel(100);
        let console_path = console_path.to_owned();

        tokio::spawn(async move {
            // Wait for console device to appear
            for _ in 0..SOCKET_WAIT_RETRY_COUNT {
                if console_path.exists() {
                    break;
                }
                tokio::time::sleep(SOCKET_WAIT_RETRY_DELAY).await;
            }

            if !console_path.exists() {
                error!("console device not found at {:?}", console_path);
                return;
            }

            // Open and stream the console
            match tokio::fs::File::open(&console_path).await {
                Ok(file) => {
                    let reader = BufReader::new(file);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if tx.send(line).await.is_err() {
                            break; // Receiver dropped
                        }
                    }
                }
                Err(e) => error!("failed to open console: {}", e),
            }
        });

        Ok(rx)
    }

    /// Get VM ID
    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }
}

impl Drop for VmManager {
    fn drop(&mut self) {
        self.stop_log_follower();
        // Clean up socket on drop
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

/// Log one line Firecracker printed: from its stderr, or from its stdout, which is the guest's
/// serial console. A console line fcvm keeps above DEBUG goes under fcvm's own targets
/// (`log_console_line_under_fcvm`); a stderr line naming fc-agent to WARN; everything else to
/// DEBUG under `firecracker`.
fn log_firecracker_line(clean: &str, is_stderr: bool) {
    if is_stderr {
        if crate::utils::console_line_is_important(clean) {
            warn!(target: "firecracker", "{}", clean);
        } else {
            debug!(target: "firecracker", "{}", clean);
        }
    } else if !crate::utils::log_console_line_under_fcvm(clean) {
        debug!(target: "firecracker", "{}", clean);
    }
}

#[cfg(test)]
mod tests {

    /// A confined manager refuses launch arguments that turn seccomp off or set a filter,
    /// before it touches its log or socket, whatever the caller resolved.
    #[tokio::test]
    async fn a_confined_launch_refuses_arguments_that_override_seccomp() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("firecracker.log");
        std::fs::write(&log, "kept").unwrap();
        let mut vm = VmManager::new(
            "confined-args".to_string(),
            dir.path().join("api.sock"),
            Some(log.clone()),
            crate::confine::Decision::for_test(Some(crate::confine::Confinement {
                landlock_abi: crate::confine::LANDLOCK_ABI_FLOOR,
                layers: vec![crate::confine::Layer::Process],
                core_limit: 1,
            })),
        );
        for args in ["--no-seccomp", "--enable-nv2 --seccomp-filter /x.bpf"] {
            let error = vm
                .start(
                    std::path::Path::new("/nonexistent/firecracker"),
                    None,
                    Some(args),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("which confinement owns"), "{args}: {error}");
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "kept");
    }

    use super::*;
    use crate::utils::DirEventSource;
    use std::future::{poll_fn, Future};
    use std::os::unix::fs::PermissionsExt;
    use std::pin::Pin;
    use std::task::Poll;

    const BLOCK_ENOSPC: &str = "2026-11-23T00:09:21.211902195 [anonymous-instance:main:ERROR:src/vmm/src/devices/virtio/block/virtio/request.rs:121] Failed to execute Out virtio block request: FileEngine(Sync(Transfer(IOError(Os { code: 28, kind: StorageFull, message: \"No space left on device\" }))))";
    const INFO_LINE: &str = "2026-11-23T00:04:33.374854932 [anonymous-instance:main:INFO:src/vmm/src/devices/virtio/device.rs:228] [Block:rootfs] notifying queues";

    #[test]
    fn firecracker_error_lines_are_picked_by_their_level() {
        assert!(is_firecracker_error_line(BLOCK_ENOSPC));
        assert!(!is_firecracker_error_line(INFO_LINE));
        // The level is the third field of the origin, not any "ERROR" in the message.
        assert!(!is_firecracker_error_line(
            "2026-11-23T00:04:33.37 [anonymous-instance:main:WARN:src/x.rs:1] saw ERROR: in a message"
        ));
        assert!(!is_firecracker_error_line("ERROR without an origin"));
    }

    #[test]
    fn firecracker_log_errors_returns_each_complete_error_line_once() {
        let mut file = std::io::Cursor::new(Vec::new());
        let mut errors = FirecrackerLogErrors::default();
        file.get_mut()
            .extend_from_slice(format!("{INFO_LINE}\n{}", &BLOCK_ENOSPC[..40]).as_bytes());
        assert!(errors.read_new(&mut file).unwrap().is_empty());
        file.get_mut()
            .extend_from_slice(format!("{}\n{INFO_LINE}\n", &BLOCK_ENOSPC[40..]).as_bytes());
        assert_eq!(
            errors.read_new(&mut file).unwrap(),
            vec![BLOCK_ENOSPC.to_string()]
        );
        assert!(errors.read_new(&mut file).unwrap().is_empty());
    }

    #[test]
    fn firecracker_log_errors_counts_the_errors_past_the_cap() {
        let lines: String = (0..REPEATED_FIRECRACKER_ERRORS + 5)
            .map(|_| format!("{BLOCK_ENOSPC}\n"))
            .collect();
        let mut file = std::io::Cursor::new(lines.into_bytes());
        let mut errors = FirecrackerLogErrors::default();
        assert_eq!(
            errors.read_new(&mut file).unwrap().len(),
            REPEATED_FIRECRACKER_ERRORS
        );
        assert_eq!(errors.counted, 5);
    }

    #[test]
    fn the_last_poll_reaches_an_error_behind_a_large_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.log");
        let backlog = format!("{INFO_LINE}\n")
            .repeat((FIRECRACKER_LOG_READ_MAX as usize / (INFO_LINE.len() + 1)) + 10);
        std::fs::write(&path, format!("{backlog}{BLOCK_ENOSPC}\n")).unwrap();
        let mut follower = FirecrackerLogFollower::new(path, "vm-test".to_string());
        assert_eq!(
            follower.poll(true),
            vec![format!("Firecracker logged an error: {BLOCK_ENOSPC}")]
        );
    }

    #[test]
    fn firecracker_log_errors_read_rest_reads_a_log_that_shrank_from_its_start() {
        let mut file = std::io::Cursor::new(format!("{INFO_LINE}\n{INFO_LINE}\n").into_bytes());
        let mut errors = FirecrackerLogErrors::default();
        assert!(errors.read_new(&mut file).unwrap().is_empty());
        let short = "t [i:main:ERROR:f.rs:1] x";
        *file.get_mut() = format!("{short}\n").into_bytes();
        assert_eq!(
            errors.read_rest(&mut file).unwrap(),
            vec![short.to_string()]
        );
    }

    #[test]
    fn a_poll_after_the_last_one_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.log");
        std::fs::write(&path, format!("{INFO_LINE}\n")).unwrap();
        let mut follower = FirecrackerLogFollower::new(path.clone(), "vm-test".to_string());
        assert!(follower.poll(true).is_empty());
        // A read the stopped follower's task had already queued runs after the relaunch emptied
        // the log and the next process wrote an error: that error is the new follower's.
        let short = "t [i:main:ERROR:f.rs:1] new";
        assert!(short.len() < INFO_LINE.len());
        std::fs::write(&path, format!("{short}\n")).unwrap();
        assert!(follower.poll(false).is_empty());
    }

    #[test]
    fn start_stops_the_old_follower_then_empties_the_log_then_starts_the_new_one() {
        let src = include_str!("vm.rs");
        let start = &src[src.find("    pub async fn start(").unwrap()..];
        let start = &start[..start.find("\n    }\n").unwrap()];
        let at = |needle: &str| {
            start
                .find(needle)
                .unwrap_or_else(|| panic!("start() no longer contains {needle}"))
        };
        assert!(
            at("self.stop_log_follower();") < at("clear_log_before_launch(")
                && at("clear_log_before_launch(")
                    < at(".context(\"spawning Firecracker process\")")
                && at(".context(\"spawning Firecracker process\")")
                    < at("self.start_log_follower();"),
            "start() must stop the old follower, empty the log, spawn, then start the new follower"
        );
    }

    #[test]
    fn a_relaunch_repeats_only_the_new_processs_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.log");
        std::fs::write(&path, format!("{BLOCK_ENOSPC}\n{INFO_LINE}\n")).unwrap();
        clear_log_before_launch(&path).unwrap();
        // Firecracker writes from the start of the file without truncating it.
        let short = "t [i:main:ERROR:f.rs:1] new";
        let mut log = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        std::io::Write::write_all(&mut log, format!("{short}\n").as_bytes()).unwrap();
        let mut errors = FirecrackerLogErrors::default();
        let mut file = std::fs::File::open(&path).unwrap();
        assert_eq!(errors.read_new(&mut file).unwrap(), vec![short.to_string()]);
        clear_log_before_launch(&dir.path().join("missing.log")).unwrap();
        assert!(!dir.path().join("missing.log").exists());
    }

    #[test]
    fn the_last_poll_reports_the_counted_errors_after_a_failed_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.log");
        std::fs::write(
            &path,
            format!("{BLOCK_ENOSPC}\n").repeat(REPEATED_FIRECRACKER_ERRORS + 5),
        )
        .unwrap();
        let mut follower = FirecrackerLogFollower::new(path, "vm-test".to_string());
        assert_eq!(follower.poll(false).len(), REPEATED_FIRECRACKER_ERRORS);
        // Reading a directory fails with EISDIR.
        follower.file = Some(std::fs::File::open(dir.path()).unwrap());
        assert!(follower.poll(false)[0].starts_with("cannot read Firecracker's log"));
        let last = follower.poll(true);
        assert_eq!(
            last,
            vec!["Firecracker logged 5 more errors than are repeated here; they are in its log until a relaunch empties it"]
        );
    }

    #[test]
    fn firecracker_log_errors_reads_a_truncated_log_from_its_start() {
        let mut file = std::io::Cursor::new(format!("{INFO_LINE}\n{INFO_LINE}\n").into_bytes());
        let mut errors = FirecrackerLogErrors::default();
        assert!(errors.read_new(&mut file).unwrap().is_empty());
        let short = "t [i:main:ERROR:f.rs:1] x";
        assert!(short.len() + 1 < 2 * (INFO_LINE.len() + 1));
        *file.get_mut() = format!("{short}\n").into_bytes();
        file.set_position(0);
        assert_eq!(errors.read_new(&mut file).unwrap(), vec![short.to_string()]);
    }

    #[derive(Clone, Copy)]
    enum ProbeStep {
        Ready,
        NotFound,
        Refused,
    }

    struct ScriptedProbe {
        steps: VecDeque<ProbeStep>,
        repeat: Option<ProbeStep>,
        calls: usize,
    }

    impl ScriptedProbe {
        fn once(steps: impl IntoIterator<Item = ProbeStep>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                repeat: None,
                calls: 0,
            }
        }

        fn repeating(step: ProbeStep) -> Self {
            Self {
                steps: VecDeque::new(),
                repeat: Some(step),
                calls: 0,
            }
        }
    }

    impl ApiSocketProbe for ScriptedProbe {
        async fn connect(&mut self, _socket_path: &Path) -> std::io::Result<()> {
            self.calls += 1;
            let step = self
                .steps
                .pop_front()
                .or(self.repeat)
                .expect("scripted API-socket probe exhausted");
            match step {
                ProbeStep::Ready => Ok(()),
                ProbeStep::NotFound => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                ProbeStep::Refused => {
                    Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
                }
            }
        }
    }

    enum EventStep {
        Wake,
        ReadError,
    }

    struct ScriptedEvents {
        steps: VecDeque<EventStep>,
        calls: usize,
    }

    impl ScriptedEvents {
        fn new(steps: impl IntoIterator<Item = EventStep>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                calls: 0,
            }
        }
    }

    impl DirEventSource for ScriptedEvents {
        fn is_available(&self) -> bool {
            true
        }

        async fn next_event(&mut self) -> anyhow::Result<()> {
            self.calls += 1;
            let step = self.steps.pop_front().expect("event script exhausted");
            match step {
                EventStep::Wake => Ok(()),
                EventStep::ReadError => Err(anyhow!("injected inotify read failure")),
            }
        }
    }

    fn manager_with_live_child(socket_path: PathBuf) -> VmManager {
        let mut command = Command::new("cat");
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = command.spawn().expect("spawn live child");

        let mut vm = VmManager::new(
            "wait-test".to_string(),
            socket_path,
            None,
            crate::confine::Decision::for_test(None),
        );
        vm.process = Some(child);
        vm
    }

    async fn assert_pending_once<F>(mut future: Pin<&mut F>)
    where
        F: Future,
    {
        poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("future completed before its required signal"),
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn api_socket_event_queued_after_check_wakes_without_clock_fallback() {
        let dir = tempfile::tempdir().expect("create socket directory");
        let mut vm = manager_with_live_child(dir.path().join("api.sock"));
        let mut probe = ScriptedProbe::once([ProbeStep::NotFound, ProbeStep::Ready]);
        let mut events = ScriptedEvents::new([EventStep::Wake]);
        let before = tokio::time::Instant::now();

        vm.wait_for_socket_with(&mut probe, &mut events)
            .await
            .expect("queued directory event should trigger a second probe");

        assert_eq!(probe.calls, 2);
        assert_eq!(events.calls, 1);
        assert_eq!(tokio::time::Instant::now(), before, "safety tick fired");
        vm.kill().await.expect("stop live child");
    }

    #[tokio::test(start_paused = true)]
    async fn api_socket_unavailable_watch_uses_ten_millisecond_fallback() {
        let dir = tempfile::tempdir().expect("create socket directory");
        let mut vm = manager_with_live_child(dir.path().join("api.sock"));
        let mut probe = ScriptedProbe::once([ProbeStep::NotFound, ProbeStep::Ready]);
        let mut no_watch = None::<crate::utils::DirWatch>;
        let before = tokio::time::Instant::now();

        vm.wait_for_socket_with(&mut probe, &mut no_watch)
            .await
            .expect("poll fallback should re-probe without inotify");

        assert_eq!(probe.calls, 2);
        assert_eq!(
            tokio::time::Instant::now() - before,
            SOCKET_WAIT_RETRY_DELAY
        );
        vm.kill().await.expect("stop live child");
    }

    #[tokio::test(start_paused = true)]
    async fn api_socket_inotify_read_error_uses_ten_millisecond_fallback() {
        let dir = tempfile::tempdir().expect("create socket directory");
        let mut vm = manager_with_live_child(dir.path().join("api.sock"));
        let mut probe = ScriptedProbe::once([ProbeStep::NotFound, ProbeStep::Ready]);
        let mut events = ScriptedEvents::new([EventStep::ReadError]);
        let before = tokio::time::Instant::now();

        vm.wait_for_socket_with(&mut probe, &mut events)
            .await
            .expect("watch read failure should degrade to polling");

        assert_eq!(probe.calls, 2);
        assert_eq!(events.calls, 1);
        assert_eq!(
            tokio::time::Instant::now() - before,
            SOCKET_WAIT_RETRY_DELAY
        );
        vm.kill().await.expect("stop live child");
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_refusal_reports_child_exit_after_complete_stderr() {
        let dir = tempfile::tempdir().expect("create socket directory");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 23")
            .spawn()
            .expect("spawn exited child");
        let status = child.wait().await.expect("reap exited child");
        assert_eq!(status.code(), Some(23));

        let mut vm = VmManager::new(
            "refused-test".to_string(),
            dir.path().join("api.sock"),
            None,
            crate::confine::Decision::for_test(None),
        );
        vm.process = Some(child);

        let (reader_started_tx, reader_started_rx) = tokio::sync::oneshot::channel();
        let (release_reader_tx, release_reader_rx) = tokio::sync::oneshot::channel();
        let stderr_tail = Arc::clone(&vm.stderr_tail);
        vm.stderr_reader = Some(tokio::spawn(async move {
            let _ = reader_started_tx.send(());
            let _ = release_reader_rx.await;
            stderr_tail
                .lock()
                .expect("stderr tail lock")
                .push_back("final stderr line".to_string());
        }));
        reader_started_rx.await.expect("stderr reader parked");

        let mut probe = ScriptedProbe::repeating(ProbeStep::Refused);
        let mut no_watch = None::<crate::utils::DirWatch>;
        let mut wait = Box::pin(vm.wait_for_socket_with(&mut probe, &mut no_watch));

        // The exited-child diagnosis must park on the reader. If the
        // ECONNREFUSED branch skips liveness, or error formatting races EOF,
        // this future either retries or completes here instead.
        assert_pending_once(wait.as_mut()).await;
        release_reader_tx.send(()).expect("release stderr reader");
        let err = wait
            .await
            .expect_err("an exited child cannot become API-ready");
        let message = format!("{err:#}");

        assert_eq!(probe.calls, 1, "ECONNREFUSED skipped child liveness");
        assert!(
            message.contains("Firecracker exited with exit status: 23"),
            "{message}"
        );
        assert!(message.contains("final stderr line"), "{message}");
        assert!(!message.contains("socket not ready"), "{message}");
    }

    /// A Firecracker process that exits before creating its API socket must fail
    /// `start()` with its exit status and stderr output, not the generic
    /// "socket not ready after 5 seconds" timeout.
    #[tokio::test]
    async fn start_reports_immediate_firecracker_exit() {
        let dir = tempfile::tempdir().expect("create temp dir");

        let fake_fc = dir.path().join("fake-firecracker.sh");
        std::fs::write(
            &fake_fc,
            "#!/bin/sh\necho 'boom: bad firecracker argument' >&2\nexit 2\n",
        )
        .expect("write fake firecracker");
        std::fs::set_permissions(&fake_fc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake firecracker");

        let socket_path = dir.path().join("api.sock");
        let mut vm = VmManager::new(
            "test-vm".to_string(),
            socket_path,
            None,
            crate::confine::Decision::for_test(None),
        );

        let err = vm
            .start(&fake_fc, None, None)
            .await
            .expect_err("start should fail when Firecracker exits immediately");
        let msg = format!("{:#}", err);

        assert!(
            msg.contains("Firecracker exited with"),
            "error should report the child exit status, got: {msg}"
        );
        assert!(
            msg.contains("boom: bad firecracker argument"),
            "error should include Firecracker's stderr output, got: {msg}"
        );
        assert!(
            !msg.contains("socket not ready"),
            "launch failure must not be reported as a socket timeout, got: {msg}"
        );
    }

    #[test]
    fn firecracker_logs_at_info_unless_asked_otherwise() {
        assert_eq!(firecracker_log_level(None).unwrap(), "Info");
        assert_eq!(firecracker_log_level(Some("  ")).unwrap(), "Info");
        assert_eq!(firecracker_log_level(Some("debug")).unwrap(), "Debug");
        assert_eq!(firecracker_log_level(Some(" Warning ")).unwrap(), "Warning");
        let refused = firecracker_log_level(Some("verbose"))
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("FCVM_FIRECRACKER_LOG_LEVEL")
                && refused.contains("Error, Warning, Info, Debug"),
            "{refused}"
        );
    }

    /// What a fake Firecracker was started with, one argument per line, or None when it never ran.
    async fn arguments_of_a_start(requested: Option<&str>) -> (Result<()>, Option<Vec<String>>) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let argv = dir.path().join("argv");
        let fake_fc = dir.path().join("fake-firecracker.sh");
        std::fs::write(
            &fake_fc,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 2\n",
                argv.display()
            ),
        )
        .expect("write fake firecracker");
        std::fs::set_permissions(&fake_fc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake firecracker");
        let mut vm = VmManager::new(
            "test-vm".to_string(),
            dir.path().join("api.sock"),
            Some(dir.path().join("firecracker.log")),
            crate::confine::Decision::for_test(None),
        );
        vm.firecracker_log_level = requested.map(str::to_string);
        let started = vm.start(&fake_fc, None, None).await;
        let seen = std::fs::read_to_string(&argv)
            .ok()
            .map(|text| text.lines().map(str::to_string).collect());
        (started, seen)
    }

    fn level_in(arguments: &[String]) -> Option<&str> {
        let at = arguments
            .iter()
            .position(|argument| argument == "--level")?;
        arguments.get(at + 1).map(String::as_str)
    }

    #[tokio::test]
    async fn start_passes_the_log_level_to_firecracker() {
        let (_, seen) = arguments_of_a_start(None).await;
        let seen = seen.expect("the fake Firecracker ran");
        assert_eq!(level_in(&seen), Some("Info"), "{seen:?}");

        let (_, seen) = arguments_of_a_start(Some("debug")).await;
        let seen = seen.expect("the fake Firecracker ran");
        assert_eq!(level_in(&seen), Some("Debug"), "{seen:?}");

        let (started, seen) = arguments_of_a_start(Some("verbose")).await;
        let refused = format!(
            "{:#}",
            started.expect_err("an unknown level must not start a VM")
        );
        assert!(refused.contains("FCVM_FIRECRACKER_LOG_LEVEL"), "{refused}");
        assert!(seen.is_none(), "Firecracker must not be spawned: {seen:?}");
    }

    /// fc-agent's console lines reach a log filtered to fcvm's own targets (RUST_LOG=fcvm=info),
    /// and its errors fcvm's default log ("warn"). They were logged under `firecracker`, outside
    /// `fcvm`, so both filters dropped them (#1104). Kernel messages and vitals samples stay
    /// out of RUST_LOG=fcvm=debug, which job logs use.
    #[test]
    fn agent_console_lines_are_logged_under_fcvm() {
        use crate::utils::log_capture::logged;
        let agent = "[fc-agent] run_agent starting";
        let error = "[fc-agent] Error: mounting FUSE volumes: Read-only file system (os error 30)";
        let kernel = "[    1.234567] virtio_net virtio1 eth0: link up";
        let vitals = "[fcvm-vitals] MemFree: 646880 kB";
        let all = || {
            for line in [agent, error, kernel, vitals] {
                super::log_firecracker_line(line, false);
            }
        };
        let info = logged("fcvm=info", all);
        assert_eq!(info.matches("run_agent starting").count(), 1, "{info:?}");
        assert_eq!(info.matches("mounting FUSE volumes").count(), 1, "{info:?}");
        let default = logged("warn", all);
        assert!(default.contains("mounting FUSE volumes"), "{default:?}");
        assert!(!default.contains("run_agent starting"), "{default:?}");
        let debug = logged("fcvm=debug", all);
        assert!(
            !debug.contains("virtio_net") && !debug.contains("MemFree"),
            "a kernel message or a vitals sample under fcvm: {debug:?}"
        );
        let console = logged("firecracker=debug", all);
        assert!(
            console.contains("virtio_net") && console.contains("MemFree"),
            "{console:?}"
        );
    }

    /// A guest kernel's panic line reaches fcvm's default log (RUST_LOG unset is "warn") and a
    /// log filtered to fcvm's own targets (RUST_LOG=fcvm=info), exactly once: never also under the
    /// console's own `firecracker` target. A guest that panics resets, and Firecracker's own log
    /// says only that it did, so this line is the one record of why. The stack frames under it
    /// stay out.
    #[test]
    fn a_guest_kernel_panic_line_is_logged_once_at_warn_under_fcvm() {
        use crate::utils::log_capture::logged;
        let panic_line = "[    9.814619] Kernel panic - not syncing: sysrq triggered crash";
        let frame = "[    9.816260]  sysrq_handle_crash+0x15/0x20";
        for filter in ["warn", "fcvm=info"] {
            let log = logged(filter, || {
                super::log_firecracker_line(panic_line, false);
                super::log_firecracker_line(frame, false);
            });
            assert_eq!(
                log.matches("Kernel panic - not syncing").count(),
                1,
                "the panic line in a log filtered to {filter}: {log:?}"
            );
            assert!(
                !log.contains("sysrq_handle_crash"),
                "a stack frame at {filter}: {log:?}"
            );
        }
        let console_target = logged("firecracker=trace", || {
            super::log_firecracker_line(panic_line, false)
        });
        assert!(
            !console_target.contains("Kernel panic"),
            "the panic line was also logged under the console's target: {console_target:?}"
        );
    }

    /// A snapshot memory file for the mark tests.
    fn memory_file(dir: &Path) -> PathBuf {
        let path = dir.join("memory.bin");
        std::fs::write(&path, vec![0xA5u8; 64 * 1024]).expect("write memory file");
        path
    }

    /// Whether something holds `path` in use: a release pass's exclusive lock is refused.
    fn marked(path: &Path) -> bool {
        let pass = std::fs::File::open(path).expect("open as a release pass does");
        match fs2::FileExt::try_lock_exclusive(&pass) {
            Ok(()) => {
                fs2::FileExt::unlock(&pass).expect("unlock");
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(error) => panic!("locking {}: {error}", path.display()),
        }
    }

    /// Whether this process has `path` open: a deleted snapshot's memory file stays on
    /// disk for as long as a descriptor of it does.
    fn open_here(path: &Path) -> bool {
        let path = path.canonicalize().expect("canonical memory file path");
        std::fs::read_dir("/proc/self/fd")
            .expect("list this process's descriptors")
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == path))
    }

    /// Close a live `cat` child's stdin, so it exits on its own.
    fn let_child_exit(vm: &mut VmManager) {
        drop(
            vm.process
                .as_mut()
                .expect("a live child")
                .stdin
                .take()
                .expect("piped stdin"),
        );
    }

    /// A guest reboot ends the VMM process and the relaunch is a cold boot in the same fcvm
    /// process (#1068): the run loop's `wait` sees the exit. The mark ends with the process.
    #[tokio::test]
    async fn the_memory_mark_ends_when_the_vmm_exits() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let memory = memory_file(dir.path());
        let mut vm = manager_with_live_child(dir.path().join("api.sock"));
        assert!(!marked(&memory), "control: nothing holds the file yet");

        vm.mark_memory_in_use(&memory).await.expect("mark");
        assert!(marked(&memory), "a running VMM's memory file is not marked");

        let_child_exit(&mut vm);
        vm.wait().await.expect("wait for the exit");
        assert!(!marked(&memory), "the mark outlived the VMM process");
        assert!(!open_here(&memory), "the memory file is still open");
    }

    /// The same through `try_wait`, which the restore and the clone run loop poll.
    #[tokio::test]
    async fn the_memory_mark_ends_when_try_wait_sees_the_exit() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let memory = memory_file(dir.path());
        let mut vm = manager_with_live_child(dir.path().join("api.sock"));
        vm.mark_memory_in_use(&memory).await.expect("mark");

        let_child_exit(&mut vm);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while vm.try_wait().expect("try_wait").is_none() {
            assert!(std::time::Instant::now() < deadline, "cat did not exit");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!marked(&memory), "the mark outlived the VMM process");
        assert!(!open_here(&memory), "the memory file is still open");
    }

    /// A snapshot load that Firecracker rejects ends with the restore killing the VMM, and
    /// the cache path then deletes the snapshot and cold-boots in the same process (#1068).
    /// The kill ends the mark and closes the file, so the deleted file does not stay open.
    #[tokio::test]
    async fn the_memory_mark_ends_when_the_vmm_is_killed() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let memory = memory_file(dir.path());
        let mut vm = manager_with_live_child(dir.path().join("api.sock"));
        vm.mark_memory_in_use(&memory).await.expect("mark");

        vm.kill().await.expect("kill");
        assert!(!marked(&memory), "the mark outlived the killed VMM");
        assert!(!open_here(&memory), "the memory file is still open");
    }

    /// A relaunch starts another VMM process, which maps no snapshot: it does not inherit
    /// the mark of the one before it, whether or not the start succeeds.
    #[tokio::test]
    async fn a_relaunch_does_not_inherit_the_memory_mark() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let memory = memory_file(dir.path());
        let fake_fc = dir.path().join("fake-firecracker.sh");
        std::fs::write(&fake_fc, "#!/bin/sh\nexit 2\n").expect("write fake firecracker");
        std::fs::set_permissions(&fake_fc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake firecracker");
        let mut vm = manager_with_live_child(dir.path().join("api.sock"));
        vm.mark_memory_in_use(&memory).await.expect("mark");
        let mut previous = vm.process.take().expect("the live child");

        let _ = vm.start(&fake_fc, None, None).await;
        assert!(!marked(&memory), "the relaunched VMM inherited the mark");
        assert!(!open_here(&memory), "the memory file is still open");
        previous.kill().await.expect("stop the previous child");
    }

    /// A manager with no VMM process has nothing to give the mark a lifetime.
    #[tokio::test]
    async fn a_memory_mark_needs_a_running_vmm() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let memory = memory_file(dir.path());
        let mut vm = VmManager::new(
            "mark-test".to_string(),
            dir.path().join("api.sock"),
            None,
            crate::confine::Decision::for_test(None),
        );
        vm.mark_memory_in_use(&memory)
            .await
            .expect_err("a mark without a VMM process");
        assert!(!marked(&memory));
        assert!(!open_here(&memory));
    }

    /// fc-agent's FATAL and WARNING lines say why a guest is about to shut itself down or what
    /// it skipped. A caller that runs fcvm with RUST_LOG=fcvm=info kept none of them while they
    /// were logged under the console's `firecracker` target, so a failed restore showed only the
    /// host's side (#1080). They are logged once, at WARN under fcvm::guest; fc-agent's other
    /// lines stay at INFO under the console's target.
    #[test]
    fn fc_agent_fatal_and_warning_lines_reach_a_log_filtered_to_fcvm() {
        use crate::utils::log_capture::logged;
        let fatal =
            "[fc-agent] FATAL: restore-completion ACK failed closed (epoch=e phase=notify-host)";
        let warning = "[fc-agent] WARNING: failed to set clock: EPERM";
        let ordinary = "[fc-agent] remounting 1 NFS share(s) after restore";
        for filter in ["warn", "fcvm=info"] {
            let log = logged(filter, || {
                super::log_firecracker_line(fatal, false);
                super::log_firecracker_line(warning, false);
                super::log_firecracker_line(ordinary, false);
            });
            assert_eq!(
                log.matches("FATAL: restore-completion").count(),
                1,
                "{filter}: {log:?}"
            );
            assert_eq!(
                log.matches("WARNING: failed to set clock").count(),
                1,
                "{filter}: {log:?}"
            );
            // fc-agent's other lines are INFO under fcvm::console, below the default filter.
            if filter == "warn" {
                assert!(!log.contains("remounting"), "{filter}: {log:?}");
            }
        }
        let console_target = logged("firecracker=trace", || {
            super::log_firecracker_line(fatal, false);
            super::log_firecracker_line(ordinary, false);
        });
        assert!(!console_target.contains("FATAL"), "{console_target:?}");
    }
}
