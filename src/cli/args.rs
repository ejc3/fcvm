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

    /// vCPUs (0 = all host CPUs)
    #[arg(long, default_value_t = 0)]
    pub cpu: u8,

    /// Memory in MiB, or "unlimited" to use all host memory (default: 2048)
    #[arg(long, default_value = "2048", value_parser = parse_mem)]
    pub mem: u32,

    /// Minimum free space on root filesystem (default: 10G).
    /// Disk is expanded after CoW copy if free space is below this threshold.
    #[arg(long, default_value = "10G")]
    pub rootfs_size: String,

    /// Volume mapping(s): HOST:GUEST[:ro] (repeat or comma-separated)
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub map: Vec<String>,

    /// Extra disk(s): HOST_PATH:GUEST_MOUNT[:ro] (repeat or comma-separated)
    /// Disks appear as /dev/vdb, /dev/vdc, etc. in order specified.
    /// Mounted at GUEST_MOUNT in both VM and container.
    /// Read-only disks (:ro) can be used with snapshots/clones.
    /// Read-write disks block snapshot/clone operations.
    /// Example: --disk /data.raw:/data --disk /scratch.raw:/scratch:ro
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub disk: Vec<String>,

    /// Create disk image from directory: HOST_DIR:GUEST_MOUNT[:ro]
    /// Creates an ext4 image from HOST_DIR contents and mounts at GUEST_MOUNT.
    /// Image is stored in VM's data directory and cleaned up on exit.
    /// Example: --disk-dir ./mydata:/data:ro
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub disk_dir: Vec<String>,

    /// Share directory via NFS: HOST_DIR:GUEST_MOUNT[:ro]
    /// Starts NFS server on host, VM mounts via network.
    /// Requires NFS kernel support (use --kernel-profile nested or --build-kernels).
    /// Example: --nfs /data:/mnt/data:ro
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub nfs: Vec<String>,

    /// Environment vars KEY=VALUE (repeat or comma-separated)
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub env: Vec<String>,

    /// Labels KEY=VALUE for tagging VMs (repeat or comma-separated)
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
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

    /// HTTP health check URL. If not specified, health is based on container running status.
    /// Example: --health-check http://localhost/health
    #[arg(long)]
    pub health_check: Option<String>,

    /// Run container as USER:GROUP (e.g., --user 1000:1000)
    /// Equivalent to podman run --userns=keep-id on the host
    #[arg(long)]
    pub user: Option<String>,

    /// Forward specific localhost ports to the host gateway via iptables DNAT.
    /// Enables containers to reach host-only services via localhost.
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

    /// Container image (e.g., nginx:alpine or localhost/myimage)
    pub image: String,

    /// Command and arguments to run in container (alternative to --cmd)
    /// Example: fcvm podman run --name foo --network bridged alpine:latest sh -c "echo hello"
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command_args: Vec<String>,
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
}

#[derive(Args, Debug)]
pub struct SnapshotServeArgs {
    /// Snapshot name to serve
    pub snapshot_name: String,
}

#[derive(Args, Debug)]
pub struct SnapshotRunArgs {
    /// Serve process PID to clone from (UFFD mode - memory sharing)
    #[arg(long, conflicts_with = "snapshot")]
    pub pid: Option<u32>,

    /// Snapshot name to clone from (direct file mode - no UFFD server needed)
    #[arg(long, conflicts_with = "pid")]
    pub snapshot: Option<String>,

    /// Optional: custom name for cloned VM (auto-generated if not provided)
    #[arg(long)]
    pub name: Option<String>,

    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub publish: Vec<String>,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Rootless)]
    pub network: NetworkMode,

    /// Execute command in container after clone is healthy (like fcvm exec -c)
    #[arg(long)]
    pub exec: Option<String>,

    /// Allocate a pseudo-TTY for the container
    #[arg(short, long)]
    pub tty: bool,

    /// Keep STDIN open for interactive mode
    #[arg(short, long)]
    pub interactive: bool,

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
    /// True rootless networking using slirp4netns (no sudo required)
    Rootless,
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
        crate::host_memory_mib().ok_or_else(|| "failed to read host memory from /proc/meminfo".to_string())
    } else {
        s.parse::<u32>().map_err(|_| {
            format!("invalid --mem value '{}': expected integer (MiB) or 'unlimited'", s)
        })
    }
}
