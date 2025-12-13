use clap::{Args, Parser, Subcommand, ValueEnum};

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

    /// Base directory for all fcvm data (default: /mnt/fcvm-btrfs or FCVM_BASE_DIR env)
    #[arg(long, global = true, env = "FCVM_BASE_DIR")]
    pub base_dir: Option<String>,

    #[command(subcommand)]
    pub cmd: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// List running VMs
    Ls(LsArgs),
    /// Podman-compatible container operations
    Podman(PodmanArgs),
    /// Snapshot operations (create, serve, run)
    Snapshot(SnapshotArgs),
    /// List available snapshots
    Snapshots,
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
    /// Container image (e.g., nginx:alpine or localhost/myimage)
    pub image: String,

    /// VM name (required)
    #[arg(long)]
    pub name: String,

    /// vCPUs
    #[arg(long, default_value_t = 2)]
    pub cpu: u8,

    /// Memory (MiB)
    #[arg(long, default_value_t = 2048)]
    pub mem: u32,

    /// Volume mapping(s): HOST:GUEST[:ro]
    #[arg(long, num_args=0.., value_delimiter=',')]
    pub map: Vec<String>,

    /// Environment vars KEY=VALUE (repeat or comma-separated)
    #[arg(long, num_args=0.., value_delimiter=',')]
    pub env: Vec<String>,

    /// Command to run inside container
    #[arg(long)]
    pub cmd: Option<String>,

    /// Publish host ports to guest
    /// Grammar: [HOSTIP:]HOSTPORT:GUESTPORT[/PROTO], comma-separated
    #[arg(long, num_args=0.., value_delimiter=',')]
    pub publish: Vec<String>,

    /// Balloon device target MiB. If not specified, no balloon device is configured
    #[arg(long)]
    pub balloon: Option<u32>,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,

    /// HTTP health check URL. If not specified, health is based on container running status.
    /// Example: --health-check http://localhost/health
    #[arg(long)]
    pub health_check: Option<String>,

    /// Run container in privileged mode (allows mknod, device access, etc.)
    /// Use for POSIX compliance tests that need full filesystem capabilities
    #[arg(long)]
    pub privileged: bool,
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
    /// Serve process PID to clone from
    #[arg(long)]
    pub pid: u32,

    /// Optional: custom name for cloned VM (auto-generated if not provided)
    #[arg(long)]
    pub name: Option<String>,

    #[arg(long, num_args=0.., value_delimiter=',')]
    pub publish: Vec<String>,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,
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
