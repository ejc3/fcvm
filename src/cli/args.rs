use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "fcvm",
    version,
    about = "Firecracker VM runner for Podman containers"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Podman-compatible container operations
    Podman(PodmanArgs),
    /// Snapshot operations (create, serve, run)
    Snapshot(SnapshotArgs),
    /// List available snapshots
    Snapshots,
    /// View VM logs
    Logs(NameArgs),
    /// Inspect VM details
    Inspect(NameArgs),
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
    /// Container image (e.g., nginx:latest) or directory to build
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

    /// Host mode
    #[arg(long, value_enum, default_value_t = ModeOpt::Auto)]
    pub mode: ModeOpt,

    /// Volume mapping(s): HOST:GUEST[:ro]
    #[arg(long, num_args=0.., value_delimiter=',')]
    pub map: Vec<String>,

    /// Map mode: block | sshfs | nfs
    #[arg(long, value_enum, default_value_t = MapModeOpt::Block)]
    pub map_mode: MapModeOpt,

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

    /// Logs: stream | file | both
    #[arg(long, default_value = "stream")]
    pub logs: String,

    /// Balloon target MiB (default equals --mem)
    #[arg(long)]
    pub balloon: Option<u32>,
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
}

#[derive(Args, Debug)]
pub struct SnapshotCreateArgs {
    /// VM name to snapshot
    pub name: String,

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
    /// Snapshot name to clone from
    pub snapshot_name: String,

    /// Optional: custom name for cloned VM (auto-generated if not provided)
    #[arg(long)]
    pub name: Option<String>,

    #[arg(long, value_enum, default_value_t = ModeOpt::Auto)]
    pub mode: ModeOpt,

    #[arg(long, num_args=0.., value_delimiter=',')]
    pub publish: Vec<String>,

    #[arg(long, default_value = "stream")]
    pub logs: String,
}

// ============================================================================
// Shared Args
// ============================================================================

#[derive(Args, Debug)]
pub struct NameArgs {
    /// VM name or id
    #[arg(long, short)]
    pub name: String,
}

// ============================================================================
// Enums
// ============================================================================

#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum ModeOpt {
    Auto,
    Privileged,
    Rootless,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum MapModeOpt {
    Block,
    Sshfs,
    Nfs,
}
