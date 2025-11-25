use anyhow::Result;
use clap::Parser;
use fcvm::cli::Commands;
use fcvm::{cli, commands, paths};
use tracing::error;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = cli::Cli::parse();

    // Initialize base directory from CLI argument (must be done before any path access)
    paths::init_base_dir(cli.base_dir.as_deref());

    // Initialize logging
    // If --sub-process flag is set, disable timestamps AND level (subprocess mode)
    // Parent process already shows timestamp and level, so subprocess just shows the message
    // But KEEP target tags to show the nesting hierarchy!
    // Otherwise, show full formatting (outermost process)
    if cli.sub_process {
        // Subprocesses NEVER have colors (their output is captured and re-logged)
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
            )
            .with_target(true) // KEEP targets to show nesting hierarchy
            .without_time()
            .with_level(false) // Disable level prefix too (INFO, DEBUG, etc.)
            .with_ansi(false) // NEVER use ANSI in subprocesses
            .init();
    } else {
        // Parent process: only use colors when outputting to a TTY (not when piped to file)
        let use_color = atty::is(atty::Stream::Stdout);
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()),
            )
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
        Commands::Test(args) => commands::cmd_test(args).await,
    };

    // Handle errors
    if let Err(e) = &result {
        error!("Error: {:#}", e);
        std::process::exit(1);
    }

    result
}
