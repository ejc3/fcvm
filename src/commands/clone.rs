use anyhow::Result;
use tracing::info;

use crate::cli::CloneArgs;

pub async fn cmd_clone(args: CloneArgs) -> Result<()> {
    info!("fcvm clone - not yet implemented");
    println!("Clone from snapshot: {} (snapshot: {})", args.name, args.snapshot);
    Ok(())
}
