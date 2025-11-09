use anyhow::Result;
use clap::Parser;
use fcvm::{cli, commands};
use fcvm::cli::Commands;
use tracing::error;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_target(false)
        .init();

    // Parse CLI arguments
    let cli = cli::Cli::parse();

    // Dispatch to appropriate command handler
    let result = match cli.cmd {
        Commands::Podman(args) => commands::cmd_podman(args).await,
        Commands::Snapshot(args) => commands::cmd_snapshot(args).await,
        Commands::Snapshots => commands::cmd_snapshots().await,
        Commands::Logs(args) => commands::cmd_logs(args).await,
        Commands::Inspect(args) => commands::cmd_inspect(args).await,
    };

    // Handle errors
    if let Err(e) = &result {
        error!("Error: {:#}", e);
        std::process::exit(1);
    }

    result
}
