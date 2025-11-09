use clap::{Parser, Subcommand, Args, ValueEnum};

#[derive(Parser, Debug)]
#[command(name="fcvm", version, about="Firecracker VM runner for Podman containers")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Run(RunArgs),
    Clone(CloneArgs),
    Stop(NameArgs),
    Ls,
    Inspect(NameArgs),
    Logs(NameArgs),
    Top,
    Setup(SetupArgs),
    /// Start memory server for a snapshot (enables memory sharing across clones)
    MemoryServer(MemoryServerArgs),
}

#[derive(Args, Debug)]
pub struct SetupArgs {
    #[command(subcommand)]
    pub cmd: SetupCommands,
}

#[derive(Subcommand, Debug)]
pub enum SetupCommands {
    /// Download or extract kernel for Firecracker
    Kernel {
        /// Output path for kernel
        #[arg(long, default_value = "~/.local/share/fcvm/images/vmlinux")]
        output: String,

        /// Download pre-built kernel instead of extracting from host
        #[arg(long)]
        download: bool,
    },

    /// Create base rootfs image with Podman and fc-agent
    Rootfs {
        /// Output directory for rootfs
        #[arg(long, default_value = "~/.local/share/fcvm/images/rootfs")]
        output: String,

        /// Debian suite to use
        #[arg(long, default_value = "bookworm")]
        suite: String,

        /// Size in MB
        #[arg(long, default_value_t = 4096)]
        size_mb: u32,
    },

    /// Check system requirements and show status
    Preflight,
}

#[derive(Args, Debug)]
pub struct NameArgs {
    /// VM name or id
    #[arg(long, short)]
    pub name: String,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Container image (e.g., ghcr.io/org/app:tag)
    pub image: String,

    /// VM name
    #[arg(long)]
    pub name: Option<String>,

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

    /// Publish host ports to guest (rootless hostfwd or privileged DNAT)
    /// Grammar: [HOSTIP:]HOSTPORT:GUESTPORT[/PROTO], comma-separated
    #[arg(long, num_args=0.., value_delimiter=',')]
    pub publish: Vec<String>,

    /// Save a warm snapshot with this name when readiness gate passes
    #[arg(long)]
    pub save_snapshot: Option<String>,

    /// Readiness gate (e.g., 'mode=vsock' or 'mode=http url=http://127.0.0.1:10080/health')
    #[arg(long)]
    pub wait_ready: Option<String>,

    /// Logs: stream | file | both
    #[arg(long, default_value="stream")]
    pub logs: String,

    /// Balloon target MiB (default equals --mem)
    #[arg(long)]
    pub balloon: Option<u32>,
}

#[derive(Args, Debug)]
pub struct CloneArgs {
    #[arg(long)]
    pub name: String,

    #[arg(long)]
    pub snapshot: String,

    #[arg(long, value_enum, default_value_t = ModeOpt::Auto)]
    pub mode: ModeOpt,

    #[arg(long, num_args=0.., value_delimiter=',')]
    pub publish: Vec<String>,

    #[arg(long, default_value="stream")]
    pub logs: String,
}

#[derive(Args, Debug)]
pub struct MemoryServerArgs {
    /// Snapshot name to serve memory for
    pub snapshot_name: String,

    /// Optional: Auto-shutdown after N minutes of inactivity
    #[arg(long)]
    pub timeout: Option<u64>,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum ModeOpt { Auto, Privileged, Rootless }

#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum MapModeOpt { Block, Sshfs, Nfs }
