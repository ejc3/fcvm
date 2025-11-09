use anyhow::Result;
use tracing::info;

use crate::cli::NameArgs;

pub async fn cmd_inspect(args: NameArgs) -> Result<()> {
    info!("fcvm inspect - not yet implemented");
    println!("Inspect VM: {}", args.name);
    Ok(())
}
