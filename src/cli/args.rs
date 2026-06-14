use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

#[derive(Parser, Debug)]
#[command(
    name = "fcvm",
    version,
    about = "Firecracker VM runner for Podman containers"
)]
pub struct Cli {
    /// Running as a subprocess (disables timestamp and level in logs)
    #[arg(long, global = true)]
    pub sub_process: bool,

    #[command(subcommand)]
    pub cmd: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// List running VMs
    Ls(LsArgs),
    /// Podman-compatible container operations
    Podman(Box<PodmanArgs>),
    /// Snapshot operations (create, serve, run)
    Snapshot(SnapshotArgs),
    /// Manage stored snapshots (list, delete, prune)
    Snapshots(SnapshotsArgs),
    /// Execute a command in a running VM
    Exec(ExecArgs),
    /// Setup kernel and rootfs (kernel ~15MB download, rootfs ~10GB creation, takes 5-10 minutes)
    Setup(SetupArgs),
    /// Run HTTP/WebSocket API server for ComputeSDK integration
    Serve(ServeArgs),
    /// Generate shell completions
    Completions(CompletionsArgs),
}

// ============================================================================
// Setup Command
// ============================================================================

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Generate default config file at ~/.config/fcvm/rootfs-config.toml and exit
    #[arg(long)]
    pub generate_config: bool,

    /// Overwrite existing config when using --generate-config
    #[arg(long, requires = "generate_config")]
    pub force: bool,

    /// Path to custom rootfs config file
    #[arg(long)]
    pub config: Option<String>,

    /// Setup a kernel profile (e.g., "nested" for nested virtualization)
    /// Profiles are defined in rootfs-config.toml under [kernel_profiles.*]
    #[arg(long)]
    pub kernel_profile: Option<String>,

    /// Build kernels locally instead of downloading from releases
    /// (use if download fails or you've modified kernel sources)
    #[arg(long)]
    pub build_kernels: bool,

    /// Override rootfs filesystem type from kernel profile config (for testing).
    #[arg(long, value_enum, hide = true)]
    pub rootfs_type: Option<RootfsType>,

    /// Install kernel as the host kernel and configure GRUB.
    /// Requires --kernel-profile flag. After setup, reboot to activate.
    #[arg(long, requires = "kernel_profile")]
    pub install_host_kernel: bool,
}

// ============================================================================
// Serve Command
// ============================================================================

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Port to listen on
    #[arg(long, default_value_t = 8090)]
    pub port: u16,
}

// ============================================================================
// Completions Command
// ============================================================================

#[derive(Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    #[arg(value_enum)]
    pub shell: Shell,
}

// ============================================================================
// Podman Commands
// ============================================================================

#[derive(Args, Debug)]
pub struct PodmanArgs {
    #[command(subcommand)]
    pub cmd: PodmanCommands,
}

#[derive(Subcommand, Debug)]
pub enum PodmanCommands {
    /// Run a container in a Firecracker VM
    Run(RunArgs),
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// VM name (required)
    #[arg(long)]
    pub name: String,

    /// vCPUs, or "unlimited" to use all host CPUs (default: 2, max: 32)
    #[arg(long, default_value = "2", value_parser = parse_cpu)]
    pub cpu: u8,

    /// Memory in MiB, or "unlimited" to use all host memory (default: 1024)
    #[arg(long, default_value = "1024", value_parser = parse_mem)]
    pub mem: u32,

    /// Enable 2MB hugepage-backed VM memory for improved TLB performance.
    /// Requires pre-allocated hugepage pool on the host.
    /// Memory size (--mem) must be divisible by 2 when using hugepages.
    /// Snapshot restore uses UFFD page fault handler automatically.
    #[arg(long)]
    pub hugepages: bool,

    /// Minimum free space on root filesystem (default: 10G).
    /// Disk is expanded after CoW copy if free space is below this threshold.
    #[arg(long, default_value = "10G")]
    pub rootfs_size: String,

    /// Volume mapping(s): HOST:GUEST[:ro] (repeat for multiple)
    #[arg(long, action = clap::ArgAction::Append)]
    pub map: Vec<String>,

    /// Use portable inode numbering for FUSE volumes.
    /// Enables deterministic inodes based on file paths instead of host inodes,
    /// allowing snapshots with volumes to be restored on different machines.
    #[arg(long)]
    pub portable_volumes: bool,

    /// Extra disk(s): HOST_PATH:GUEST_MOUNT[:ro] (repeat for multiple)
    /// Disks appear as /dev/vdb, /dev/vdc, etc. in order specified.
    /// Mounted at GUEST_MOUNT in both VM and container.
    /// Read-only disks (:ro) can be used with snapshots/clones.
    /// Read-write disks block snapshot/clone operations.
    /// Example: --disk /data.raw:/data --disk /scratch.raw:/scratch:ro
    #[arg(long, action = clap::ArgAction::Append)]
    pub disk: Vec<String>,

    /// Create disk image from directory: HOST_DIR:GUEST_MOUNT[:ro]
    /// Creates an ext4 image from HOST_DIR contents and mounts at GUEST_MOUNT.
    /// Image is stored in VM's data directory and cleaned up on exit.
    /// Example: --disk-dir ./mydata:/data:ro
    #[arg(long, action = clap::ArgAction::Append)]
    pub disk_dir: Vec<String>,

    /// Share directory via NFS: HOST_DIR:GUEST_MOUNT[:ro]
    /// Starts NFS server on host, VM mounts via network.
    /// Requires NFS kernel support (use --kernel-profile nested or --build-kernels).
    /// Example: --nfs /data:/mnt/data:ro
    #[arg(long, action = clap::ArgAction::Append)]
    pub nfs: Vec<String>,

    /// Environment vars KEY=VALUE (repeat for multiple; values may contain commas)
    #[arg(long, action = clap::ArgAction::Append)]
    pub env: Vec<String>,

    /// Labels KEY=VALUE for tagging VMs (repeat for multiple)
    #[arg(long, action = clap::ArgAction::Append)]
    pub label: Vec<String>,

    /// Command to run inside container
    ///
    /// Example: --cmd "nginx -g 'daemon off;'"
    #[arg(long)]
    pub cmd: Option<String>,

    /// Publish host ports to guest
    /// Grammar: [HOSTIP:]HOSTPORT:GUESTPORT[/PROTO], comma-separated or repeated
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub publish: Vec<String>,

    /// Balloon device target MiB. If not specified, no balloon device is configured
    #[arg(long)]
    pub balloon: Option<u32>,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Rootless)]
    pub network: NetworkMode,

    /// VMM backend: firecracker (default) or cloud-hypervisor (#632).
    #[arg(long, value_enum, default_value_t = Hypervisor::Firecracker)]
    pub hypervisor: Hypervisor,

    /// Routable IPv6 /64 prefix for routed mode VM addressing.
    /// Each VM gets a unique address in this prefix via NDP proxy.
    /// When set, MASQUERADE is skipped (the prefix is directly routable).
    /// When not set, auto-detected from host interfaces.
    /// Example: --ipv6-prefix 2803:6084:7058:46f6
    #[arg(long)]
    pub ipv6_prefix: Option<String>,

    /// HTTP health check URL. If not specified, health is based on container running status.
    /// The URL hostname is sent as the Host header; the connection goes to the guest IP.
    /// Example: --health-check http://myapp.example.com/status
    #[arg(long)]
    pub health_check: Option<String>,

    /// Timeout in seconds for HTTP health check requests. Default: 5.
    #[arg(long, default_value = "5")]
    pub health_check_timeout: u64,

    /// Run container as USER:GROUP (e.g., --user 1000:1000)
    /// Equivalent to podman run --userns=keep-id on the host
    #[arg(long)]
    pub user: Option<String>,

    /// Forward specific localhost ports to the host gateway via TCP proxy.
    /// Enables containers to reach host-only services via localhost.
    /// Supported with rootless and routed networking (not bridged).
    /// Comma-separated port list, e.g., --forward-localhost 1421,9099
    #[arg(long, value_delimiter = ',')]
    pub forward_localhost: Vec<u16>,

    /// Run container in privileged mode (allows mknod, device access, etc.)
    /// Use for POSIX compliance tests that need full filesystem capabilities
    #[arg(long)]
    pub privileged: bool,

    /// Keep STDIN open even if not attached
    #[arg(short, long)]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short, long)]
    pub tty: bool,

    /// Debug fc-agent with strace (output to /tmp/fc-agent.strace in guest)
    /// Useful for diagnosing fc-agent startup issues
    #[arg(long)]
    pub strace_agent: bool,

    /// Run setup if kernel/rootfs are missing (takes 5-10 minutes on first run)
    /// Without this flag, fcvm will fail if setup hasn't been run
    #[arg(long)]
    pub setup: bool,

    /// Custom kernel path (overrides default kernel from setup)
    #[arg(long)]
    pub kernel: Option<String>,

    /// Kernel profile to use (e.g., "nested" for nested virtualization)
    /// Must be set up first with: fcvm setup --kernel-profile <name>
    #[arg(long)]
    pub kernel_profile: Option<String>,

    /// Directory for vsock socket (default: auto-generated in vm-disks)
    /// Use this to create a predictable socket path for external listeners.
    /// Example: --vsock-dir /tmp/myvm creates /tmp/myvm/vsock.sock
    #[arg(long)]
    pub vsock_dir: Option<String>,

    /// Disable automatic snapshot cache (bypass snapshot lookup and creation).
    /// By default, fcvm creates snapshots after container image pull for fast subsequent launches.
    #[arg(long)]
    pub no_snapshot: bool,

    /// Use non-blocking writes for container stdout/stderr on the host side.
    /// Without this flag, a slow or broken pipe reader (e.g., `fcvm ... | slow-consumer`)
    /// backpressures the entire output pipeline into the container, potentially deadlocking
    /// FUSE-based services like configerator_fuse. With this flag, output that can't be
    /// written immediately is dropped, keeping the container healthy at the cost of lost logs.
    #[arg(long)]
    pub non_blocking_output: bool,

    /// Override rootfs filesystem type from kernel profile config (for testing).
    /// Normally driven by rootfs_type in kernel profile config.
    #[arg(long, value_enum, hide = true)]
    pub rootfs_type: Option<RootfsType>,

    /// Image delivery mode for localhost images (default: auto-detect from kernel profile).
    /// overlay: pre-built overlay storage (instant), btrfs: pre-built btrfs image (instant),
    /// archive: docker archive via podman load (slow).
    #[arg(long, value_enum)]
    pub image_mode: Option<ImageMode>,

    /// Container image (e.g., nginx:alpine or localhost/myimage)
    pub image: String,

    /// Command and arguments to run in container (alternative to --cmd)
    /// Example: fcvm podman run --name foo --network bridged alpine:latest sh -c "echo hello"
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command_args: Vec<String>,

    /// Internal (not a CLI flag): cold-boot from this captured disk instead of the
    /// content-addressed base rootfs. Set by the disk-only clone dispatcher.
    #[arg(skip)]
    pub rootfs_override: Option<std::path::PathBuf>,

    /// Internal (not a CLI flag): attach this pre-built read-only image device
    /// (overlay additionalImageStore / docker archive) instead of exporting one.
    /// Set by the disk-only clone dispatcher from snapshot metadata so the captured
    /// container's image layers stay reachable.
    #[arg(skip)]
    pub image_disk_override: Option<std::path::PathBuf>,
}

// ============================================================================
// Snapshot Commands
// ============================================================================

#[derive(Args, Debug)]
pub struct SnapshotArgs {
    #[command(subcommand)]
    pub cmd: SnapshotCommands,
}

#[derive(Subcommand, Debug)]
pub enum SnapshotCommands {
    /// Create snapshot from a running VM
    Create(SnapshotCreateArgs),
    /// Serve snapshot memory for cloning
    Serve(SnapshotServeArgs),
    /// Run a clone from a snapshot
    Run(SnapshotRunArgs),
    /// List running snapshot servers
    Ls,
}

#[derive(Args, Debug)]
pub struct SnapshotCreateArgs {
    /// VM name to snapshot (mutually exclusive with --pid)
    #[arg(conflicts_with = "pid")]
    pub name: Option<String>,

    /// VM PID to snapshot (mutually exclusive with name)
    #[arg(long, conflicts_with = "name")]
    pub pid: Option<u32>,

    /// Optional: custom snapshot name (defaults to VM name)
    #[arg(long)]
    pub tag: Option<String>,

    /// Capture only the disk (no memory image). Clones cold-boot fresh from the
    /// captured disk instead of resuming via UFFD. See docs/disk-only-clone.html.
    #[arg(long)]
    pub disk_only: bool,
}

#[derive(Args, Debug)]
pub struct SnapshotServeArgs {
    /// Snapshot name to serve
    pub snapshot_name: String,
}

#[derive(Args, Debug)]
pub struct SnapshotRunArgs {
    /// Serve process PID to clone from (UFFD mode - lazy on-demand paging)
    #[arg(long, conflicts_with = "snapshot")]
    pub pid: Option<u32>,

    /// Snapshot name to clone from (direct file mode - no UFFD server needed)
    #[arg(long, conflicts_with = "pid")]
    pub snapshot: Option<String>,

    /// Optional: custom name for cloned VM (auto-generated if not provided)
    #[arg(long)]
    pub name: Option<String>,

    /// Execute command in container after clone is healthy (like fcvm exec -c)
    #[arg(long)]
    pub exec: Option<String>,

    /// Disable KVM dirty page tracking, avoiding its logging overhead.
    /// Tradeoff: diff snapshots from this VM won't work. Note: file-backed
    /// clone memory is shared through the host page cache with tracking on
    /// OR off (measured in #632) — this flag does not change sharing.
    #[arg(long)]
    pub no_dirty_tracking: bool,

    /// Disable swap for the Firecracker process (sets memory.swap.max=0 on its
    /// cgroup). Prevents the kernel from swapping guest memory pages, forcing it
    /// to evict file cache instead. Useful for large VMs where swap I/O would
    /// degrade performance.
    #[arg(long)]
    pub no_swap: bool,

    // ========================================================================
    // Internal fields - not exposed via CLI, used for startup snapshot support
    // ========================================================================
    /// Base snapshot key for startup snapshot creation (internal use only).
    /// When set, a startup snapshot will be created after the VM becomes healthy.
    #[arg(skip)]
    pub startup_snapshot_base_key: Option<String>,

    /// vCPUs (internal use only).
    /// Passed from podman run's --cpu when restoring from a snapshot.
    #[arg(skip)]
    pub cpu: Option<u8>,

    /// Memory in MiB (internal use only).
    /// Passed from podman run's --mem when restoring from a snapshot.
    #[arg(skip)]
    pub mem: Option<u32>,

    /// Firecracker binary path (internal use only).
    /// Passed from podman run runtime config when restoring from a snapshot cache hit.
    #[arg(skip)]
    pub firecracker_bin: Option<String>,

    /// Extra Firecracker args (internal use only).
    /// Passed from podman run runtime config when restoring from a snapshot cache hit.
    #[arg(skip)]
    pub firecracker_args: Option<String>,

    /// Whether hugepages are enabled (internal use only).
    /// Passed from podman run's --hugepages when restoring from a snapshot cache hit.
    #[arg(skip)]
    pub hugepages: Option<bool>,

    /// Whether to use non-blocking output on the host side (internal use only).
    /// Passed from podman run's --non-blocking-output when restoring from a snapshot.
    #[arg(skip)]
    pub non_blocking_output: bool,
}

// ============================================================================
// Snapshots Management Commands (list, delete, prune stored snapshots)
// ============================================================================

#[derive(Args, Debug)]
pub struct SnapshotsArgs {
    #[command(subcommand)]
    pub cmd: SnapshotsCommands,
}

#[derive(Subcommand, Debug)]
pub enum SnapshotsCommands {
    /// List all stored snapshots
    Ls(SnapshotsLsArgs),
    /// Delete a specific snapshot
    Delete(SnapshotsDeleteArgs),
    /// Delete all system (auto-generated) snapshots
    Prune(SnapshotsPruneArgs),
}

#[derive(Args, Debug)]
pub struct SnapshotsLsArgs {
    /// Output in JSON format
    #[arg(long)]
    pub json: bool,

    /// Filter by type: user or system
    #[arg(long, value_enum)]
    pub filter: Option<SnapshotTypeFilter>,

    /// Filter by container image name
    #[arg(long)]
    pub image: Option<String>,

    /// Show accurate disk usage accounting for btrfs shared extents (slower)
    #[arg(long)]
    pub shared: bool,
}

#[derive(Args, Debug)]
pub struct SnapshotsDeleteArgs {
    /// Name of the snapshot to delete
    pub name: String,

    /// Force deletion without confirmation
    #[arg(short, long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct SnapshotsPruneArgs {
    /// Force deletion without confirmation
    #[arg(short, long)]
    pub force: bool,

    /// Delete ALL snapshots (including user-created ones)
    #[arg(long)]
    pub all: bool,

    /// Only delete snapshots matching this container image name
    #[arg(long)]
    pub image: Option<String>,
}

/// Filter for snapshot type in list command
#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum SnapshotTypeFilter {
    /// User-created snapshots (via fcvm snapshot create)
    User,
    /// System-generated snapshots (auto-created cache)
    System,
}

// ============================================================================
// Shared Args
// ============================================================================
// Enums
// ============================================================================

/// Network mode for VM networking
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, ValueEnum)]
pub enum NetworkMode {
    /// Bridged networking using network namespaces (requires sudo)
    #[default]
    Bridged,
    /// True rootless networking using pasta (no sudo required)
    Rootless,
    /// Routed networking using veth + IPv6 routing (requires sudo, no NAT needed)
    Routed,
}

/// VMM backend selection (#632). Maps to [`crate::hypervisor::Backend`].
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, ValueEnum)]
pub enum Hypervisor {
    /// Firecracker (default).
    #[default]
    Firecracker,
    /// Cloud Hypervisor.
    CloudHypervisor,
}

impl From<Hypervisor> for crate::hypervisor::Backend {
    fn from(h: Hypervisor) -> Self {
        match h {
            Hypervisor::Firecracker => crate::hypervisor::Backend::Firecracker,
            Hypervisor::CloudHypervisor => crate::hypervisor::Backend::CloudHypervisor,
        }
    }
}

/// Root filesystem type for the VM.
///
/// Controls whether the rootfs is ext4 (default) or btrfs.
/// Normally driven by the kernel profile config; this enum enables CLI override for testing.
#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum RootfsType {
    /// Standard ext4 rootfs (default)
    Ext4,
    /// btrfs rootfs (converted from ext4 via btrfs-convert)
    Btrfs,
}

/// Image delivery mode for localhost container images.
///
/// Controls how pre-built container images are delivered to the guest VM.
/// Auto-detected from kernel profile if not specified.
#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum ImageMode {
    /// Pre-built overlay storage image mounted as additionalImageStore (read-only, instant)
    Overlay,
    /// Pre-built btrfs storage image with real subvolumes, reflink-copied as graphroot (read-write, instant)
    Btrfs,
    /// Docker archive loaded via podman at boot (slow, works with any storage driver)
    Archive,
}

// ============================================================================
// Ls Command
// ============================================================================

#[derive(Args, Debug)]
pub struct LsArgs {
    /// Output in JSON format
    #[arg(long)]
    pub json: bool,

    /// Filter by fcvm process PID
    #[arg(long)]
    pub pid: Option<u32>,
}

// ============================================================================
// Exec Command
// ============================================================================

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// VM PID to exec into (mutually exclusive with name)
    #[arg(long, conflicts_with = "name")]
    pub pid: Option<u32>,

    /// Execute in the VM instead of inside the container
    #[arg(long)]
    pub vm: bool,

    /// Execute inside container (default, mutually exclusive with --vm)
    #[arg(short, long)]
    pub container: bool,

    /// Keep STDIN open even if not attached
    #[arg(short, long)]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short, long)]
    pub tty: bool,

    /// Suppress log output (auto-enabled with -t)
    #[arg(short, long)]
    pub quiet: bool,

    /// VM name to exec into (mutually exclusive with --pid)
    #[arg(long, conflicts_with = "pid")]
    pub name: Option<String>,

    /// Command and arguments to execute
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    pub command: Vec<String>,
}

/// Parse --mem value: either an integer (MiB) or "unlimited" (all host memory).
fn parse_mem(s: &str) -> Result<u32, String> {
    if s.eq_ignore_ascii_case("unlimited") {
        crate::host_memory_mib()
            .ok_or_else(|| "failed to read host memory from /proc/meminfo".to_string())
    } else {
        s.parse::<u32>().map_err(|_| {
            format!(
                "invalid --mem value '{}': expected integer (MiB) or 'unlimited'",
                s
            )
        })
    }
}

/// Parse --cpu value: either an integer or "unlimited" (all host CPUs, capped at 32).
fn parse_cpu(s: &str) -> Result<u8, String> {
    if s.eq_ignore_ascii_case("unlimited") {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get().min(32) as u8)
            .unwrap_or(2);
        Ok(cpus)
    } else {
        s.parse::<u8>().map_err(|_| {
            format!(
                "invalid --cpu value '{}': expected integer or 'unlimited'",
                s
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Parse a `fcvm podman run` command line and return the RunArgs.
    fn parse_run(extra: &[&str]) -> RunArgs {
        let mut argv = vec!["fcvm", "podman", "run", "--name", "test"];
        argv.extend_from_slice(extra);
        let cli = Cli::try_parse_from(argv).expect("CLI should parse");
        match cli.cmd {
            Commands::Podman(podman) => match podman.cmd {
                PodmanCommands::Run(run) => run,
            },
            _ => panic!("expected `podman run` command"),
        }
    }

    #[test]
    fn env_value_with_comma_is_not_split() {
        let run = parse_run(&["--env", "FLAGS=a,b", "nginx:alpine"]);
        assert_eq!(run.env, vec!["FLAGS=a,b"]);
    }

    #[test]
    fn repeated_env_flags_append() {
        let run = parse_run(&["--env", "A=1", "--env", "B=2", "nginx:alpine"]);
        assert_eq!(run.env, vec!["A=1", "B=2"]);
    }

    #[test]
    fn label_and_map_values_with_commas_are_not_split() {
        let run = parse_run(&[
            "--label",
            "notes=a,b",
            "--map",
            "/host/dir,with-comma:/guest",
            "nginx:alpine",
        ]);
        assert_eq!(run.label, vec!["notes=a,b"]);
        assert_eq!(run.map, vec!["/host/dir,with-comma:/guest"]);
    }

    #[test]
    fn publish_and_forward_localhost_split_on_commas() {
        let run = parse_run(&[
            "--publish",
            "8080:80,8443:443",
            "--forward-localhost",
            "1421,9099",
            "nginx:alpine",
        ]);
        assert_eq!(run.publish, vec!["8080:80", "8443:443"]);
        assert_eq!(run.forward_localhost, vec![1421u16, 9099]);
    }
}
