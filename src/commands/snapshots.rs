use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::io::{self, Write};
use tracing::info;

use crate::cli::{
    SnapshotTypeFilter, SnapshotsArgs, SnapshotsCommands, SnapshotsDeleteArgs, SnapshotsLsArgs,
    SnapshotsPruneArgs,
};
use crate::paths;
use crate::storage::{SnapshotConfig, SnapshotManager, SnapshotType};

/// Main dispatcher for snapshots management commands
pub async fn cmd_snapshots(args: SnapshotsArgs) -> Result<()> {
    match args.cmd {
        SnapshotsCommands::Ls(ls_args) => cmd_snapshots_ls(ls_args).await,
        SnapshotsCommands::Delete(delete_args) => cmd_snapshots_delete(delete_args).await,
        SnapshotsCommands::Prune(prune_args) => cmd_snapshots_prune(prune_args).await,
    }
}

/// Snapshot info for display/JSON output
#[derive(Debug, Serialize)]
struct SnapshotInfo {
    name: String,
    snapshot_type: String,
    image: String,
    created_at: String,
    age: String,
    size_bytes: u64,
    size_human: String,
}

/// List all snapshots
async fn cmd_snapshots_ls(args: SnapshotsLsArgs) -> Result<()> {
    info!("Listing snapshots");

    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_names = snapshot_manager.list_snapshots().await?;

    if snapshot_names.is_empty() {
        if args.json {
            println!("[]");
        } else {
            println!("No snapshots found.");
        }
        return Ok(());
    }

    // Load all snapshot configs and compute info
    let mut snapshots: Vec<SnapshotInfo> = Vec::new();

    for name in snapshot_names {
        match snapshot_manager.load_snapshot(&name).await {
            Ok(config) => {
                // Filter by type if specified
                if let Some(filter) = args.filter {
                    let matches = match filter {
                        SnapshotTypeFilter::User => config.snapshot_type == SnapshotType::User,
                        SnapshotTypeFilter::System => config.snapshot_type == SnapshotType::System,
                    };
                    if !matches {
                        continue;
                    }
                }

                let info = build_snapshot_info(&name, &config).await;
                snapshots.push(info);
            }
            Err(e) => {
                // Skip snapshots with invalid config but warn
                tracing::warn!("Skipping snapshot '{}': {}", name, e);
            }
        }
    }

    // Sort by creation time (newest first)
    snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    if args.json {
        let json = serde_json::to_string_pretty(&snapshots)?;
        println!("{}", json);
    } else {
        if snapshots.is_empty() {
            println!("No snapshots found matching filter.");
            return Ok(());
        }

        // Print table header
        println!(
            "{:<20} {:<8} {:<6} {:<25} {:<10}",
            "NAME", "TYPE", "AGE", "IMAGE", "SIZE"
        );

        for info in snapshots {
            // Truncate name if too long
            let name_display = if info.name.len() > 18 {
                format!("{}...", &info.name[..15])
            } else {
                info.name.clone()
            };

            // Truncate image if too long
            let image_display = if info.image.len() > 23 {
                format!("{}...", &info.image[..20])
            } else {
                info.image.clone()
            };

            println!(
                "{:<20} {:<8} {:<6} {:<25} {:<10}",
                name_display, info.snapshot_type, info.age, image_display, info.size_human
            );
        }
    }

    Ok(())
}

/// Build snapshot info from config
async fn build_snapshot_info(name: &str, config: &SnapshotConfig) -> SnapshotInfo {
    // Calculate age
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(config.created_at);
    let age = format_duration(duration);

    // Calculate size (sum of memory.bin, disk.raw, vmstate.bin)
    let snapshot_dir = paths::snapshot_dir().join(name);
    let size_bytes = calculate_dir_size(&snapshot_dir).await.unwrap_or(0);
    let size_human = format_size(size_bytes);

    SnapshotInfo {
        name: name.to_string(),
        snapshot_type: config.snapshot_type.to_string(),
        image: config.metadata.image.clone(),
        created_at: config.created_at.to_rfc3339(),
        age,
        size_bytes,
        size_human,
    }
}

/// Format duration as human-readable age
fn format_duration(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds();
    if seconds < 60 {
        format!("{}s", seconds)
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else if seconds < 604800 {
        format!("{}d", seconds / 86400)
    } else {
        format!("{}w", seconds / 604800)
    }
}

/// Format size as human-readable
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}K", bytes as f64 / KB as f64)
    } else {
        format!("{}B", bytes)
    }
}

/// Calculate total size of directory
async fn calculate_dir_size(dir: &std::path::Path) -> Result<u64> {
    let mut total = 0u64;

    if !dir.exists() {
        return Ok(0);
    }

    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let metadata = entry.metadata().await?;
        if metadata.is_file() {
            total += metadata.len();
        }
    }

    Ok(total)
}

/// Delete a specific snapshot
async fn cmd_snapshots_delete(args: SnapshotsDeleteArgs) -> Result<()> {
    info!("Deleting snapshot: {}", args.name);

    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());

    // Check if snapshot exists
    let config = snapshot_manager
        .load_snapshot(&args.name)
        .await
        .context("snapshot not found")?;

    // Confirm deletion unless --force
    if !args.force {
        print!(
            "Delete snapshot '{}' ({}, {})? [y/N] ",
            args.name, config.snapshot_type, config.metadata.image
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    snapshot_manager.delete_snapshot(&args.name).await?;
    println!("Deleted snapshot '{}'", args.name);

    Ok(())
}

/// Delete all system (auto-generated) snapshots
async fn cmd_snapshots_prune(args: SnapshotsPruneArgs) -> Result<()> {
    info!("Pruning system snapshots");

    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_names = snapshot_manager.list_snapshots().await?;

    // Find all system snapshots
    let mut system_snapshots: Vec<(String, SnapshotConfig)> = Vec::new();

    for name in snapshot_names {
        if let Ok(config) = snapshot_manager.load_snapshot(&name).await {
            if config.snapshot_type == SnapshotType::System {
                system_snapshots.push((name, config));
            }
        }
    }

    if system_snapshots.is_empty() {
        println!("No system snapshots to prune.");
        return Ok(());
    }

    // Calculate total size
    let mut total_size = 0u64;
    for (name, _) in &system_snapshots {
        let dir = paths::snapshot_dir().join(name);
        total_size += calculate_dir_size(&dir).await.unwrap_or(0);
    }

    // Confirm deletion unless --force
    if !args.force {
        println!(
            "Found {} system snapshot(s) totaling {}:",
            system_snapshots.len(),
            format_size(total_size)
        );
        for (name, config) in &system_snapshots {
            println!("  {} ({})", name, config.metadata.image);
        }
        print!("\nDelete all? [y/N] ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Delete all system snapshots
    let mut deleted = 0;
    let mut failed = 0;

    for (name, _) in system_snapshots {
        match snapshot_manager.delete_snapshot(&name).await {
            Ok(()) => {
                deleted += 1;
                info!("Deleted {}", name);
            }
            Err(e) => {
                failed += 1;
                tracing::warn!("Failed to delete {}: {}", name, e);
            }
        }
    }

    println!(
        "Pruned {} system snapshot(s), freed ~{}",
        deleted,
        format_size(total_size)
    );

    if failed > 0 {
        bail!("Failed to delete {} snapshot(s)", failed);
    }

    Ok(())
}
