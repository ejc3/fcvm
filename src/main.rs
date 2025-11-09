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
        Commands::Run(args) => commands::cmd_run(args).await,
        Commands::Clone(args) => commands::cmd_clone(args).await,
        Commands::Stop(args) => commands::cmd_stop(args).await,
        Commands::Ls => commands::cmd_ls().await,
        Commands::Inspect(args) => commands::cmd_inspect(args).await,
        Commands::Logs(args) => commands::cmd_logs(args).await,
        Commands::Top => commands::cmd_top().await,
        Commands::Setup(args) => commands::cmd_setup(args).await,
    };

    // Handle errors
    if let Err(e) = &result {
        error!("Error: {:#}", e);
        std::process::exit(1);
    }

    result
}
