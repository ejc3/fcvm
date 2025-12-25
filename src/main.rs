use anyhow::Result;
use clap::Parser;
use fcvm::cli::Commands;
use fcvm::{cli, commands, paths};
use tracing::error;
use tracing_subscriber::EnvFilter;

/// Raise file descriptor limit for high-parallelism workloads.
/// The fuse-pipe server can have many open files when serving parallel tests.
fn raise_resource_limits() {
    use libc::{rlimit, setrlimit, RLIMIT_NOFILE};

    let new_limit = rlimit {
        rlim_cur: 65536,
        rlim_max: 65536,
    };

    let result = unsafe { setrlimit(RLIMIT_NOFILE, &new_limit) };
    if result != 0 {
        eprintln!(
            "[fcvm] warning: failed to raise RLIMIT_NOFILE: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Raise resource limits early for fuse-pipe server
    raise_resource_limits();

    // Parse CLI arguments
    let cli = cli::Cli::parse();

    // Initialize base directory from CLI argument (must be done before any path access)
    paths::init_base_dir(cli.base_dir.as_deref());

    // Initialize logging
    // If --sub-process flag is set, disable timestamps AND level (subprocess mode)
    // Parent process already shows timestamp and level, so subprocess just shows the message
    // But KEEP target tags to show the nesting hierarchy!
    // Otherwise, show full formatting (outermost process)
    // Use RUST_LOG if set, otherwise default to INFO
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if cli.sub_process {
        // Subprocesses NEVER have colors (their output is captured and re-logged)
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr) // Logs to stderr, keep stdout clean for command output
            .with_target(true) // KEEP targets to show nesting hierarchy
            .without_time()
            .with_level(false) // Disable level prefix too (INFO, DEBUG, etc.)
            .with_ansi(false) // NEVER use ANSI in subprocesses
            .init();
    } else {
        // Parent process: only use colors when outputting to a TTY (not when piped to file)
        use std::io::IsTerminal;
        let use_color = std::io::stderr().is_terminal();
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr) // Logs to stderr, keep stdout clean for command output
            .with_target(true) // Show targets for all processes
            .with_ansi(use_color) // Only use ANSI when outputting to TTY
            .init();
    }

    // Dispatch to appropriate command handler
    let result = match cli.cmd {
        Commands::Ls(args) => commands::cmd_ls(args).await,
        Commands::Podman(args) => commands::cmd_podman(args).await,
        Commands::Snapshot(args) => commands::cmd_snapshot(args).await,
        Commands::Snapshots => commands::cmd_snapshots().await,
        Commands::Exec(args) => commands::cmd_exec(args).await,
        Commands::Setup => commands::cmd_setup().await,
    };

    // Handle errors
    if let Err(e) = &result {
        error!("Error: {:#}", e);
        std::process::exit(1);
    }

    result
}
