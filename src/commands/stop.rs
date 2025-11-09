use anyhow::Result;
use tracing::info;

use crate::cli::NameArgs;

pub async fn cmd_stop(args: NameArgs) -> Result<()> {
    info!("fcvm stop - not yet implemented");
    println!("Stop VM: {}", args.name);
    Ok(())
}
