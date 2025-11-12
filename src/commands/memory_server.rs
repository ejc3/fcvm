use anyhow::{Context, Result};
use tracing::info;

use crate::cli::MemoryServerArgs;
use crate::paths;
use crate::storage::SnapshotManager;
use crate::uffd::UffdServer;


pub async fn cmd_memory_server(args: MemoryServerArgs) -> Result<()> {
    info!("Starting memory server for snapshot: {}", args.snapshot_name);

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_config = snapshot_manager.load_snapshot(&args.snapshot_name).await
        .context("loading snapshot configuration")?;

    info!(
        snapshot = %args.snapshot_name,
        mem_file = %snapshot_config.memory_path.display(),
        mem_size_mb = snapshot_config.metadata.memory_mib,
        "loaded snapshot configuration"
    );

    // Create and start UFFD server
    let server = UffdServer::new(
        args.snapshot_name.clone(),
        &snapshot_config.memory_path,
    ).await
        .context("creating UFFD server")?;

    println!("Memory Server: {}", args.snapshot_name);
    println!("  Socket: {}", server.socket_path().display());
    println!("  Memory: {} MB", snapshot_config.metadata.memory_mib);
    println!("  Waiting for VMs to connect...");
    println!();
    println!("Clone VMs with: fcvm clone --snapshot {}", args.snapshot_name);
    println!("Press Ctrl-C to stop");
    println!();

    // Run server (blocks until all VMs disconnect or Ctrl-C)
    server.run().await
        .context("running UFFD server")?;

    println!("Memory server stopped");

    Ok(())
}
