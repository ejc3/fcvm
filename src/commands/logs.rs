use anyhow::Result;
use tracing::info;

use crate::cli::NameArgs;

pub async fn cmd_logs(args: NameArgs) -> Result<()> {
    info!("fcvm logs - not yet implemented");
    println!("Logs for VM: {}", args.name);
    Ok(())
}
