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
    /// Test operations (stress test, benchmarks)
    Test(TestArgs),
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

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,
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

    #[arg(long, default_value = "stream")]
    pub logs: String,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,
}

// ============================================================================
// Shared Args
// ============================================================================
// Enums
// ============================================================================

#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
pub enum MapModeOpt {
    Block,
    Sshfs,
    Nfs,
}

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
// Test Commands
// ============================================================================

#[derive(Args, Debug)]
pub struct TestArgs {
    #[command(subcommand)]
    pub cmd: TestCommands,
}

#[derive(Subcommand, Debug)]
pub enum TestCommands {
    /// Stress test snapshot/clone performance
    Stress(StressTestArgs),

    /// Sanity test: start a single VM and verify health check passes
    Sanity(SanityTestArgs),

    /// Volume test: verify host directory mounting via FUSE over vsock
    Volume(VolumeTestArgs),

    /// Volume stress test: heavy I/O testing on FUSE volumes
    VolumeStress(VolumeStressTestArgs),

    /// Clone lock test: verify POSIX file locking across multiple clones
    CloneLock(CloneLockTestArgs),

    /// Run pjdfstest POSIX filesystem compliance tests against a FUSE volume
    Pjdfstest(PjdfstestArgs),
}

#[derive(Args, Debug)]
pub struct StressTestArgs {
    /// Snapshot name to test
    #[arg(long, default_value = "final")]
    pub snapshot: String,

    /// Number of VMs to clone
    #[arg(long, default_value_t = 10)]
    pub num_clones: usize,

    /// Number of concurrent clones per batch
    #[arg(long, default_value_t = 5)]
    pub batch_size: usize,

    /// Timeout for health checks in seconds
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,

    /// Clean up before starting (kills all firecracker processes)
    #[arg(long)]
    pub clean: bool,

    /// Name for baseline VM
    #[arg(long, default_value = "baseline-vm")]
    pub baseline_name: String,

    /// Verbose output
    #[arg(short, long)]
    pub verbose: bool,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,
}

#[derive(Args, Debug)]
pub struct SanityTestArgs {
    /// Image to use for the VM
    #[arg(long, default_value = "nginx:alpine")]
    pub image: String,

    /// Timeout for health check in seconds
    #[arg(long, default_value_t = 60)]
    pub timeout: u64,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,
}

#[derive(Args, Debug)]
pub struct VolumeTestArgs {
    /// Number of volumes to test (1-4)
    #[arg(long, default_value_t = 1)]
    pub num_volumes: usize,

    /// Timeout for test in seconds
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,
}

#[derive(Args, Debug)]
pub struct VolumeStressTestArgs {
    /// Number of volumes to test (1-4)
    #[arg(long, default_value_t = 2)]
    pub num_volumes: usize,

    /// Size of test files in MB
    #[arg(long, default_value_t = 10)]
    pub file_size_mb: usize,

    /// Number of concurrent read/write operations
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    /// Number of I/O iterations
    #[arg(long, default_value_t = 10)]
    pub iterations: usize,

    /// Timeout for test in seconds
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,
}

#[derive(Args, Debug)]
pub struct CloneLockTestArgs {
    /// Number of clones to spawn for locking test
    #[arg(long, default_value_t = 10)]
    pub num_clones: usize,

    /// Number of lock iterations per clone
    #[arg(long, default_value_t = 100)]
    pub iterations: usize,

    /// Timeout for test in seconds
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,
}

#[derive(Args, Debug)]
pub struct PjdfstestArgs {
    /// Timeout for test in seconds
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,

    /// Network mode: bridged (requires sudo) or rootless (no sudo)
    #[arg(long, value_enum, default_value_t = NetworkMode::Bridged)]
    pub network: NetworkMode,

    /// Test pattern filter (e.g., "chmod", "chown", "link")
    #[arg(long)]
    pub filter: Option<String>,

    /// Verbose output from pjdfstest
    #[arg(short, long)]
    pub verbose: bool,
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
