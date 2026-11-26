//! Opt-in confinement of a VM's VMM process (`--confine`).
//!
//! Without it, the only boundary between a guest and the host is KVM plus the VMM's device
//! emulation: code that escapes into the Firecracker process holds the host user's account
//! and, in rootless mode, every capability inside the namespace holder's user namespace.
//! Confinement starts the VMM so that such code can do as little as possible. The layers
//! arrive one pull request at a time, and [`Confinement::layers`] records which ones a VM
//! was started with:
//!
//! - [`Layer::Process`] (this module, [`apply_process_layer`]): no capabilities, none to
//!   regain across exec, no core dumps, `no_new_privs`.
//!
//! Default starts are untouched: with confinement off no pre_exec step and no VMM argument
//! is added.
//!
//! # Fail closed
//!
//! A start that asks for confinement (the flag, or the host's `required` mode) and cannot
//! have all of it is refused before anything is created, with every missing piece listed
//! by mechanism ([`Missing`]). Nothing falls back to an unconfined start.
//!
//! # Kernel floor
//!
//! Confinement needs Landlock ABI [`LANDLOCK_ABI_FLOOR`]: ABI 9 added
//! `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`, without which Landlock does not mediate `connect()`
//! to a pathname Unix socket, and the VMM must keep `connect()` for vsock. The floor is
//! checked from the first layer on, so the flag means the same thing on every kernel it is
//! accepted on. A second tier for older kernels (a private PID and mount namespace in place
//! of `RESOLVE_UNIX`) would be another entry under [`Mechanism::Landlock`], not a new flag.

use serde::{Deserialize, Serialize};

/// The lowest Landlock ABI confinement accepts (see the module documentation).
pub const LANDLOCK_ABI_FLOOR: u32 = 9;

/// The host's confinement mode: the `[confine] mode` config key, tightened by
/// `FCVM_CONFINE`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Each start is confined when it asks with `--confine`.
    #[default]
    Off,
    /// Every start is confined, and one that cannot be fails.
    Required,
}

impl std::str::FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "off" => Ok(Mode::Off),
            "required" => Ok(Mode::Required),
            other => anyhow::bail!("unknown confinement mode {other:?}: expected off or required"),
        }
    }
}

/// The `[confine]` table of the fcvm config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct ConfineConfig {
    #[serde(default)]
    pub mode: Mode,
    /// Whether `FCVM_CONFINE_SECCOMP=log` is accepted on a host whose mode is `required`.
    /// Log mode allows every syscall, so by default it does not satisfy `required`.
    #[serde(default)]
    pub allow_seccomp_log: bool,
}

static CONFIG_MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
static CONFIG_ALLOW_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Record the config's `[confine] allow_seccomp_log`; called with [`set_config_mode`].
pub fn set_config_allow_seccomp_log(allow: bool) {
    let _ = CONFIG_ALLOW_LOG.set(allow);
}

/// The config's `[confine] allow_seccomp_log`: the recorded one, else read from the config
/// file, as [`config_mode`] does for the mode.
fn config_allow_seccomp_log() -> anyhow::Result<bool> {
    match CONFIG_ALLOW_LOG.get() {
        Some(allow) => Ok(*allow),
        None => {
            let (config, _, _) = crate::setup::rootfs::load_config(None)
                .map_err(|e| e.context("reading the fcvm config's [confine] table"))?;
            Ok(config.confine.allow_seccomp_log)
        }
    }
}

/// Record the config's mode; called once by [`crate::paths::init_from_loaded_config`].
pub fn set_config_mode(mode: Mode) {
    let _ = CONFIG_MODE.set(mode);
}

/// The config's mode: the one [`set_config_mode`] recorded, or, when nothing did (a library
/// embedder, a command that set its paths another way), the one `load` reads from the config
/// file. An unset mode is never taken as off.
fn config_mode(
    recorded: Option<Mode>,
    load: impl FnOnce() -> anyhow::Result<Mode>,
) -> anyhow::Result<Mode> {
    match recorded {
        Some(mode) => Ok(mode),
        None => load().map_err(|e| e.context("reading the fcvm config's [confine] mode")),
    }
}

fn load_config_mode() -> anyhow::Result<Mode> {
    let (config, _, _) = crate::setup::rootfs::load_config(None)?;
    Ok(config.confine.mode)
}

/// The environment variable that can make a host's mode stricter than its config.
pub const MODE_ENV: &str = "FCVM_CONFINE";

/// The host's mode: the config's, unless `FCVM_CONFINE` asks for a stricter one. An
/// environment value that would loosen the config is an error, not ignored.
pub fn host_mode(config: Mode, env: Option<&str>) -> anyhow::Result<Mode> {
    let Some(env) = env else {
        return Ok(config);
    };
    let from_env: Mode = env
        .parse()
        .map_err(|e| anyhow::anyhow!("{MODE_ENV}={env}: {e}"))?;
    anyhow::ensure!(
        from_env >= config,
        "{MODE_ENV}={env} cannot loosen the fcvm config's [confine] mode = \"required\""
    );
    Ok(from_env)
}

/// A layer of confinement, recorded in VM state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// No capabilities and none to regain, `no_new_privs`, no core dumps.
    Process,
    /// The VMM's syscalls are an allowlist ([`SeccompMode::Enforce`]).
    Seccomp,
    /// The allowlist is installed in log mode ([`SeccompMode::Log`]): every syscall is
    /// allowed and the ones outside the list are recorded. Not a restriction.
    #[serde(rename = "seccomp-log")]
    SeccompLog,
}

/// What the confined seccomp policy does with a syscall it does not list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeccompMode {
    /// `SECCOMP_RET_TRAP`: Firecracker's SIGSYS handler logs the thread and the syscall
    /// number and shuts the VM down, so a syscall outside the policy stops the VMM.
    Enforce,
    /// `SECCOMP_RET_LOG`: allowed, and recorded by the kernel's audit subsystem. For
    /// finding what the policy misses, never for running workloads.
    Log,
}

impl SeccompMode {
    pub const ALL: [SeccompMode; 2] = [SeccompMode::Enforce, SeccompMode::Log];

    fn action(self) -> &'static str {
        match self {
            SeccompMode::Enforce => "trap",
            SeccompMode::Log => "log",
        }
    }

    fn name(self) -> &'static str {
        match self {
            SeccompMode::Enforce => "enforce",
            SeccompMode::Log => "log",
        }
    }
}

/// Selects [`SeccompMode`]: `enforce` (the default) or `log`.
pub const SECCOMP_ENV: &str = "FCVM_CONFINE_SECCOMP";

/// The confined policy for this architecture, in Firecracker's seccompiler JSON: per thread
/// class (`vmm`, `api`, `vcpu`), the syscalls Firecracker uses in fcvm.
#[cfg(target_arch = "aarch64")]
const SECCOMP_POLICY: &str = include_str!("../resources/seccomp/firecracker-confined-aarch64.json");
#[cfg(target_arch = "x86_64")]
const SECCOMP_POLICY: &str = include_str!("../resources/seccomp/firecracker-confined-x86_64.json");
/// The Firecracker commit whose shipped seccomp filters the confined policies are derived
/// from (resources/seccomp/confined-changes.json); `[firecracker] commit` in
/// rootfs-config.toml must be this commit. A VMM built from another commit is not confined:
/// its syscalls were never checked against the policy.
pub const CONFINED_FIRECRACKER_COMMIT: &str = "cf5d302d87823789dfdc4df30bfb98c10371cb6f";

/// The environment a confined VMM starts with, on top of fcvm's own.
///
/// `MALLOC_ARENA_MAX=1`: the policy is the filter of a musl-built Firecracker, and fcvm runs
/// a glibc build. musl's allocator grows the heap with brk and mmap, which every thread
/// class lists. glibc gives each thread but the first an arena of its own and grows it with
/// mprotect, which the api and vcpu classes do not list: a boot plan of 22 KB grew the
/// API thread's arena while Firecracker parsed it, and under the policy that call stops the
/// VMM. With one arena every thread allocates from the first, which grows with brk, as
/// musl's heap does.
///
/// `TZ=<UTC>0`: glibc reads the zone file (`/etc/localtime`) at the first log line of a
/// process, with fstat, read and lseek, where musl maps it. At Firecracker's Warning and
/// Error log levels that line is written under a filter, by whichever thread has it, and the
/// api and vcpu classes list no lseek (vcpu no read either). A POSIX zone string that names
/// no file costs one failed openat, which every class lists, and no read: measured with
/// strace on glibc 2.39. Firecracker's own log is then stamped in UTC.
pub const CONFINED_VMM_ENV: &[(&str, &str)] = &[("MALLOC_ARENA_MAX", "1"), ("TZ", "<UTC>0")];

/// `seccompiler-bin --target-arch` for this architecture.
#[cfg(target_arch = "aarch64")]
pub const SECCOMP_ARCH: &str = "aarch64";
#[cfg(target_arch = "x86_64")]
pub const SECCOMP_ARCH: &str = "x86_64";

/// The policy with every thread class's default action set for `mode`.
pub fn seccomp_policy(mode: SeccompMode) -> String {
    let mut policy: serde_json::Value =
        serde_json::from_str(SECCOMP_POLICY).expect("the embedded seccomp policy is JSON");
    for (_, class) in policy
        .as_object_mut()
        .expect("the seccomp policy is a map of thread classes")
        .iter_mut()
    {
        class["default_action"] = serde_json::Value::from(mode.action());
    }
    serde_json::to_string_pretty(&policy).expect("serializing JSON")
}

/// Whether the confined policy was derived from the Firecracker at `commit`. `fcvm setup`
/// compiles the filter only beside a build of that commit, and a confined launch refuses a
/// binary without its filter, so no other build runs confined, whichever way it was chosen:
/// a kernel profile, `--firecracker-bin`, or the binary a snapshot recorded.
pub fn policy_is_for(commit: &str) -> bool {
    commit == CONFINED_FIRECRACKER_COMMIT
}

/// The digest in a filter's file name: of the Firecracker commit the policy was derived from
/// and of the policy, so a repin and a policy change each make a new file, and a binary
/// built before either is left without one.
fn filter_digest(commit: &str, policy: &str) -> String {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    hash.update(commit.as_bytes());
    hash.update(b"\n");
    hash.update(policy.as_bytes());
    hex::encode(hash.finalize())[..16].to_string()
}

/// The compiled filter for `mode`, beside the content-addressed `firecracker` it was built
/// for, named after [`filter_digest`].
pub fn filter_path(firecracker: &std::path::Path, mode: SeccompMode) -> std::path::PathBuf {
    let digest = filter_digest(CONFINED_FIRECRACKER_COMMIT, &seccomp_policy(mode));
    let mut path = firecracker.as_os_str().to_owned();
    path.push(format!(".confined-{}-{digest}.bpf", mode.name()));
    path.into()
}

/// A syscall the confined policy stopped: the Firecracker thread that made it and its number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrappedSyscall {
    pub thread: String,
    pub number: u64,
}

/// The last "bad syscall" line of a Firecracker log, which its SIGSYS handler writes before
/// it shuts the VM down: `[<instance>:<thread>:ERROR:<file>:<line>] Shutting down VM after
/// intercepting a bad syscall (<number>).`
pub fn trapped_syscall(log: &str) -> Option<TrappedSyscall> {
    const MARK: &str = "intercepting a bad syscall (";
    let line = log.lines().rev().find(|line| line.contains(MARK))?;
    let rest = &line[line.find(MARK)? + MARK.len()..];
    let number = rest[..rest.find(')')?].parse().ok()?;
    let tag = &line[line.find('[')? + 1..];
    let thread = tag[..tag.find(']')?]
        .split(':')
        .nth(1)
        .unwrap_or("")
        .to_string();
    Some(TrappedSyscall { thread, number })
}

/// What fcvm logs when the confined policy stopped a VMM, naming the thread and the syscall.
pub fn trapped_syscall_report(trapped: &TrappedSyscall) -> String {
    format!(
        "Firecracker was stopped by its confinement policy: its {} thread made syscall {}, \
         which resources/seccomp/firecracker-confined-{}.json does not allow. A syscall \
         outside the policy stops the VMM; {SECCOMP_ENV}=log runs the workload with such \
         syscalls recorded by the kernel's audit log instead, to check it before enforcing",
        if trapped.thread.is_empty() {
            "(unnamed)"
        } else {
            &trapped.thread
        },
        trapped.number,
        SECCOMP_ARCH,
    )
}

/// `seccompiler-bin` built from the same Firecracker commit as `firecracker`.
pub fn seccompiler_path(firecracker: &std::path::Path) -> std::path::PathBuf {
    let mut path = firecracker.as_os_str().to_owned();
    path.push(".seccompiler-bin");
    path.into()
}

/// How a VM's VMM was confined, recorded in its state and shown by `fcvm ls`. It is a
/// property of the VMM process: it enters no snapshot, cache or pool key, so a confined
/// clone restores a snapshot an unconfined VMM wrote and the reverse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confinement {
    /// The kernel's Landlock ABI when the VM started.
    pub landlock_abi: u32,
    pub layers: Vec<Layer>,
    /// The RLIMIT_CORE the process layer installs, decided at the preflight from the
    /// inherited hard limit and the core_pattern ([`core_limit_for`]). The spawn installs
    /// exactly this, and fails if it cannot, rather than read the limit again.
    #[serde(default)]
    pub core_limit: u64,
    /// What the seccomp layer does with an unlisted syscall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp: Option<SeccompMode>,
}

/// What a missing piece belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    /// The kernel's Landlock support.
    Landlock,
    /// The VMM backend.
    Backend,
    /// The network mode.
    Network,
    /// A debugging setting that needs the VMM unconfined.
    Debugging,
    /// The process layer itself.
    Process,
    /// The seccomp filter or its mode.
    Seccomp,
    /// The command itself.
    Command,
}

impl std::fmt::Display for Mechanism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mechanism::Landlock => "landlock",
            Mechanism::Backend => "backend",
            Mechanism::Network => "network",
            Mechanism::Debugging => "debugging",
            Mechanism::Process => "process",
            Mechanism::Seccomp => "seccomp",
            Mechanism::Command => "command",
        })
    }
}

/// One piece of confinement a start cannot have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub mechanism: Mechanism,
    pub detail: String,
}

/// The network mode of the VM being started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Rootless,
    Bridged,
    Routed,
}

impl From<crate::cli::NetworkMode> for Network {
    fn from(mode: crate::cli::NetworkMode) -> Self {
        match mode {
            crate::cli::NetworkMode::Rootless => Network::Rootless,
            crate::cli::NetworkMode::Bridged => Network::Bridged,
            crate::cli::NetworkMode::Routed => Network::Routed,
        }
    }
}

impl From<crate::firecracker::config::NetworkMode> for Network {
    fn from(mode: crate::firecracker::config::NetworkMode) -> Self {
        match mode {
            crate::firecracker::config::NetworkMode::Rootless => Network::Rootless,
            crate::firecracker::config::NetworkMode::Bridged => Network::Bridged,
            crate::firecracker::config::NetworkMode::Routed => Network::Routed,
        }
    }
}

/// One start, as confinement sees it.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// `--confine` was given.
    pub flag: bool,
    pub backend: crate::hypervisor::Backend,
    pub network: Network,
    /// The extra Firecracker arguments the VMM would get (`FCVM_FIRECRACKER_ARGS` included).
    pub firecracker_args: Option<&'a str>,
    /// Why this command cannot confine its VMM at all, when it cannot.
    pub unsupported: Option<&'static str>,
    /// The kernel profile the VMM starts with (`None`: default); it names the Firecracker
    /// build the VMM runs.
    pub kernel_profile: Option<&'a str>,
}

/// What the host offers, read once per start.
#[derive(Debug, Clone)]
pub struct Host {
    pub mode: Mode,
    /// The Landlock ABI, or the errno `landlock_create_ruleset` failed with.
    pub landlock_abi: Result<u32, i32>,
    pub strace_fc: bool,
    pub no_seccomp: bool,
    /// `FCVM_FIRECRACKER_ARGS`.
    pub firecracker_args: Option<String>,
    /// `/proc/sys/kernel/core_pattern`, or `None` when it cannot be read. It is read once,
    /// at the preflight: an administrator who later switches the pattern (to a socket, or to
    /// a pipe while the inherited hard core limit is 0) changes what VMMs already running
    /// can dump.
    pub core_pattern: Option<String>,
    /// This process's hard RLIMIT_CORE, which the VMM inherits: an unprivileged process
    /// cannot raise it.
    pub core_hard_limit: u64,
    /// `FCVM_CONFINE_SECCOMP`.
    pub seccomp: SeccompMode,
    /// The commit of the Firecracker the start's kernel profile runs, when the profile names
    /// one ([`CONFINED_FIRECRACKER_COMMIT`] for the default build).
    pub firecracker_commit: Option<String>,
    /// Whether log mode is acceptable here: the config's mode is `off` (whoever sets
    /// `FCVM_CONFINE=required` in the environment could as well not set it, so log mode
    /// loosens nothing the config asks for), or the config says
    /// `[confine] allow_seccomp_log = true`.
    pub allow_seccomp_log: bool,
}

/// The host's mode: the config's, tightened by `FCVM_CONFINE`.
pub fn read_mode() -> anyhow::Result<Mode> {
    let env = mode_env(std::env::var(MODE_ENV))?;
    host_mode(
        config_mode(CONFIG_MODE.get().copied(), load_config_mode)?,
        env.as_deref(),
    )
}

/// `FCVM_CONFINE` as read: unset is `None`; a value that is not valid UTF-8 is an error,
/// never taken as unset.
fn mode_env(var: Result<String, std::env::VarError>) -> anyhow::Result<Option<String>> {
    match var {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{MODE_ENV} is set to a value that is not valid UTF-8")
        }
    }
}

/// Whether a start needs the rest of the host read (the Landlock probe among it): only one
/// that asks for confinement or runs on a host that requires it. An unconfined start on an
/// `off` host makes no Landlock syscall at all.
fn needs_host_read(flag: bool, mode: Mode) -> bool {
    flag || mode != Mode::Off
}

impl Host {
    /// Read this host for a start under `mode`: the kernel's Landlock ABI, the core
    /// pattern and the debugging environment variables.
    pub fn read(mode: Mode, kernel_profile: Option<&str>) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let profile = kernel_profile.unwrap_or("default");
        let firecracker_commit = crate::setup::get_kernel_profile(profile)
            .with_context(|| format!("reading kernel profile {profile} for confinement"))?
            .and_then(|profile| profile.firecracker_commit);
        Ok(Host {
            allow_seccomp_log: config_allow_seccomp_log()?
                || config_mode(CONFIG_MODE.get().copied(), load_config_mode)? == Mode::Off,
            firecracker_commit,
            mode,
            landlock_abi: landlock_abi(),
            core_pattern: std::fs::read_to_string("/proc/sys/kernel/core_pattern")
                .ok()
                .map(|p| p.trim_end().to_string()),
            core_hard_limit: core_hard_limit(),
            strace_fc: std::env::var_os("FCVM_STRACE_FC").is_some(),
            no_seccomp: std::env::var_os("FCVM_NO_SECCOMP").is_some(),
            firecracker_args: std::env::var("FCVM_FIRECRACKER_ARGS").ok(),
            seccomp: match std::env::var(SECCOMP_ENV).as_deref() {
                Err(_) | Ok("enforce") => SeccompMode::Enforce,
                Ok("log") => SeccompMode::Log,
                Ok(other) => anyhow::bail!("{SECCOMP_ENV}={other}: expected enforce or log"),
            },
        })
    }
}

/// The kernel's Landlock ABI version, or the errno: EOPNOTSUPP when Landlock is built but
/// not enabled, ENOSYS when it is not built.
pub fn landlock_abi() -> Result<u32, i32> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: with a null attribute and size 0 the call only reports the version.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(abi as u32)
    }
}

/// Every piece of confinement `request` cannot have on `host`.
pub fn missing(request: &Request<'_>, host: &Host) -> Vec<Missing> {
    let mut missing = Vec::new();
    let mut add = |mechanism, detail: String| missing.push(Missing { mechanism, detail });
    if let Some(reason) = request.unsupported {
        add(Mechanism::Command, reason.to_string());
    }
    match host.landlock_abi {
        Ok(abi) if abi >= LANDLOCK_ABI_FLOOR => {}
        Ok(abi) => add(
            Mechanism::Landlock,
            format!(
                "the kernel's Landlock ABI is {abi}; confinement needs ABI {LANDLOCK_ABI_FLOOR} \
                 or later, whose RESOLVE_UNIX right restricts connect() to pathname Unix sockets"
            ),
        ),
        Err(errno) => add(
            Mechanism::Landlock,
            format!(
                "Landlock is unavailable ({}); confinement needs Landlock ABI \
                 {LANDLOCK_ABI_FLOOR} or later",
                std::io::Error::from_raw_os_error(errno)
            ),
        ),
    }
    if request.backend != crate::hypervisor::Backend::Firecracker {
        add(
            Mechanism::Backend,
            "confinement supports the Firecracker backend only".to_string(),
        );
    }
    if let Some(commit) = host
        .firecracker_commit
        .as_deref()
        .filter(|commit| *commit != CONFINED_FIRECRACKER_COMMIT)
    {
        add(
            Mechanism::Backend,
            format!(
                "kernel profile {} runs Firecracker {commit}; the confined syscall policy is \
                 derived from Firecracker {CONFINED_FIRECRACKER_COMMIT}, and another build's \
                 syscalls were never checked against it",
                request.kernel_profile.unwrap_or("default")
            ),
        );
    }
    match request.network {
        Network::Rootless => {}
        Network::Bridged | Network::Routed => add(
            Mechanism::Network,
            format!(
                "--network {} runs the VMM as root; confinement supports --network rootless \
                 only until a later change drops a root VMM to an unprivileged uid before exec",
                if request.network == Network::Bridged {
                    "bridged"
                } else {
                    "routed"
                }
            ),
        ),
    }
    if host.strace_fc {
        add(
            Mechanism::Debugging,
            "FCVM_STRACE_FC runs the VMM under strace, which must fork and ptrace".to_string(),
        );
    }
    if host.seccomp == SeccompMode::Log && !host.allow_seccomp_log {
        add(
            Mechanism::Seccomp,
            format!(
                "{SECCOMP_ENV}=log allows every syscall and only records the ones outside \
                 the allowlist, and this host's config requires confinement; unset it, or \
                 set [confine] allow_seccomp_log = true in the fcvm config to accept log \
                 mode here"
            ),
        );
    }
    if host.no_seccomp {
        add(
            Mechanism::Debugging,
            "FCVM_NO_SECCOMP turns the VMM's seccomp filter off".to_string(),
        );
    }
    for args in [request.firecracker_args, host.firecracker_args.as_deref()]
        .into_iter()
        .flatten()
    {
        for detail in overriding_args(args) {
            add(Mechanism::Debugging, detail);
        }
    }
    // The process layer sets RLIMIT_CORE to 1, which the kernel takes as "no dump" for a file
    // or a pipe core_pattern (fs/coredump.c). A socket core_pattern ("@...") ignores it.
    match host.core_pattern.as_deref() {
        // A pipe ignores a core limit of 0; only 1 stops it, and an inherited hard limit
        // of 0 cannot be raised to 1 without CAP_SYS_RESOURCE.
        Some(pattern) if pattern.starts_with('|') && host.core_hard_limit < 1 => add(
            Mechanism::Process,
            format!(
                "the host's core_pattern ({pattern}) pipes core dumps, which only a core limit \
                 of 1 stops, and this process's hard core limit is 0, which it cannot raise"
            ),
        ),
        Some(pattern) if !pattern.starts_with('@') => {}
        Some(pattern) => add(
            Mechanism::Process,
            format!(
                "the host's core_pattern ({pattern}) sends core dumps to a socket, which \
                 ignores RLIMIT_CORE, so a crashed VMM's memory could leave the process"
            ),
        ),
        None => add(
            Mechanism::Process,
            "the host's core_pattern cannot be read, so whether a crashed VMM's memory \
             could be dumped cannot be told"
                .to_string(),
        ),
    }
    missing
}

/// This process's hard RLIMIT_CORE.
pub fn core_hard_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes the struct, which outlives the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) } != 0 {
        return 0;
    }
    limit.rlim_max
}

/// The core limit the process layer sets, given the inherited hard limit: 1, which the
/// kernel takes as "no dump" for a pipe as well as a file, unless the hard limit is already
/// 0, which cannot be raised and stops a file dump (a pipe one is refused in the preflight).
pub fn core_limit_for(hard: u64) -> libc::rlim_t {
    if hard >= 1 {
        1
    } else {
        0
    }
}

/// What in the extra Firecracker arguments `args` confinement refuses: the options that
/// turn seccomp off or set a filter of their own.
pub fn overriding_args(args: &str) -> Vec<String> {
    ["--no-seccomp", "--seccomp-filter"]
        .iter()
        .filter(|arg| args.split_whitespace().any(|a| a == **arg))
        .map(|arg| format!("the extra Firecracker arguments set {arg}, which confinement owns"))
        .collect()
}

/// Decide whether this start is confined. `Ok(None)` is an unconfined start the host
/// allows; an error lists everything a start that must be confined is missing.
pub fn decide(request: &Request<'_>, host: &Host) -> anyhow::Result<Option<Confinement>> {
    if !request.flag && host.mode == Mode::Off {
        return Ok(None);
    }
    let missing = missing(request, host);
    if missing.is_empty() {
        return Ok(Some(Confinement {
            landlock_abi: host.landlock_abi.unwrap_or_default(),
            core_limit: core_limit_for(host.core_hard_limit),
            layers: vec![
                Layer::Process,
                match host.seccomp {
                    SeccompMode::Enforce => Layer::Seccomp,
                    SeccompMode::Log => Layer::SeccompLog,
                },
            ],
            seccomp: Some(host.seccomp),
        }));
    }
    let why = if request.flag {
        "--confine was given"
    } else {
        "the host requires confinement ([confine] mode or FCVM_CONFINE = required)"
    };
    let list: String = missing
        .iter()
        .map(|m| format!("\n  {}: {}", m.mechanism, m.detail))
        .collect();
    anyhow::bail!("{why}, and this start cannot be confined; nothing was started:{list}")
}

/// [`decide`] on this host.
pub fn resolve(request: &Request<'_>) -> anyhow::Result<Decision> {
    let mode = read_mode()?;
    if !needs_host_read(request.flag, mode) {
        return Ok(Decision(None));
    }
    let host = Host::read(mode, request.kernel_profile)?;
    let decided = decide(request, &host)?;
    if let Some(confinement) = &decided {
        tracing::info!(
            landlock_abi = confinement.landlock_abi,
            layers = ?confinement.layers,
            "the VMM will be confined"
        );
        if confinement.seccomp == Some(SeccompMode::Log) {
            tracing::warn!(
                "{SECCOMP_ENV}=log: this VMM's syscall allowlist is installed in log mode, \
                 which allows every syscall and records the ones outside the list; it \
                 restricts nothing"
            );
        }
    }
    Ok(Decision(decided))
}

/// Where `fcvm setup` records the SHA-256 of the compiled filter at `filter`.
///
/// The record sits beside the filter and has the same owner, so it does not stop a process
/// of that user from replacing both. It catches a filter that is damaged, truncated, or
/// replaced without its record, and it lets `fcvm setup` rebuild one that no longer matches.
pub fn filter_digest_path(filter: &std::path::Path) -> std::path::PathBuf {
    let mut path = filter.as_os_str().to_owned();
    path.push(".sha256");
    path.into()
}

/// The SHA-256 of a compiled filter's bytes, as hex.
pub fn filter_bytes_digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The largest filter or record [`read_store_file`] reads. A compiled policy is about 3 KiB.
const FILTER_READ_LIMIT: u64 = 1 << 20;

/// The bytes of a store file a confined launch depends on, opened once, without following a
/// symbolic link and without waiting (a FIFO under the name would otherwise hold the start
/// until something wrote to it): a regular file that others cannot write, owned by this
/// user, by root, or by `binary_owner`, the owner of the Firecracker binary the file is for.
/// That owner already decides what the VMM runs (under `sudo fcvm` the store belongs to the
/// user who ran `fcvm setup`), so accepting its filter gives it nothing it lacks.
fn read_store_file(path: &std::path::Path, binary_owner: u32) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context as _;
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening {} (a symbolic link is refused)", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("reading the metadata of {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "{} is not a regular file", path.display());
    // SAFETY: geteuid has no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        meta.uid() == euid || meta.uid() == 0 || meta.uid() == binary_owner,
        "{} is owned by uid {}, not by this user ({euid}), root, or the owner of the \
         Firecracker binary (uid {binary_owner})",
        path.display(),
        meta.uid()
    );
    anyhow::ensure!(
        meta.mode() & 0o002 == 0,
        "{} is writable by every user (mode {:o})",
        path.display(),
        meta.mode() & 0o7777
    );
    anyhow::ensure!(
        meta.len() <= FILTER_READ_LIMIT,
        "{} is {} bytes, more than a compiled filter can be",
        path.display(),
        meta.len()
    );
    let mut bytes = Vec::new();
    file.take(FILTER_READ_LIMIT + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(bytes)
}

/// The compiled filter for `firecracker` in `mode`, read once and checked against the
/// digest `fcvm setup` recorded for it ([`filter_digest_path`]).
pub fn verified_filter_bytes(
    firecracker: &std::path::Path,
    mode: SeccompMode,
) -> anyhow::Result<Vec<u8>> {
    let filter = filter_path(firecracker, mode);
    let hint = format!(
        "`fcvm setup` compiles it, with its digest, beside each Firecracker it builds from \
         commit {CONFINED_FIRECRACKER_COMMIT}, which the confined policy is derived from: \
         run it if {} is that build; a Firecracker of another commit cannot run confined",
        firecracker.display()
    );
    let binary_owner = {
        use anyhow::Context as _;
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(firecracker)
            .with_context(|| format!("reading the metadata of {}", firecracker.display()))?
            .uid()
    };
    let bytes = read_store_file(&filter, binary_owner)
        .map_err(|e| e.context(format!("the confined seccomp filter is unusable. {hint}")))?;
    let recorded = read_store_file(&filter_digest_path(&filter), binary_owner).map_err(|e| {
        e.context(format!(
            "the digest fcvm setup records for the confined seccomp filter is unusable. {hint}"
        ))
    })?;
    let recorded = String::from_utf8_lossy(&recorded);
    let actual = filter_bytes_digest(&bytes);
    anyhow::ensure!(
        recorded.trim() == actual,
        "the confined seccomp filter {} has SHA-256 {actual}, and fcvm setup recorded {}: \
         the file is not the one setup compiled. Run `fcvm setup`, which rebuilds it",
        filter.display(),
        recorded.trim()
    );
    Ok(bytes)
}

/// `bytes` in a sealed memfd, for the VMM to read as `/proc/self/fd/<n>`: what the VMM loads
/// is what was verified, whatever happens to the file afterwards.
pub fn sealed_memfd(bytes: &[u8]) -> anyhow::Result<std::os::fd::OwnedFd> {
    use anyhow::Context as _;
    use std::io::Write as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    // SAFETY: the name is a NUL-terminated literal; the call returns a new descriptor or -1.
    let fd = unsafe {
        libc::memfd_create(
            c"fcvm-seccomp-filter".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    anyhow::ensure!(
        fd >= 0,
        "memfd_create for the seccomp filter: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fd is a descriptor this call just created and nothing else owns.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes)
        .context("writing the seccomp filter to its memfd")?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
    // SAFETY: fcntl with integer arguments on a descriptor this function owns.
    anyhow::ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } == 0,
        "sealing the seccomp filter's memfd: {}",
        std::io::Error::last_os_error()
    );
    Ok(file.into())
}

/// The threads of `pid` that do not run under a seccomp filter, by name: every entry of
/// `/proc/<pid>/task` whose status lacks `Seccomp: 2` (filter mode) or reports no installed
/// filter. A thread that exits while it is read is skipped. An error when the process has
/// no readable thread at all, so a process that is gone is never taken as filtered.
pub fn unfiltered_threads(pid: u32) -> anyhow::Result<Vec<String>> {
    use anyhow::Context as _;
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task"))
        .with_context(|| format!("listing the threads of process {pid}"))?;
    let mut seen = 0usize;
    let mut unfiltered = Vec::new();
    for task in tasks {
        let Ok(task) = task else { continue };
        let Ok(status) = std::fs::read_to_string(task.path().join("status")) else {
            continue;
        };
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                .map(str::trim)
        };
        seen += 1;
        let filtered = field("Seccomp") == Some("2")
            && field("Seccomp_filters")
                .and_then(|count| count.parse::<u32>().ok())
                .is_some_and(|count| count >= 1);
        if !filtered {
            unfiltered.push(format!(
                "{} (tid {}, Seccomp: {}, Seccomp_filters: {})",
                field("Name").unwrap_or("?"),
                task.file_name().to_string_lossy(),
                field("Seccomp").unwrap_or("absent"),
                field("Seccomp_filters").unwrap_or("absent"),
            ));
        }
    }
    anyhow::ensure!(seen > 0, "process {pid} has no readable thread");
    Ok(unfiltered)
}

/// How much of the end of a Firecracker log [`last_trapped_syscall`] reads. The SIGSYS
/// handler's line is among the last the process writes.
const TRAP_LOG_TAIL: u64 = 64 * 1024;

/// The syscall the confined policy stopped, from the end of the Firecracker log at `path`.
/// Reads at most [`TRAP_LOG_TAIL`] bytes, whatever the log's size (at Debug level it grows
/// by gigabytes an hour), and decodes them lossily, so a byte that is not UTF-8 costs
/// nothing.
pub fn last_trapped_syscall(path: &std::path::Path) -> Option<TrappedSyscall> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(std::io::SeekFrom::Start(len.saturating_sub(TRAP_LOG_TAIL)))
        .ok()?;
    let mut tail = Vec::new();
    file.take(TRAP_LOG_TAIL).read_to_end(&mut tail).ok()?;
    trapped_syscall(&String::from_utf8_lossy(&tail))
}

/// Whether a VMM is confined, as [`resolve`] decided it. It is the only way to get one: it
/// has no public constructor and no `Default`, and every VMM spawn takes one
/// ([`crate::utils::NamespaceParams::decision`]), so no VMM starts without the decision
/// the host's mode and the start's request make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision(Option<Confinement>);

impl Decision {
    /// How the VMM is confined, or `None` when [`resolve`] decided it runs unconfined.
    pub fn confinement(&self) -> Option<&Confinement> {
        self.0.as_ref()
    }

    /// For a confined decision: `firecracker` has the filter `fcvm setup` compiled for it,
    /// unchanged. Callers run it as soon as the binary is known and before anything is
    /// created, so a host that was upgraded and not set up again is refused with nothing
    /// started; the launch checks again and hands the VMM the bytes it checked.
    pub fn require_filter(&self, firecracker: &std::path::Path) -> anyhow::Result<()> {
        let Some(mode) = self.0.as_ref().and_then(|confinement| confinement.seccomp) else {
            return Ok(());
        };
        verified_filter_bytes(firecracker, mode)
            .map(|_| ())
            .map_err(|e| {
                anyhow::anyhow!(
                    "this start must be confined and cannot be; nothing was started:\n  \
                     {}: {e:#}",
                    Mechanism::Seccomp
                )
            })
    }

    /// A decision for tests that spawn a process through the VMM pre_exec directly.
    #[cfg(test)]
    pub(crate) fn for_test(confinement: Option<Confinement>) -> Self {
        Self(confinement)
    }
}

const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const TUNSETOWNER: libc::c_ulong = 0x4004_54cc;

/// A TAP name as `struct ifreq` takes it, built before the fork.
#[derive(Debug, Clone)]
pub struct TapName([u8; libc::IFNAMSIZ]);

impl TapName {
    pub fn new(name: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !name.is_empty() && name.len() < libc::IFNAMSIZ && !name.contains('\0'),
            "TAP name {name:?} does not fit an ifreq"
        );
        let mut bytes = [0u8; libc::IFNAMSIZ];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Ok(Self(bytes))
    }
}

/// Make the persistent TAP `name` owned by the calling process's uid: attach with
/// `TUNSETIFF`, which needs CAP_NET_ADMIN over the namespace, set `TUNSETOWNER`, detach.
/// On Linux 7.3 a process without CAP_NET_ADMIN gets EPERM attaching to an unowned TAP (7.0
/// lets it), so a confined VMM's TAP is owned before its capabilities are dropped. Only
/// confined starts do this, so the default path's namespace build is untouched.
///
/// Async-signal-safe: raw syscalls only, no allocation.
pub(crate) fn own_tap(name: &TapName) -> std::io::Result<()> {
    #[repr(C)]
    struct IfReq {
        name: [u8; libc::IFNAMSIZ],
        flags: libc::c_short,
        pad: [u8; 22],
    }
    let mut request = IfReq {
        name: name.0,
        flags: 0x0002 | 0x1000, // IFF_TAP | IFF_NO_PI
        pad: [0; 22],
    };
    // SAFETY: a NUL-terminated literal path.
    let fd = unsafe { libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `request` is a struct ifreq that outlives the call; TUNSETOWNER takes the uid
    // by value; `fd` is ours to close.
    let result = unsafe {
        if libc::ioctl(fd, TUNSETIFF as _, &mut request as *mut IfReq) != 0
            || libc::ioctl(fd, TUNSETOWNER as _, libc::geteuid() as libc::c_ulong) != 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    // SAFETY: `fd` is ours.
    unsafe { libc::close(fd) };
    result
}

/// Apply [`Layer::Process`] to the calling process. Runs in the VMM's pre_exec, after it
/// has entered its namespaces and before `PR_SET_PDEATHSIG`. It needs CAP_SETPCAP, which
/// the process holds after entering the holder's user namespace.
///
/// Async-signal-safe: raw syscalls only, no allocation. No step changes a uid, gid or
/// fsuid or raises a capability, so none clears the parent-death signal.
pub(crate) fn apply_process_layer(core_limit: libc::rlim_t) -> std::io::Result<()> {
    // securebits (linux/securebits.h): NOROOT, NO_SETUID_FIXUP and NO_CAP_AMBIENT_RAISE on,
    // KEEP_CAPS off, all four locked. With NOROOT the exec as namespace uid 0 grants no
    // capabilities.
    const SECBITS: libc::c_ulong = (1 << 0) // SECBIT_NOROOT
        | (1 << 1) // SECBIT_NOROOT_LOCKED
        | (1 << 2) // SECBIT_NO_SETUID_FIXUP
        | (1 << 3) // SECBIT_NO_SETUID_FIXUP_LOCKED
        | (1 << 5) // SECBIT_KEEP_CAPS_LOCKED
        | (1 << 6) // SECBIT_NO_CAP_AMBIENT_RAISE
        | (1 << 7); // SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let check = |rc: libc::c_int| {
        if rc != 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };

    // 1, not 0, when the inherited hard limit allows it ([`core_limit_for`]). In Linux
    // v7.3.3, fs/coredump.c: coredump_pipe() (line 976) aborts a pipe dump when the limit is
    // exactly 1 ("RLIMIT_CORE is set to 1, aborting core") and otherwise ignores the limit;
    // coredump_file() (line 886) writes nothing below binfmt->min_coredump, a page;
    // coredump_socket() (line 858) does not read the limit, so a socket core_pattern is
    // refused in the preflight.
    let no_core = libc::rlimit {
        rlim_cur: core_limit,
        rlim_max: core_limit,
    };
    // SAFETY: setrlimit reads the struct, which outlives the call.
    check(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) })?;
    // SAFETY: prctl with integer arguments only.
    check(unsafe { libc::prctl(libc::PR_SET_SECUREBITS, SECBITS) })?;
    // Capability numbers past the kernel's last one fail with EINVAL, which ends the loop.
    for cap in 0..64 as libc::c_ulong {
        // SAFETY: prctl with integer arguments only.
        if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) && cap > 0 {
                break;
            }
            return Err(error);
        }
    }
    // SAFETY: prctl with integer arguments only.
    check(unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    })?;
    let header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: capset reads a version 3 header and two data structs, both outlive the call.
    check(unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } as libc::c_int)?;
    // SAFETY: prctl with integer arguments only.
    check(unsafe {
        libc::prctl(
            libc::PR_SET_NO_NEW_PRIVS,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypervisor::Backend;
    use std::os::unix::ffi::OsStringExt;

    fn host(abi: Result<u32, i32>) -> Host {
        Host {
            mode: Mode::Off,
            landlock_abi: abi,
            strace_fc: false,
            no_seccomp: false,
            firecracker_args: None,
            core_pattern: Some("core".to_string()),
            core_hard_limit: u64::MAX,
            seccomp: SeccompMode::Enforce,
            firecracker_commit: None,
            allow_seccomp_log: true,
        }
    }

    fn request(flag: bool) -> Request<'static> {
        Request {
            flag,
            backend: Backend::Firecracker,
            network: Network::Rootless,
            firecracker_args: None,
            unsupported: None,
            kernel_profile: None,
        }
    }

    #[test]
    fn an_unconfined_start_on_an_off_host_is_left_alone_whatever_the_kernel() {
        assert_eq!(decide(&request(false), &host(Ok(1))).unwrap(), None);
        assert_eq!(
            decide(&request(false), &host(Err(libc::ENOSYS))).unwrap(),
            None
        );
    }

    #[test]
    fn a_confined_start_records_the_abi_and_its_layers() {
        assert_eq!(
            decide(&request(true), &host(Ok(11))).unwrap(),
            Some(Confinement {
                landlock_abi: 11,
                layers: vec![Layer::Process, Layer::Seccomp],
                core_limit: 1,
                seccomp: Some(SeccompMode::Enforce),
            })
        );
    }

    /// Each mode's policy differs only in the default action of every thread class, and
    /// each compiles to its own file name, so a policy change never reuses a stale filter.
    #[test]
    fn the_seccomp_policy_sets_the_default_action_per_mode_and_names_its_file() {
        let bin = std::path::Path::new("/assets/firecracker/firecracker-abc");
        for mode in SeccompMode::ALL {
            let policy: serde_json::Value = serde_json::from_str(&seccomp_policy(mode)).unwrap();
            for class in ["vmm", "api", "vcpu"] {
                assert_eq!(policy[class]["default_action"], mode.action(), "{class}");
                assert_eq!(policy[class]["filter_action"], "allow", "{class}");
            }
            let path = filter_path(bin, mode).to_string_lossy().into_owned();
            assert!(
                path.starts_with(&format!(
                    "/assets/firecracker/firecracker-abc.confined-{}-",
                    mode.name()
                )) && path.ends_with(".bpf"),
                "{path}"
            );
        }
        assert_ne!(
            filter_path(bin, SeccompMode::Enforce),
            filter_path(bin, SeccompMode::Log)
        );
        // A repin names a new file even when the fork's filters, and so the policy, are
        // unchanged: the filter beside a binary of the old commit is not found again.
        let policy = seccomp_policy(SeccompMode::Enforce);
        assert_ne!(
            filter_digest(CONFINED_FIRECRACKER_COMMIT, &policy),
            filter_digest("0000000000000000000000000000000000000000", &policy)
        );
        assert!(policy_is_for(CONFINED_FIRECRACKER_COMMIT));
        assert!(!policy_is_for("4fc14097b96701d5c382a24a7b8bbaec025445e4"));
    }

    #[test]
    fn the_trapped_syscall_is_read_from_the_firecracker_log() {
        let log = "2026-11-23 [vmm] ok\n\
                   2026-11-23T17:00:00 [anonymous-instance:main] Shutting down VM after \
                   intercepting a bad syscall (220).\n";
        assert_eq!(
            trapped_syscall(log),
            Some(TrappedSyscall {
                thread: "main".into(),
                number: 220
            })
        );
        assert_eq!(trapped_syscall("nothing here\n"), None);
        // As Firecracker wrote it on an x64 runner, where the API thread was stopped.
        let log = "2026-11-25T01:20:07.607904286 [anonymous-instance:fc_api:ERROR:src/vmm/src/\
                   signal_handler.rs:110] Shutting down VM after intercepting signal 31, code 1.\n\
                   2026-11-25T01:20:07.607912353 [anonymous-instance:fc_api:ERROR:src/vmm/src/\
                   signal_handler.rs:70] Shutting down VM after intercepting a bad syscall (232).\n";
        let trapped = trapped_syscall(log).unwrap();
        assert_eq!(
            trapped,
            TrappedSyscall {
                thread: "fc_api".into(),
                number: 232
            }
        );
        let report = trapped_syscall_report(&trapped);
        assert!(
            report.contains("its fc_api thread made syscall 232"),
            "{report}"
        );
        assert!(report.contains("FCVM_CONFINE_SECCOMP=log"), "{report}");
    }

    /// io_uring operations are not seen by seccomp, so the policy must not let the VMM
    /// make a ring; fcvm's drives use the Sync engine. No thread may create a process or
    /// exec either.
    #[test]
    fn the_seccomp_policy_denies_io_uring_process_creation_and_exec() {
        for arch in SECCOMP_ARCHES {
            let policy = read_json(&seccomp_file(&format!("firecracker-confined-{arch}.json")));
            for class in ["vmm", "api", "vcpu"] {
                for rule in policy[class]["filter"].as_array().unwrap() {
                    let syscall = rule["syscall"].as_str().unwrap();
                    assert!(
                        !syscall.starts_with("io_uring")
                            && !["fork", "vfork", "clone", "clone3", "execve", "execveat"]
                                .contains(&syscall)
                            && !["ptrace", "process_vm_readv", "process_vm_writev"]
                                .contains(&syscall)
                            && !["pidfd_open", "pidfd_getfd", "kill", "unshare", "setns"]
                                .contains(&syscall)
                            && !["mount", "bpf", "perf_event_open", "keyctl"].contains(&syscall),
                        "{arch} {class} allows {syscall}"
                    );
                }
            }
        }
    }

    /// Whether `removal` (an entry of the changes' `remove`) takes `rule` out: every rule of
    /// its syscall, or, when it names an `arg`, only the rules that compare that argument
    /// with that value.
    fn removes(removal: &serde_json::Value, rule: &serde_json::Value) -> bool {
        if removal["syscall"] != rule["syscall"] {
            return false;
        }
        let Some(arg) = removal.get("arg") else {
            return true;
        };
        rule["args"].as_array().is_some_and(|args| {
            args.iter()
                .any(|a| a["index"] == arg["index"] && a["val"] == arg["val"])
        })
    }

    /// The confined policy of `arch`: the fork's shipped filter for that architecture, less
    /// every removed rule ([`removes`]), plus the declared additions for that architecture,
    /// each class's in declaration order after the fork's.
    fn derived_policy(
        fork: &serde_json::Value,
        changes: &serde_json::Value,
        arch: &str,
    ) -> serde_json::Value {
        let removed = changes["remove"].as_array().unwrap();
        let mut policy = fork.clone();
        for (class, filter) in policy.as_object_mut().unwrap().iter_mut() {
            let rules = filter["filter"].as_array_mut().unwrap();
            rules.retain(|rule| !removed.iter().any(|removal| removes(removal, rule)));
            for add in changes["add"].as_array().unwrap() {
                let listed = |key: &str, value: &str| {
                    add[key].as_array().unwrap().iter().any(|v| v == value)
                };
                if listed("arches", arch) && listed("classes", class) {
                    rules.push(add["rule"].clone());
                }
            }
        }
        policy
    }

    fn seccomp_file(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources/seccomp")
            .join(name)
    }

    fn read_json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    const SECCOMP_ARCHES: [&str; 2] = ["x86_64", "aarch64"];

    /// Each confined policy is the fork's shipped filter for that architecture plus the
    /// changes in confined-changes.json, so a change to the fork's filter or to the declared
    /// changes cannot leave a policy behind. `make confine-seccomp-policy` writes the files
    /// (FCVM_WRITE_SECCOMP_POLICY=1); this test then reads them back.
    #[test]
    fn the_confined_policies_are_the_fork_filters_plus_the_declared_changes() {
        let changes = read_json(&seccomp_file("confined-changes.json"));
        for arch in SECCOMP_ARCHES {
            let fork = read_json(&seccomp_file(&format!(
                "fork-{arch}-unknown-linux-musl.json"
            )));
            // Every removal and addition still means something against this fork filter.
            for removal in changes["remove"].as_array().unwrap() {
                let syscall = removal["syscall"].as_str().unwrap();
                assert!(
                    fork.as_object()
                        .unwrap()
                        .values()
                        .flat_map(|class| class["filter"].as_array().unwrap())
                        .any(|rule| removes(removal, rule)),
                    "{arch}: {removal} removes nothing from the fork filter"
                );
                assert!(
                    !removal["why"].as_str().unwrap_or("").is_empty(),
                    "{syscall}: no reason"
                );
            }
            for add in changes["add"].as_array().unwrap() {
                let rule = &add["rule"];
                assert!(
                    !rule["comment"].as_str().unwrap_or("").is_empty(),
                    "an added rule has no comment: {rule}"
                );
                for class in add["classes"].as_array().unwrap() {
                    let class = class.as_str().unwrap();
                    assert!(fork.get(class).is_some(), "{class} is not a thread class");
                    if add["arches"].as_array().unwrap().iter().any(|a| a == arch) {
                        assert!(
                            !fork[class]["filter"].as_array().unwrap().contains(rule),
                            "{arch} {class}: the fork filter already has {rule}"
                        );
                    }
                }
            }
            let derived = derived_policy(&fork, &changes, arch);
            let path = seccomp_file(&format!("firecracker-confined-{arch}.json"));
            let mut out = Vec::new();
            let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
            let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
            serde::Serialize::serialize(&derived, &mut ser).unwrap();
            out.push(b'\n');
            if std::env::var_os("FCVM_WRITE_SECCOMP_POLICY").is_some() {
                std::fs::write(&path, &out).unwrap();
            }
            // Byte for byte, so the file is what the generator writes and writing it again
            // changes nothing.
            assert!(
                std::fs::read(&path).unwrap() == out,
                "{} is not the fork filter plus confined-changes.json as \
                 `make confine-seccomp-policy` writes it: run that",
                path.display()
            );
        }
    }

    /// The fork filters the policies are derived from are the ones of the Firecracker the
    /// default profile builds: a repin of `[firecracker] commit` fails here until the
    /// filters are refreshed (`make confine-seccomp-refresh`) and the policies regenerated.
    #[test]
    fn the_confined_policies_are_derived_from_the_pinned_firecracker() {
        let changes = read_json(&seccomp_file("confined-changes.json"));
        let config: toml::Table = toml::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("rootfs-config.toml"),
            )
            .unwrap(),
        )
        .unwrap();
        let firecracker = &config["firecracker"];
        assert_eq!(
            changes["fork"]["repo"].as_str(),
            firecracker["repo"].as_str()
        );
        assert_eq!(
            changes["fork"]["commit"].as_str(),
            firecracker["commit"].as_str(),
            "the confined seccomp policies were derived from another Firecracker commit"
        );
        assert_eq!(
            changes["fork"]["commit"].as_str(),
            Some(CONFINED_FIRECRACKER_COMMIT)
        );
    }

    /// Where glibc and musl make different syscalls for one libc function, measured with
    /// scripts/confine-libc-probe on Ubuntu 24.04 (glibc 2.39, musl 1.2.4) on each
    /// architecture: (architecture, musl's syscall, glibc's). The fork's filter is written
    /// for a musl Firecracker and fcvm runs a glibc one, so wherever a class of the fork's
    /// filter lists musl's call, the confined policy's class must list glibc's. The forms
    /// that differ only for calls Firecracker never makes (creat, alarm, sysconf) are left
    /// out, and so is thread creation, which happens before any filter is installed.
    const GLIBC_FOR_MUSL: &[(&str, &str, &str)] = &[
        ("x86_64", "open", "openat"),
        ("x86_64", "stat", "newfstatat"),
        ("x86_64", "lstat", "newfstatat"),
        ("x86_64", "epoll_pwait", "epoll_wait"),
        ("x86_64", "select", "pselect6"),
        ("x86_64", "pipe", "pipe2"),
        ("x86_64", "tkill", "tgkill"),
        ("x86_64", "tkill", "getpid"),
        ("x86_64", "nanosleep", "clock_nanosleep"),
        ("x86_64", "faccessat", "faccessat2"),
        ("aarch64", "tkill", "tgkill"),
        ("aarch64", "tkill", "getpid"),
        ("aarch64", "nanosleep", "clock_nanosleep"),
        ("aarch64", "faccessat", "faccessat2"),
    ];

    #[test]
    fn glibcs_call_is_listed_wherever_the_fork_filter_lists_musls() {
        for arch in SECCOMP_ARCHES {
            let fork = read_json(&seccomp_file(&format!(
                "fork-{arch}-unknown-linux-musl.json"
            )));
            let policy = read_json(&seccomp_file(&format!("firecracker-confined-{arch}.json")));
            let lists = |filter: &serde_json::Value, class: &str, syscall: &str| {
                filter[class]["filter"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|rule| rule["syscall"] == syscall)
            };
            for class in ["vmm", "api", "vcpu"] {
                for (_, musl, glibc) in GLIBC_FOR_MUSL.iter().filter(|(a, ..)| *a == arch) {
                    if lists(&fork, class, musl) {
                        assert!(
                            lists(&policy, class, glibc),
                            "{arch} {class}: the fork's filter lists {musl}, where glibc calls \
                             {glibc}, and the confined policy does not list {glibc}"
                        );
                    }
                }
            }
        }
    }

    /// The two architectures' policies list the same syscalls with the same arguments in
    /// every thread class, apart from the lists below: the names that exist on one
    /// architecture only or are the call one libc makes there, and the KVM ioctl requests
    /// one architecture has and the other does not. A syscall, an argument rule or an ioctl
    /// request added to one policy and not the other fails here, and so does a list entry
    /// the policies no longer need.
    #[test]
    fn the_two_architectures_policies_differ_only_in_listed_names() {
        // On x86_64 only: the musl forms the fork's x86_64 filter lists (open, stat; aarch64
        // has neither syscall) and glibc's epoll_wait (aarch64 has only epoll_pwait).
        const X86_64_ONLY: &[(&str, &str)] = &[
            ("vmm", "open"),
            ("api", "open"),
            ("vcpu", "open"),
            ("vmm", "stat"),
            ("vmm", "epoll_wait"),
            ("api", "epoll_wait"),
        ];
        // On aarch64 only: none. Its openat and newfstatat are in the x86_64 policy too.
        const AARCH64_ONLY: &[(&str, &str)] = &[];
        // ioctl requests one architecture's KVM has and the other's does not: (class, the
        // request's name in the fork's filter).
        const X86_64_ONLY_IOCTLS: &[(&str, &str)] = &[
            ("vmm", "KVM_GET_CLOCK"),
            ("vmm", "KVM_GET_PIT2"),
            ("vmm", "KVM_GET_IRQCHIP"),
            ("vcpu", "KVM_GET_TSC_KHZ"),
            ("vcpu", "KVM_KVMCLOCK_CTRL"),
            ("vcpu", "KVM_GET_VCPU_EVENTS"),
            ("vcpu", "KVM_GET_DEBUGREGS"),
            ("vcpu", "KVM_GET_REGS"),
            ("vcpu", "KVM_GET_SREGS"),
            ("vcpu", "KVM_GET_XCRS"),
            ("vcpu", "KVM_GET_LAPIC"),
            ("vcpu", "KVM_GET_XSAVE"),
            ("vcpu", "KVM_GET_XSAVE2"),
            ("vcpu", "KVM_GET_MSRS"),
            ("vcpu", "KVM_GET_CPUID2"),
        ];
        const AARCH64_ONLY_IOCTLS: &[(&str, &str)] = &[
            ("vmm", "KVM_SET_DEVICE_ATTR"),
            ("vmm", "KVM_GET_DEVICE_ATTR"),
            ("vcpu", "KVM_GET_ONE_REG"),
            ("vcpu", "KVM_GET_REG_LIST"),
        ];
        type Rules = std::collections::BTreeMap<(String, String), Vec<String>>;
        // Per (class, syscall): each rule's conditions, and for ioctl each request's name
        // (the first word of its comment) keyed by its conditions.
        let rules = |arch: &str| {
            let policy = read_json(&seccomp_file(&format!("firecracker-confined-{arch}.json")));
            let mut out = Rules::new();
            let mut ioctls = std::collections::BTreeMap::<(String, String), String>::new();
            for (class, filter) in policy.as_object().unwrap() {
                for rule in filter["filter"].as_array().unwrap() {
                    let name = rule["syscall"].as_str().unwrap().to_string();
                    let mut args = rule["args"].clone();
                    let mut request = String::new();
                    // Comments are prose; only the conditions are compared.
                    if let Some(list) = args.as_array_mut() {
                        for arg in list {
                            if let Some(comment) = arg.as_object_mut().unwrap().remove("comment") {
                                if request.is_empty() {
                                    request = comment
                                        .as_str()
                                        .unwrap_or("")
                                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                                        .next()
                                        .unwrap_or("")
                                        .to_string();
                                }
                            }
                        }
                    }
                    if name == "ioctl" {
                        ioctls.insert((class.clone(), args.to_string()), request);
                    }
                    out.entry((class.clone(), name))
                        .or_default()
                        .push(args.to_string());
                }
            }
            for list in out.values_mut() {
                list.sort();
            }
            (out, ioctls)
        };
        let ((x86, x86_ioctls), (arm, arm_ioctls)) = (rules("x86_64"), rules("aarch64"));
        // What `ours` has and `theirs` does not, as (class, name), sorted.
        let only =
            |ours: &Rules,
             theirs: &Rules,
             names: &std::collections::BTreeMap<(String, String), String>| {
                let mut syscalls = Vec::new();
                let mut requests = Vec::new();
                for ((class, name), args) in ours {
                    match theirs.get(&(class.clone(), name.clone())) {
                        None => syscalls.push((class.clone(), name.clone())),
                        Some(their_args) if name == "ioctl" => {
                            for arg in args.iter().filter(|arg| !their_args.contains(arg)) {
                                requests.push((
                                    class.clone(),
                                    names[&(class.clone(), arg.clone())].clone(),
                                ));
                            }
                        }
                        Some(their_args) => assert!(
                            args == their_args,
                            "{class}: {name} has other arguments on one architecture ({args:?}) \
                         than on the other ({their_args:?})"
                        ),
                    }
                }
                syscalls.sort();
                requests.sort();
                (syscalls, requests)
            };
        let listed = |list: &[(&str, &str)]| {
            let mut list: Vec<(String, String)> = list
                .iter()
                .map(|(class, name)| (class.to_string(), name.to_string()))
                .collect();
            list.sort();
            list
        };
        let (x86_syscalls, x86_requests) = only(&x86, &arm, &x86_ioctls);
        let (arm_syscalls, arm_requests) = only(&arm, &x86, &arm_ioctls);
        assert_eq!(x86_syscalls, listed(X86_64_ONLY), "syscalls on x86_64 only");
        assert_eq!(
            arm_syscalls,
            listed(AARCH64_ONLY),
            "syscalls on aarch64 only"
        );
        assert_eq!(
            x86_requests,
            listed(X86_64_ONLY_IOCTLS),
            "ioctl requests on x86_64 only"
        );
        assert_eq!(
            arm_requests,
            listed(AARCH64_ONLY_IOCTLS),
            "ioctl requests on aarch64 only"
        );
    }

    /// The x64 CI runners' 6.14 kernel has ABI 6 and stock 7.0 has 8: both are refused,
    /// naming the ABI found.
    #[test]
    fn below_the_landlock_floor_the_start_is_refused_naming_the_abi() {
        for abi in [4, 6, 8] {
            let error = decide(&request(true), &host(Ok(abi)))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("landlock: the kernel's Landlock ABI is {abi};")),
                "{error}"
            );
            assert!(error.contains("nothing was started"), "{error}");
        }
        let error = decide(&request(true), &host(Err(libc::EOPNOTSUPP)))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("landlock: Landlock is unavailable"),
            "{error}"
        );
    }

    #[test]
    fn every_missing_piece_is_listed_by_mechanism() {
        let request = Request {
            flag: true,
            backend: Backend::CloudHypervisor,
            network: Network::Bridged,
            firecracker_args: Some("--enable-nv2 --no-seccomp"),
            unsupported: None,
            kernel_profile: None,
        };
        let host = Host {
            mode: Mode::Off,
            landlock_abi: Ok(6),
            strace_fc: true,
            no_seccomp: true,
            firecracker_args: Some("--seccomp-filter /x.bpf".to_string()),
            core_pattern: Some("|/usr/lib/systemd/systemd-coredump %P".to_string()),
            core_hard_limit: u64::MAX,
            seccomp: SeccompMode::Enforce,
            firecracker_commit: None,
            allow_seccomp_log: true,
        };
        let mechanisms: Vec<Mechanism> = missing(&request, &host)
            .iter()
            .map(|m| m.mechanism)
            .collect();
        assert_eq!(
            mechanisms,
            vec![
                Mechanism::Landlock,
                Mechanism::Backend,
                Mechanism::Network,
                Mechanism::Debugging,
                Mechanism::Debugging,
                Mechanism::Debugging,
                Mechanism::Debugging,
            ]
        );
        let error = decide(&request, &host).unwrap_err().to_string();
        for needle in [
            "backend: confinement supports the Firecracker backend only",
            "network: --network bridged runs the VMM as root",
            "debugging: FCVM_STRACE_FC",
            "debugging: FCVM_NO_SECCOMP",
            "debugging: the extra Firecracker arguments set --no-seccomp",
            "debugging: the extra Firecracker arguments set --seccomp-filter",
        ] {
            assert!(error.contains(needle), "missing {needle:?} in {error}");
        }
    }

    /// A kernel profile that runs another Firecracker build (the GPU profiles pin their own
    /// fork commits, with VFIO and PCI) is refused: the policy was derived and audited for
    /// the default build only, and its VFIO ioctls would stop the VMM.
    #[test]
    fn a_profile_running_another_firecracker_build_is_refused() {
        let gpu = Host {
            firecracker_commit: Some("4fc14097b96701d5c382a24a7b8bbaec025445e4".to_string()),
            ..host(Ok(11))
        };
        let gpu_request = Request {
            kernel_profile: Some("gpu"),
            ..request(true)
        };
        let error = decide(&gpu_request, &gpu).unwrap_err().to_string();
        assert!(
            error.contains(
                "kernel profile gpu runs Firecracker 4fc14097b96701d5c382a24a7b8bbaec025445e4"
            ),
            "{error}"
        );
        let default = Host {
            firecracker_commit: Some(CONFINED_FIRECRACKER_COMMIT.to_string()),
            ..host(Ok(11))
        };
        assert!(decide(&request(true), &default).unwrap().is_some());
    }

    #[test]
    fn routed_and_an_unsupported_command_are_refused() {
        let request = Request {
            network: Network::Routed,
            unsupported: Some("raw VM snapshots"),
            ..request(true)
        };
        let error = decide(&request, &host(Ok(11))).unwrap_err().to_string();
        assert!(error.contains("command: raw VM snapshots"), "{error}");
        assert!(error.contains("network: --network routed"), "{error}");
    }

    #[test]
    fn a_required_host_confines_a_start_without_the_flag_or_refuses_it() {
        let required = Host {
            mode: Mode::Required,
            ..host(Ok(11))
        };
        assert!(decide(&request(false), &required).unwrap().is_some());
        let required_old = Host {
            mode: Mode::Required,
            ..host(Ok(6))
        };
        let error = decide(&request(false), &required_old)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("the host requires confinement"),
            "{error}"
        );
    }

    #[test]
    fn the_environment_can_tighten_the_config_but_not_loosen_it() {
        assert_eq!(host_mode(Mode::Off, None).unwrap(), Mode::Off);
        assert_eq!(
            host_mode(Mode::Off, Some("required")).unwrap(),
            Mode::Required
        );
        assert_eq!(
            host_mode(Mode::Required, Some("required")).unwrap(),
            Mode::Required
        );
        let error = host_mode(Mode::Required, Some("off"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot loosen"), "{error}");
        assert!(host_mode(Mode::Off, Some("on")).is_err());
    }

    #[test]
    fn a_non_utf8_mode_variable_is_an_error_not_unset() {
        assert_eq!(mode_env(Err(std::env::VarError::NotPresent)).unwrap(), None);
        assert_eq!(
            mode_env(Ok("required".to_string())).unwrap(),
            Some("required".to_string())
        );
        let bad = std::ffi::OsString::from_vec(vec![0xff, b'x']);
        let error = mode_env(Err(std::env::VarError::NotUnicode(bad)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("FCVM_CONFINE"), "{error}");
    }

    /// An unconfined start on an off host reads no more of the host: no Landlock syscall.
    #[test]
    fn an_unconfined_start_on_an_off_host_makes_no_landlock_probe() {
        assert!(!needs_host_read(false, Mode::Off));
        assert!(needs_host_read(true, Mode::Off));
        assert!(needs_host_read(false, Mode::Required));
    }

    /// RLIMIT_CORE 1 stops file and pipe dumps; a socket core_pattern ignores it, and an
    /// unreadable one cannot be checked, so both refuse confinement.
    #[test]
    fn a_core_pattern_that_ignores_the_core_limit_refuses_confinement() {
        for pattern in [Some("@/run/systemd/coredump"), None] {
            let host = Host {
                core_pattern: pattern.map(str::to_string),
                ..host(Ok(11))
            };
            let error = decide(&request(true), &host).unwrap_err().to_string();
            assert!(
                error.contains("process: the host's core_pattern"),
                "{error}"
            );
        }
        for pattern in ["core", "|/usr/lib/systemd/systemd-coredump %P"] {
            let host = Host {
                core_pattern: Some(pattern.to_string()),
                ..host(Ok(11))
            };
            assert!(
                decide(&request(true), &host).unwrap().is_some(),
                "{pattern}"
            );
        }
    }

    /// An inherited hard core limit of 0 cannot become 1: a pipe core_pattern then refuses
    /// confinement up front, and a file one keeps the 0, which stops a file dump.
    #[test]
    fn an_inherited_zero_core_limit_is_kept_for_files_and_refused_for_pipes() {
        let pipe = Host {
            core_pattern: Some("|/usr/lib/systemd/systemd-coredump %P".to_string()),
            core_hard_limit: 0,
            ..host(Ok(11))
        };
        let error = decide(&request(true), &pipe).unwrap_err().to_string();
        assert!(error.contains("hard core limit is 0"), "{error}");
        let file = Host {
            core_hard_limit: 0,
            ..host(Ok(11))
        };
        assert!(decide(&request(true), &file).unwrap().is_some());
        assert_eq!(core_limit_for(0), 0);
        assert_eq!(core_limit_for(1), 1);
        assert_eq!(core_limit_for(u64::MAX), 1);
    }

    /// The spawn installs the core limit the preflight validated and fails if it cannot. If
    /// fcvm's hard limit drops to 0 after the preflight (another process's prlimit), reading
    /// it again would hand the VMM 0, which a pipe core_pattern ignores.
    #[tokio::test(flavor = "current_thread")]
    async fn a_core_limit_that_can_no_longer_be_installed_fails_the_spawn() {
        let (mut holder, holder_pid) = crate::commands::common::spawn_namespace_holder()
            .await
            .expect("spawning the namespace holder");
        // Decided while the hard limit allowed 1, as under a pipe core_pattern.
        let decision = Decision::for_test(Some(Confinement {
            landlock_abi: LANDLOCK_ABI_FLOOR,
            layers: vec![Layer::Process],
            core_limit: 1,
            seccomp: None,
        }));
        // Then this process's hard limit drops to 0, which it cannot raise again. (Each
        // test runs in a process of its own.)
        let zero = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: setrlimit reads the struct, which outlives the call.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) }, 0);
        let mut cmd = tokio::process::Command::new("true");
        crate::utils::install_namespace_pre_exec(
            &mut cmd,
            &crate::utils::NamespaceParams {
                user_namespace_path: Some(format!("/proc/{holder_pid}/ns/user").into()),
                net_namespace_path: Some(format!("/proc/{holder_pid}/ns/net").into()),
                ..crate::utils::NamespaceParams::new("core-limit-dropped", decision)
            },
        )
        .unwrap();
        let spawned = cmd.spawn();
        let _ = holder.start_kill();
        let _ = holder.wait().await;
        crate::companion::forget(holder_pid);
        crate::network::netns_agent::forget(holder_pid);
        let error = spawned
            .map(|_| ())
            .expect_err("the VMM started with a core limit below the one the preflight checked");
        assert_eq!(error.raw_os_error(), Some(libc::EPERM), "{error}");
    }

    /// The TUNSETIFF calls pass a mutable ifreq: the kernel writes it back.
    #[test]
    fn tunsetiff_gets_a_mutable_ifreq() {
        let code = non_test(include_str!("confine.rs"));
        let tests = &include_str!("confine.rs")[code.len()..];
        // own_tap in the code, attach_tap in the probe.
        assert_eq!(
            code.matches("TUNSETIFF as _, &mut request as *mut IfReq")
                .count(),
            1
        );
        assert_eq!(
            tests
                .matches(concat!("as _, &mut request as *mut ", "IfReq) }"))
                .count(),
            1
        );
        assert!(!code.contains(concat!("TUNSETIFF as _, &", "request)")));
    }

    /// Each restore path and the setup VM resolve before their first host side effect.
    #[test]
    fn confinement_is_resolved_before_any_host_side_effect() {
        let snapshot = include_str!("commands/snapshot.rs");
        let body = |source: &'static str, name: &str| -> &'static str {
            let start = source.find(name).unwrap();
            let rest = &source[start..];
            &rest[..rest.find("\n}\n").unwrap()]
        };
        let inner = body(snapshot, "async fn cmd_snapshot_run_inner(");
        let resolve = inner.find("crate::confine::resolve(").unwrap();
        for effect in [
            "reclaim_abandoned()",
            "Owner::begin(",
            "EarlyPrefill::start(",
        ] {
            assert!(
                resolve < inner.find(effect).unwrap(),
                "restore: {effect} first"
            );
        }
        let raw = body(snapshot, "async fn cmd_snapshot_run_raw(");
        let resolve = raw.find("crate::confine::resolve(").unwrap();
        for effect in ["reclaim_abandoned()", "Owner::begin("] {
            assert!(
                resolve < raw.find(effect).unwrap(),
                "raw restore: {effect} first"
            );
        }
        let setup = body(
            include_str!("setup/rootfs.rs"),
            "async fn boot_vm_for_setup(",
        );
        let resolve = setup.find("crate::confine::resolve(").unwrap();
        for effect in ["remove_dir_all(", "create_dir_all(", "File::create("] {
            assert!(
                resolve < setup.find(effect).unwrap(),
                "setup VM: {effect} first"
            );
        }
    }

    /// The last pre_exec step arms the parent-death signal and then checks that fcvm is
    /// still the parent: one that exited before the signal was armed sends none.
    #[test]
    fn the_vmm_exits_if_its_parent_died_before_the_death_signal_was_armed() {
        // Within install_namespace_pre_exec: other helpers in utils.rs have checks of their own.
        let source = include_str!("utils.rs");
        let start = source.find("pub fn install_namespace_pre_exec(").unwrap();
        let utils = &source[start..start + source[start..].find("\n}\n").unwrap()];
        let arm = utils
            .find("libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL)")
            .unwrap();
        let check = utils[arm..].find("libc::getppid() != parent").unwrap();
        assert!(utils[arm + check..].contains("libc::_exit(1)"));
    }

    /// A mode nothing recorded is read from the config, never taken as off: a library
    /// embedder or a command that set its paths another way still honours `required`.
    #[test]
    fn an_unrecorded_mode_is_loaded_from_the_config_not_taken_as_off() {
        assert_eq!(
            config_mode(None, || Ok(Mode::Required)).unwrap(),
            Mode::Required
        );
        assert_eq!(
            config_mode(Some(Mode::Off), || panic!(
                "a recorded mode is not reloaded"
            ))
            .unwrap(),
            Mode::Off
        );
        let error = config_mode(None, || anyhow::bail!("no config file"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("[confine] mode"), "{error}");
    }

    /// Every source file under `src/`, as (path, text).
    fn sources() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut dirs = vec![std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ))];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let rel = path
                        .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .unwrap()
                        .display()
                        .to_string();
                    out.push((rel, text));
                }
            }
        }
        out
    }

    /// The code of `text` before its `#[cfg(test)]` module, if it has one.
    fn non_test(text: &str) -> &str {
        text.find("\n#[cfg(test)]\nmod tests")
            .map_or(text, |at| &text[..at])
    }

    /// Every place outside tests that spawns a process through the VMM pre_exec. Each one
    /// passes a [`Decision`], which only [`resolve`] makes; a new site must be added here
    /// after it is checked to resolve its VM's confinement first.
    #[test]
    fn every_vmm_spawn_site_is_known() {
        let mut sites: Vec<(String, usize)> = sources()
            .into_iter()
            .map(|(path, text)| {
                let calls = non_test(&text)
                    .matches("install_namespace_pre_exec(")
                    .count()
                    - usize::from(path == "src/utils.rs");
                (path, calls)
            })
            .filter(|(_, calls)| *calls > 0)
            .collect();
        sites.sort();
        assert_eq!(
            sites,
            vec![
                ("src/commands/vm.rs".to_string(), 1),
                ("src/firecracker/vm.rs".to_string(), 1),
                ("src/hypervisor/cloud_hypervisor/mod.rs".to_string(), 1),
                ("src/setup/rootfs.rs".to_string(), 1),
            ],
            "a VMM spawn site was added or removed: check that it resolves its decision"
        );
    }

    /// The functions that build a `Command` from a Firecracker or Cloud Hypervisor binary
    /// path, and whether each also calls `install_namespace_pre_exec` (which takes the
    /// [`Decision`]). A VMM spawn that skips the helper starts with no decision at all, so
    /// every such function must be known and must call it.
    ///
    /// What it finds: `Command::new(<arg>)` outside tests where `<arg>` names a VMM binary
    /// path (`firecracker_bin`, `fc_bin`, `ch_bin`, `cloud_hypervisor`, a spec's `binary`).
    /// What it cannot see: a VMM binary whose path reaches `Command::new` under another name
    /// (a string built elsewhere, a variable called something else), or a VMM passed as an
    /// argument to another program (the nsenter and strace wrappers in
    /// `VmManager::start`, which that function's own helper call covers).
    fn vmm_command_sites() -> Vec<(String, String, bool)> {
        let command = regex::Regex::new(r"Command::new\(([^)]*)\)").unwrap();
        let vmm = regex::Regex::new(r"(?i)firecracker|fc_bin|ch_bin|cloud_hypervisor|\bbinary\b")
            .unwrap();
        let function =
            regex::Regex::new(r"\n\s*(?:pub(?:\([a-z]+\))? )?(?:async )?fn (\w+)").unwrap();
        let mut sites = Vec::new();
        for (path, text) in sources() {
            let code = non_test(&text);
            let starts: Vec<(usize, String)> = function
                .captures_iter(code)
                .map(|c| (c.get(0).unwrap().start(), c[1].to_string()))
                .collect();
            for call in command.captures_iter(code) {
                if !vmm.is_match(&call[1]) {
                    continue;
                }
                let at = call.get(0).unwrap().start();
                let Some(index) = starts.iter().rposition(|(start, _)| *start < at) else {
                    continue;
                };
                let end = starts
                    .get(index + 1)
                    .map_or(code.len(), |(start, _)| *start);
                let body = &code[starts[index].0..end];
                sites.push((
                    path.clone(),
                    starts[index].1.clone(),
                    body.contains("install_namespace_pre_exec("),
                ));
            }
        }
        sites.sort();
        sites.dedup();
        sites
    }

    #[test]
    fn every_vmm_binary_is_spawned_through_the_helper() {
        assert_eq!(
            vmm_command_sites(),
            vec![
                (
                    "src/firecracker/vm.rs".to_string(),
                    "start".to_string(),
                    true
                ),
                (
                    "src/hypervisor/cloud_hypervisor/mod.rs".to_string(),
                    "spawn".to_string(),
                    true
                ),
                (
                    "src/setup/rootfs.rs".to_string(),
                    "boot_vm_for_setup".to_string(),
                    true
                ),
            ],
            "a function builds a Command from a VMM binary: it must be listed here and spawn \
             through install_namespace_pre_exec with a resolved decision"
        );
    }

    /// Only this module makes a [`Decision`]: no code elsewhere builds one, so the only
    /// way to one is [`resolve`].
    #[test]
    fn a_decision_is_made_only_here() {
        for (path, text) in sources() {
            if path == "src/confine.rs" {
                continue;
            }
            for pattern in ["Decision(", "Decision {"] {
                assert!(
                    !text.contains(pattern),
                    "{path} builds a confine::Decision ({pattern})"
                );
            }
        }
        let here = include_str!("confine.rs");
        let tests_at = here.find("\n#[cfg(test)]\nmod tests").unwrap();
        assert_eq!(
            here[..tests_at].matches("Decision(").count(),
            3,
            "the type and resolve's two returns"
        );
    }

    /// fcvm setup's VM is refused under a host's `required` mode, and it resolves before it
    /// spawns. (Before this, it spawned with no decision at all.)
    #[test]
    fn the_setup_vm_resolves_before_it_spawns_and_is_refused_under_required() {
        let source = include_str!("setup/rootfs.rs");
        let resolve = source
            .find("crate::confine::resolve(&layer2_setup_confinement_request())")
            .expect("the setup VM resolves its confinement");
        let spawn = source.find("install_namespace_pre_exec(").unwrap();
        assert!(resolve < spawn, "the setup VM resolves before it spawns");
        let required = Host {
            mode: Mode::Required,
            ..host(Ok(11))
        };
        let error = decide(
            &crate::setup::rootfs::layer2_setup_request_for_test(),
            &required,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("command: fcvm setup's Layer 2 VM runs unconfined"),
            "{error}"
        );
    }

    /// A disk-only restore boots through `podman run` with arguments it builds from the
    /// snapshot's metadata, which has no --confine: it must carry the request's.
    #[test]
    fn a_disk_only_restore_carries_the_confine_request() {
        let source = include_str!("commands/snapshot.rs");
        let start = source.find("async fn cmd_snapshot_run_disk_only(").unwrap();
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(body.contains("confine: args.confine,"), "{body}");
    }

    #[test]
    fn the_config_key_parses_and_defaults_to_off() {
        #[derive(Deserialize)]
        struct Doc {
            #[serde(default)]
            confine: ConfineConfig,
        }
        let empty: Doc = toml::from_str("").unwrap();
        assert_eq!(empty.confine.mode, Mode::Off);
        let required: Doc = toml::from_str("[confine]\nmode = \"required\"\n").unwrap();
        assert_eq!(required.confine.mode, Mode::Required);
    }

    /// What the probe child prints about itself.
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct ProbeReport {
        cap_eff: String,
        cap_prm: String,
        cap_bnd: String,
        cap_amb: String,
        no_new_privs: String,
        /// errno of writing a network sysctl of its own namespace back unchanged, 0 if it
        /// succeeded: the write needs CAP_NET_ADMIN over the namespace. Refused, it is EACCES
        /// for an unprivileged invoker and EPERM when fcvm runs as root.
        net_sysctl_errno: i32,
        /// errno of `unshare(CLONE_NEWNS)`, 0 if it succeeded: needs CAP_SYS_ADMIN.
        unshare_errno: i32,
        core_limit: u64,
        /// errno of attaching to the namespace's persistent TAP as the VMM does
        /// (`TUNSETIFF` by name), 0 if it succeeded.
        tap_attach_errno: i32,
    }

    const PROBE_ENV: &str = "FCVM_CONFINE_PROBE_CHILD";
    /// Set for the probe that first makes the TAP, as the namespace's builder does.
    const PROBE_MAKE_TAP_ENV: &str = "FCVM_CONFINE_PROBE_MAKE_TAP";
    const PROBE_TAP: &str = "tap-probe";

    /// `TUNSETIFF` on `name`, as Firecracker attaches to its TAP: the errno, or 0.
    fn attach_tap(name: &str) -> i32 {
        #[repr(C)]
        struct IfReq {
            name: [u8; libc::IFNAMSIZ],
            flags: libc::c_short,
            pad: [u8; 22],
        }
        let mut request = IfReq {
            name: [0; libc::IFNAMSIZ],
            flags: 0x0002 | 0x1000, // IFF_TAP | IFF_NO_PI
            pad: [0; 22],
        };
        request.name[..name.len()].copy_from_slice(name.as_bytes());
        // SAFETY: a NUL-terminated literal path.
        let fd = unsafe { libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        assert!(
            fd >= 0,
            "open /dev/net/tun: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `request` is a struct ifreq that outlives the call.
        let rc = unsafe { libc::ioctl(fd, 0x4004_54ca as _, &mut request as *mut IfReq) };
        let errno = if rc == 0 {
            0
        } else {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
        };
        // SAFETY: fd is ours.
        unsafe { libc::close(fd) };
        errno
    }

    /// The probe child's body. It is a test so that the test binary can run it in a child
    /// spawned through the VMM's pre_exec (`probe_reports`); run any other way it has no
    /// child to be and returns.
    #[test]
    fn confine_probe_child() {
        if std::env::var_os(PROBE_ENV).is_none() {
            return;
        }
        if std::env::var_os(PROBE_MAKE_TAP_ENV).is_some() {
            crate::network::rtnl::add_tap(PROBE_TAP).expect("making the probe's TAP");
        }
        let tap_attach_errno = attach_tap(PROBE_TAP);
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{name}:")))
                .unwrap_or_else(|| panic!("{name} in /proc/self/status"))
                .trim()
                .to_string()
        };
        let errno = |ok: bool| {
            if ok {
                0
            } else {
                std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
            }
        };
        let sysctl = "/proc/sys/net/ipv4/conf/lo/forwarding";
        let value = std::fs::read(sysctl).unwrap();
        let net_sysctl_errno = match std::fs::write(sysctl, &value) {
            Ok(()) => 0,
            Err(e) => e.raw_os_error().unwrap_or(-1),
        };
        // SAFETY: unshare of the mount namespace affects this short-lived child only.
        let unshare_errno = errno(unsafe { libc::unshare(libc::CLONE_NEWNS) } == 0);
        let mut core = libc::rlimit {
            rlim_cur: 1,
            rlim_max: 1,
        };
        // SAFETY: getrlimit writes the struct.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut core) }, 0);
        println!(
            "PROBE {}",
            serde_json::json!({
                "cap_eff": field("CapEff"),
                "cap_prm": field("CapPrm"),
                "cap_bnd": field("CapBnd"),
                "cap_amb": field("CapAmb"),
                "no_new_privs": field("NoNewPrivs"),
                "net_sysctl_errno": net_sysctl_errno,
                "unshare_errno": unshare_errno,
                "core_limit": core.rlim_max,
                "tap_attach_errno": tap_attach_errno,
            })
        );
    }

    /// Run [`confine_probe_child`] in the namespaces of a rootless holder, through the
    /// same pre_exec a VMM gets, confined or not.
    async fn probe_report(holder_pid: u32, confine: bool, make_tap: bool) -> ProbeReport {
        let mut cmd = tokio::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "confine::tests::confine_probe_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_ENV, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
        if make_tap {
            cmd.env(PROBE_MAKE_TAP_ENV, "1");
        }
        crate::utils::install_namespace_pre_exec(
            &mut cmd,
            &crate::utils::NamespaceParams {
                user_namespace_path: Some(format!("/proc/{holder_pid}/ns/user").into()),
                net_namespace_path: Some(format!("/proc/{holder_pid}/ns/net").into()),
                tap_owner: confine.then(|| PROBE_TAP.to_string()),
                ..crate::utils::NamespaceParams::new(
                    "confine-probe",
                    Decision::for_test(confine.then(|| Confinement {
                        landlock_abi: LANDLOCK_ABI_FLOOR,
                        layers: vec![Layer::Process],
                        core_limit: core_limit_for(core_hard_limit()),
                        seccomp: None,
                    })),
                )
            },
        )
        .unwrap();
        let output = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "probe failed: {}\nstdout: {stdout}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        // libtest prints the test's name and its output on one line.
        let line = stdout
            .lines()
            .find_map(|l| l.find("PROBE ").map(|at| &l[at + "PROBE ".len()..]))
            .unwrap_or_else(|| panic!("no PROBE line in {stdout}"));
        serde_json::from_str(line).unwrap()
    }

    /// Unconfined, the VMM holds every capability in the holder's user namespace and can
    /// reconfigure the namespace's network and make mount namespaces. Confined, it holds
    /// none, the exec as namespace uid 0 gave none back, and both of those fail. It still
    /// attaches to the namespace's TAP, which `rtnl::add_tap` made unowned, as the default
    /// path makes every TAP, because its pre_exec owned the TAP before dropping its
    /// capabilities: on Linux 7.3 a process without CAP_NET_ADMIN gets EPERM attaching to an
    /// unowned TAP (on 7.0 it does not).
    /// (The parent-death signal is checked on a real VMM in tests/test_confine.rs: the probe
    /// runs in a libtest thread, and a new thread's parent-death signal is always 0.)
    #[tokio::test(flavor = "current_thread")]
    async fn the_process_layer_leaves_the_vmm_no_capabilities_in_its_namespace() {
        let (mut holder, holder_pid) = crate::commands::common::spawn_namespace_holder()
            .await
            .expect("spawning the namespace holder");
        // The first probe makes the TAP as the namespace's builder does (`rtnl::add_tap`).
        let open = probe_report(holder_pid, false, true).await;
        let confined = probe_report(holder_pid, true, false).await;
        let _ = holder.start_kill();
        let _ = holder.wait().await;
        crate::companion::forget(holder_pid);
        crate::network::netns_agent::forget(holder_pid);

        // Refused either way; which errno depends on whether fcvm runs as root.
        assert!(
            [libc::EACCES, libc::EPERM].contains(&confined.net_sysctl_errno),
            "{confined:?}"
        );
        let confined = ProbeReport {
            net_sysctl_errno: libc::EACCES,
            ..confined
        };

        // The control: the probe sees what an unconfined VMM has today.
        assert_ne!(open.cap_eff, "0000000000000000", "{open:?}");
        assert_eq!(open.net_sysctl_errno, 0, "{open:?}");
        assert_eq!(open.unshare_errno, 0, "{open:?}");
        assert_eq!(open.no_new_privs, "0", "{open:?}");
        assert_eq!(open.tap_attach_errno, 0, "{open:?}");

        assert_eq!(
            confined,
            ProbeReport {
                cap_eff: "0000000000000000".to_string(),
                cap_prm: "0000000000000000".to_string(),
                cap_bnd: "0000000000000000".to_string(),
                cap_amb: "0000000000000000".to_string(),
                no_new_privs: "1".to_string(),
                net_sysctl_errno: libc::EACCES,
                unshare_errno: libc::EPERM,
                core_limit: core_limit_for(core_hard_limit()),
                tap_attach_errno: 0,
            }
        );
    }

    /// Log mode allows every syscall, so it is refused where the host's config requires
    /// confinement, unless the config accepts it; where it is accepted the start is
    /// recorded as `seccomp-log`, never as `seccomp`.
    #[test]
    fn log_mode_does_not_pass_as_the_seccomp_layer() {
        let required = |allow_seccomp_log: bool| Host {
            mode: Mode::Required,
            seccomp: SeccompMode::Log,
            allow_seccomp_log,
            ..host(Ok(11))
        };
        let refused = missing(&request(false), &required(false));
        assert_eq!(
            refused.iter().map(|m| m.mechanism).collect::<Vec<_>>(),
            vec![Mechanism::Seccomp],
            "{refused:?}"
        );
        let error = decide(&request(false), &required(false))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("seccomp: FCVM_CONFINE_SECCOMP=log allows every syscall")
                && error.contains("allow_seccomp_log = true"),
            "{error}"
        );
        // Enforce is not affected by the refusal.
        let enforce = Host {
            seccomp: SeccompMode::Enforce,
            ..required(false)
        };
        assert_eq!(
            decide(&request(false), &enforce).unwrap().unwrap().layers,
            vec![Layer::Process, Layer::Seccomp]
        );
        // Accepted by the config: confined, and recorded as what it is.
        let accepted = decide(&request(false), &required(true)).unwrap().unwrap();
        assert_eq!(accepted.layers, vec![Layer::Process, Layer::SeccompLog]);
        assert_eq!(accepted.seccomp, Some(SeccompMode::Log));
        assert_eq!(
            serde_json::to_value(&accepted.layers).unwrap(),
            serde_json::json!(["process", "seccomp-log"])
        );
    }

    /// A Firecracker stand-in in a new directory, with `filter` (and, when given, a digest
    /// record holding `recorded`) under the names the enforce filter has.
    fn store_with_filter(
        filter: Option<&[u8]>,
        recorded: Option<&str>,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let firecracker = dir.path().join("firecracker");
        std::fs::write(&firecracker, b"stand-in").unwrap();
        let path = filter_path(&firecracker, SeccompMode::Enforce);
        if let Some(filter) = filter {
            std::fs::write(&path, filter).unwrap();
        }
        if let Some(recorded) = recorded {
            std::fs::write(filter_digest_path(&path), recorded).unwrap();
        }
        (dir, firecracker)
    }

    /// The launch takes the filter only as the bytes `fcvm setup` recorded: a file that is
    /// merely there under the right name is refused, and so is a symbolic link, a file
    /// every user can write, and a file whose record does not match.
    #[test]
    fn a_filter_is_taken_only_as_the_bytes_setup_recorded() {
        use std::os::unix::fs::PermissionsExt as _;
        let good = b"compiled filter";
        let digest = filter_bytes_digest(good);
        let refusal = |filter: Option<&[u8]>, recorded: Option<&str>| {
            let (_dir, firecracker) = store_with_filter(filter, recorded);
            format!(
                "{:#}",
                verified_filter_bytes(&firecracker, SeccompMode::Enforce).unwrap_err()
            )
        };

        let (_dir, firecracker) = store_with_filter(Some(good), Some(&format!("{digest}\n")));
        assert_eq!(
            verified_filter_bytes(&firecracker, SeccompMode::Enforce).unwrap(),
            good
        );

        let missing = refusal(None, None);
        assert!(
            missing.contains("the confined seccomp filter is unusable")
                && missing.contains("fcvm setup"),
            "{missing}"
        );
        // The right name is not enough: no record, or a record of other bytes.
        let unrecorded = refusal(Some(b"three empty programs"), None);
        assert!(
            unrecorded.contains("the digest fcvm setup records"),
            "{unrecorded}"
        );
        let replaced = refusal(Some(b"three empty programs"), Some(&digest));
        assert!(
            replaced.contains("is not the one setup compiled"),
            "{replaced}"
        );

        // A symbolic link under the filter's name, to a file with the recorded bytes.
        let (dir, firecracker) = store_with_filter(None, None);
        let path = filter_path(&firecracker, SeccompMode::Enforce);
        let elsewhere = dir.path().join("elsewhere.bpf");
        std::fs::write(&elsewhere, good).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        std::fs::write(filter_digest_path(&path), &digest).unwrap();
        let linked = format!(
            "{:#}",
            verified_filter_bytes(&firecracker, SeccompMode::Enforce).unwrap_err()
        );
        assert!(linked.contains("a symbolic link is refused"), "{linked}");

        // A FIFO under the filter's name is refused at once; the open does not wait for a
        // writer.
        let (_dir, firecracker) = store_with_filter(None, Some(&digest));
        let path = filter_path(&firecracker, SeccompMode::Enforce);
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: mkfifo with a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // On its own thread, so that an open that does wait fails this test, not hangs it.
        let (answer, answered) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let refused = verified_filter_bytes(&firecracker, SeccompMode::Enforce).unwrap_err();
            let _ = answer.send(format!("{refused:#}"));
        });
        let fifo = answered
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("opening a FIFO under the filter's name waited for a writer");
        assert!(fifo.contains("is not a regular file"), "{fifo}");

        // A filter every user can write.
        let (_dir, firecracker) = store_with_filter(Some(good), Some(&digest));
        let path = filter_path(&firecracker, SeccompMode::Enforce);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let writable = format!(
            "{:#}",
            verified_filter_bytes(&firecracker, SeccompMode::Enforce).unwrap_err()
        );
        assert!(writable.contains("writable by every user"), "{writable}");

        // The preflight says nothing was started, by mechanism.
        let (_dir, firecracker) = store_with_filter(None, None);
        let confined = Decision::for_test(Some(Confinement {
            landlock_abi: LANDLOCK_ABI_FLOOR,
            layers: vec![Layer::Process, Layer::Seccomp],
            core_limit: 1,
            seccomp: Some(SeccompMode::Enforce),
        }));
        let error = confined
            .require_filter(&firecracker)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("nothing was started") && error.contains("seccomp: "),
            "{error}"
        );
        // An unconfined start needs no filter.
        Decision::for_test(None)
            .require_filter(&firecracker)
            .unwrap();
    }

    /// What the VMM opens is a sealed copy of the verified bytes: reading it back through
    /// `/proc/self/fd` gives them, and nothing can write, grow or shrink it.
    #[test]
    fn the_filter_is_handed_over_as_a_sealed_copy() {
        use std::os::fd::AsRawFd as _;
        let bytes = b"verified filter bytes".to_vec();
        let fd = sealed_memfd(&bytes).unwrap();
        let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let written = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"x"));
        assert!(written.is_err(), "the sealed memfd took a write");
        // SAFETY: fcntl and ftruncate with integer arguments on a descriptor this test owns.
        let (seals, truncated) = unsafe {
            (
                libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS),
                libc::ftruncate(fd.as_raw_fd(), 0),
            )
        };
        let all = libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        assert_eq!(seals & all, all, "seals {seals:#x}");
        assert_eq!(truncated, -1);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    /// The trap is found at the end of a log too large to read whole, past a byte that is
    /// not UTF-8. Reading the whole file as a string, as the report once did, finds nothing.
    #[test]
    fn the_trap_is_read_from_the_end_of_a_large_log_with_a_bad_byte() {
        use std::io::{Seek as _, Write as _};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firecracker.log");
        let mut log = std::fs::File::create(&path).unwrap();
        // 8 GiB of hole: no blocks, and more than a read of the whole file should cost.
        log.seek(std::io::SeekFrom::Start(8 << 30)).unwrap();
        log.write_all(
            b"\n2026-11-25T01:20:07 [anonymous-instance:fc_vcpu 0] a guest wrote \xff\xfe\n",
        )
        .unwrap();
        log.write_all(
            b"2026-11-25T01:20:07.607912353 [anonymous-instance:fc_api:ERROR:src/vmm/src/\
              signal_handler.rs:70] Shutting down VM after intercepting a bad syscall (226).\n",
        )
        .unwrap();
        drop(log);
        let started = std::time::Instant::now();
        assert_eq!(
            last_trapped_syscall(&path),
            Some(TrappedSyscall {
                thread: "fc_api".into(),
                number: 226
            })
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "reading the end of the log took {:?}",
            started.elapsed()
        );
        // A log shorter than the tail, and one with no trap.
        std::fs::write(&path, "[anonymous-instance:main] ok\n").unwrap();
        assert_eq!(last_trapped_syscall(&path), None);
        assert_eq!(last_trapped_syscall(&dir.path().join("absent.log")), None);
    }

    const SECCOMP_PROBE_ENV: &str = "FCVM_SECCOMP_PROBE_CHILD";

    /// The child of [`seccomp_probe`]: installs an allow-all seccomp filter on this thread
    /// (`one`), on every thread (`all`) or on none (`none`), with a second thread named
    /// `probe-sleeper` alive, says `READY <this thread's tid>` and waits for its stdin to
    /// close. Run any other way it has no parent to answer and returns.
    #[test]
    fn seccomp_probe_child() {
        let Ok(mode) = std::env::var(SECCOMP_PROBE_ENV) else {
            return;
        };
        let (parked, park) = std::sync::mpsc::channel::<()>();
        let (started, start) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("probe-sleeper".into())
            .spawn(move || {
                started.send(()).unwrap();
                let _ = park.recv();
            })
            .unwrap();
        start.recv().unwrap();
        let allow = [libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        }];
        let program = libc::sock_fprog {
            len: 1,
            filter: allow.as_ptr().cast_mut(),
        };
        let flags: libc::c_ulong = match mode.as_str() {
            "none" => u64::MAX as libc::c_ulong,
            "one" => 0,
            "all" => libc::SECCOMP_FILTER_FLAG_TSYNC,
            other => panic!("unknown probe mode {other}"),
        };
        if mode != "none" {
            // SAFETY: prctl and seccomp with a program that outlives the calls.
            unsafe {
                assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
                assert_eq!(
                    libc::syscall(
                        libc::SYS_seccomp,
                        libc::SECCOMP_SET_MODE_FILTER,
                        flags,
                        &program as *const libc::sock_fprog,
                    ),
                    0,
                    "seccomp: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        // SAFETY: gettid has no arguments and cannot fail.
        println!("READY {}", unsafe { libc::gettid() });
        let mut rest = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut rest);
        drop(parked);
    }

    /// [`unfiltered_threads`] of a child running [`seccomp_probe_child`] in `mode`, and the
    /// tid of the child's thread that installed the filter.
    fn seccomp_probe(mode: &str) -> (Vec<String>, String) {
        use std::io::BufRead as _;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "confine::tests::seccomp_probe_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SECCOMP_PROBE_ENV, mode)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        // libtest prints the test's name and its output on one line.
        let tid = lines
            .find_map(|line| {
                let line = line.unwrap();
                line.find("READY ")
                    .map(|at| line[at + "READY ".len()..].trim().to_string())
            })
            .expect("the probe child said READY");
        let unfiltered = unfiltered_threads(child.id());
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert!(status.success(), "probe child ({mode}): {status}");
        (unfiltered.unwrap(), tid)
    }

    /// The check a confined start makes of its VMM: a process is filtered only when every
    /// thread is, and a thread that is not is named.
    #[test]
    fn a_thread_without_a_seccomp_filter_is_named() {
        let (all, _) = seccomp_probe("all");
        assert_eq!(all, Vec::<String>::new());

        // The filter is on the thread that installed it and on no other.
        let (one, tid) = seccomp_probe("one");
        assert!(
            one.iter().any(|t| t.starts_with("probe-sleeper (tid ")),
            "{one:?}"
        );
        assert!(
            one.iter()
                .all(|t| t.contains("Seccomp: 0, Seccomp_filters: 0")),
            "{one:?}"
        );
        assert!(
            !one.iter().any(|t| t.contains(&format!("(tid {tid},"))),
            "the filtered thread {tid} is listed: {one:?}"
        );

        let (none, tid) = seccomp_probe("none");
        assert!(
            none.iter().any(|t| t.contains(&format!("(tid {tid},"))),
            "{none:?}"
        );
        assert!(none.len() >= 3, "{none:?}");

        // A process that is gone is an error, never an empty list.
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let pid = gone.id();
        gone.wait().unwrap();
        assert!(unfiltered_threads(pid).is_err());
    }

    /// Only the Firecracker launch hands a VMM the confined filter, so every other place
    /// that spawns through the VMM pre_exec must be unable to get a confined decision: its
    /// request names why confinement is unsupported there, or its backend is one
    /// confinement refuses. The Firecracker launch itself verifies the filter or bails.
    #[test]
    fn every_vmm_spawn_site_adds_the_filter_or_cannot_be_confined() {
        let sources = sources();
        let code = |path: &str| {
            let (_, text) = sources
                .iter()
                .find(|(p, _)| p == path)
                .unwrap_or_else(|| panic!("{path}"));
            non_test(text).to_string()
        };
        // Each resolve in these files asks with a reason confinement is unsupported.
        for path in ["src/commands/vm.rs", "src/setup/rootfs.rs"] {
            let text = code(path);
            let requests: Vec<&str> = text
                .match_indices("crate::confine::Request {")
                .map(|(at, _)| &text[at..at + text[at..].find("\n    }").unwrap()])
                .collect();
            assert!(!requests.is_empty(), "{path} builds no confinement request");
            for request in requests {
                assert!(
                    request.contains("unsupported: Some("),
                    "{path} can be given a confined decision and adds no filter: {request}"
                );
            }
        }
        let unsupported = Request {
            unsupported: Some("not here"),
            ..request(true)
        };
        assert!(decide(&unsupported, &host(Ok(11))).is_err());
        let required = Host {
            mode: Mode::Required,
            ..host(Ok(11))
        };
        assert!(decide(
            &Request {
                flag: false,
                ..unsupported
            },
            &required
        )
        .is_err());
        let cloud_hypervisor = Request {
            backend: Backend::CloudHypervisor,
            ..request(true)
        };
        assert!(decide(&cloud_hypervisor, &host(Ok(11))).is_err());

        let launch = code("src/firecracker/vm.rs");
        let start = &launch[launch.find("pub async fn start(").unwrap()..];
        let verify = start
            .find("crate::confine::verified_filter_bytes(")
            .expect("the launch verifies the filter");
        let spawn = start
            .find("install_namespace_pre_exec(")
            .expect("the launch spawns through the VMM pre_exec");
        assert!(
            verify < spawn,
            "the launch spawns before it verifies the filter"
        );
        assert!(
            start[..spawn].contains("names no seccomp mode"),
            "a confined decision without a seccomp mode reaches the spawn"
        );
    }
}
