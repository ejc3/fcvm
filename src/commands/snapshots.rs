use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::info;

use crate::paths;

/// List available snapshots
pub async fn cmd_snapshots() -> Result<()> {
    info!("Listing snapshots");

    let snapshots_dir = paths::snapshot_dir();

    if !snapshots_dir.exists() {
        println!("No snapshots found.");
        return Ok(());
    }

    let mut entries = tokio::fs::read_dir(&snapshots_dir)
        .await
        .context("reading snapshots directory")?;

    let mut snapshots = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                snapshots.push(name.to_string());
            }
        }
    }

    if snapshots.is_empty() {
        println!("No snapshots found.");
        return Ok(());
    }

    snapshots.sort();

    println!("Available snapshots:");
    for snapshot in snapshots {
        println!("  {}", snapshot);
    }

    Ok(())
}
