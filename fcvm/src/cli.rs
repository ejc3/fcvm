use clap::{Parser, Subcommand, Args, ValueEnum};
use crate::lib::{Mode, MapMode};

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

#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum ModeOpt { Auto, Privileged, Rootless }

#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum MapModeOpt { Block, Sshfs, Nfs }

impl From<ModeOpt> for Mode {
    fn from(m: ModeOpt) -> Self { match m { ModeOpt::Auto => Mode::Auto, ModeOpt::Privileged => Mode::Privileged, ModeOpt::Rootless => Mode::Rootless } }
}
impl From<MapModeOpt> for MapMode {
    fn from(m: MapModeOpt) -> Self { match m { MapModeOpt::Block => MapMode::Block, MapModeOpt::Sshfs => MapMode::Sshfs, MapModeOpt::Nfs => MapMode::Nfs } }
}
