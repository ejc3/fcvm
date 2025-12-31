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
    Podman(PodmanArgs),
    /// Snapshot operations (create, serve, run)
    Snapshot(SnapshotArgs),
    /// List available snapshots
    Snapshots,
    /// Execute a command in a running VM
    Exec(ExecArgs),
    /// Setup kernel and rootfs (kernel ~15MB download, rootfs ~10GB creation, takes 5-10 minutes)
    Setup(SetupArgs),
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

    /// Also setup inception kernel for nested virtualization
    #[arg(long)]
    pub inception: bool,

    /// Build kernels locally instead of downloading from releases
    /// (use if download fails or you've modified kernel sources)
    #[arg(long)]
    pub build_kernels: bool,
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

    /// Volume mapping(s): HOST:GUEST[:ro] (repeat or comma-separated)
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub map: Vec<String>,

    /// Environment vars KEY=VALUE (repeat or comma-separated)
    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub env: Vec<String>,

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

    /// Debug fc-agent with strace (output to /tmp/fc-agent.strace in guest)
    /// Useful for diagnosing fc-agent startup issues
    #[arg(long)]
    pub strace_agent: bool,

    /// Run setup if kernel/rootfs are missing (takes 5-10 minutes on first run)
    /// Without this flag, fcvm will fail if setup hasn't been run
    #[arg(long)]
    pub setup: bool,

    /// Custom kernel path (overrides default kernel from setup)
    /// Use for inception support with a KVM-enabled kernel
    #[arg(long)]
    pub kernel: Option<String>,
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

    #[arg(long, action = clap::ArgAction::Append, value_delimiter=',')]
    pub publish: Vec<String>,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,

    /// Execute command in container after clone is healthy (like fcvm exec -c)
    #[arg(long)]
    pub exec: Option<String>,
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
    /// VM name to exec into (mutually exclusive with --pid)
    #[arg(conflicts_with = "pid")]
    pub name: Option<String>,

    /// VM PID to exec into (mutually exclusive with name)
    #[arg(long, conflicts_with = "name")]
    pub pid: Option<u32>,

    /// Execute in the VM instead of inside the container
    #[arg(long)]
    pub vm: bool,

    /// Keep STDIN open even if not attached
    #[arg(short, long)]
    pub interactive: bool,

    /// Allocate a pseudo-TTY
    #[arg(short, long)]
    pub tty: bool,

    /// Suppress log output (auto-enabled with -t)
    #[arg(short, long)]
    pub quiet: bool,

    /// Command and arguments to execute
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}
