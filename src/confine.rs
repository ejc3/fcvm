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
}

static CONFIG_MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();

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
}

/// How a VM's VMM was confined, recorded in its state and shown by `fcvm ls`. It is a
/// property of the VMM process: it enters no snapshot, cache or pool key, so a confined
/// clone restores a snapshot an unconfined VMM wrote and the reverse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confinement {
    /// The kernel's Landlock ABI when the VM started.
    pub landlock_abi: u32,
    pub layers: Vec<Layer>,
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
    /// `/proc/sys/kernel/core_pattern`, or `None` when it cannot be read.
    pub core_pattern: Option<String>,
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
    pub fn read(mode: Mode) -> anyhow::Result<Self> {
        Ok(Host {
            mode,
            landlock_abi: landlock_abi(),
            core_pattern: std::fs::read_to_string("/proc/sys/kernel/core_pattern")
                .ok()
                .map(|p| p.trim_end().to_string()),
            strace_fc: std::env::var_os("FCVM_STRACE_FC").is_some(),
            no_seccomp: std::env::var_os("FCVM_NO_SECCOMP").is_some(),
            firecracker_args: std::env::var("FCVM_FIRECRACKER_ARGS").ok(),
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
            layers: vec![Layer::Process],
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
    let host = Host::read(mode)?;
    let decided = decide(request, &host)?;
    if let Some(confinement) = &decided {
        tracing::info!(
            landlock_abi = confinement.landlock_abi,
            layers = ?confinement.layers,
            "the VMM will be confined"
        );
    }
    Ok(Decision(decided))
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
pub(crate) fn apply_process_layer() -> std::io::Result<()> {
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

    // 1, not 0: the kernel takes a limit of 1 as "no dump" for a pipe core_pattern too,
    // where it ignores any other limit (fs/coredump.c, coredump_pipe); a file dump needs at
    // least a page.
    let no_core = libc::rlimit {
        rlim_cur: 1,
        rlim_max: 1,
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
        }
    }

    fn request(flag: bool) -> Request<'static> {
        Request {
            flag,
            backend: Backend::Firecracker,
            network: Network::Rootless,
            firecracker_args: None,
            unsupported: None,
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
                layers: vec![Layer::Process],
            })
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
        };
        let host = Host {
            mode: Mode::Off,
            landlock_abi: Ok(6),
            strace_fc: true,
            no_seccomp: true,
            firecracker_args: Some("--seccomp-filter /x.bpf".to_string()),
            core_pattern: Some("|/usr/lib/systemd/systemd-coredump %P".to_string()),
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
                core_limit: 1,
                tap_attach_errno: 0,
            }
        );
    }
}
