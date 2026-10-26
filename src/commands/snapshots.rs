use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use tokio::process::Command;
use tracing::info;

use crate::cli::{
    SnapshotTypeFilter, SnapshotsArgs, SnapshotsCommands, SnapshotsDeleteArgs, SnapshotsLsArgs,
    SnapshotsPruneArgs,
};
use crate::paths;
use crate::storage::{SnapshotConfig, SnapshotGeneration, SnapshotManager, SnapshotType};

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
    memory_size_bytes: u64,
    /// The directories the snapshot maps into the guest. `snapshot run --map` serves a
    /// read-only, portable one from another host directory.
    maps: Vec<SnapshotMapInfo>,
}

/// A directory a snapshot maps into the guest, as `snapshot run --map` names it.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct SnapshotMapInfo {
    guest_path: String,
    read_only: bool,
    portable: bool,
}

fn snapshot_maps(config: &SnapshotConfig) -> Vec<SnapshotMapInfo> {
    config
        .metadata
        .volumes
        .iter()
        .map(|v| SnapshotMapInfo {
            guest_path: v.guest_path.clone(),
            read_only: v.read_only,
            portable: v.portable,
        })
        .collect()
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

                // Filter by image if specified
                if let Some(ref image) = args.image {
                    if config.metadata.image != *image {
                        continue;
                    }
                }

                let info = build_snapshot_info(&name, &config, args.shared).await;
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
async fn build_snapshot_info(
    name: &str,
    config: &SnapshotConfig,
    use_btrfs_shared: bool,
) -> SnapshotInfo {
    // Calculate age
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(config.created_at);
    let age = format_duration(duration);

    // Calculate size (sum of memory.bin, disk.raw, vmstate.bin)
    let snapshot_dir = paths::snapshot_dir().join(name);
    let (size_bytes, size_human) = if use_btrfs_shared {
        // Use btrfs fi du to get accurate exclusive (non-shared) size
        match calculate_btrfs_exclusive_size(&snapshot_dir).await {
            Ok(exclusive) => (exclusive, format_size_exclusive(exclusive)),
            Err(_) => {
                // Fall back to st_blocks if btrfs fi du fails
                let size = calculate_dir_size(&snapshot_dir).await.unwrap_or(0);
                (size, format_size(size))
            }
        }
    } else {
        let size = calculate_dir_size(&snapshot_dir).await.unwrap_or(0);
        (size, format_size(size))
    };

    // Memory file size (memory.bin)
    let memory_size_bytes = tokio::fs::metadata(&config.memory_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    SnapshotInfo {
        name: name.to_string(),
        snapshot_type: config.snapshot_type.to_string(),
        image: config.metadata.image.clone(),
        created_at: config.created_at.to_rfc3339(),
        age,
        size_bytes,
        size_human,
        memory_size_bytes,
        maps: snapshot_maps(config),
    }
}

/// Format duration as human-readable age
fn format_duration(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds();

    // Handle negative durations (clock skew, future timestamps)
    if seconds < 0 {
        return "0s".to_string();
    }

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

/// Format size as human-readable with (shared) indicator for btrfs reflinks
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    let size = if bytes >= GB {
        format!("{:.1}G", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}K", bytes as f64 / KB as f64)
    } else {
        format!("{}B", bytes)
    };

    // Indicate size may be shared via btrfs reflinks
    format!("{} (shared)", size)
}

/// Format size for exclusive (non-shared) bytes
fn format_size_exclusive(bytes: u64) -> String {
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

/// Calculate exclusive (non-shared) disk usage using btrfs fi du
async fn calculate_btrfs_exclusive_size(dir: &std::path::Path) -> Result<u64> {
    let output = Command::new("btrfs")
        .args(["filesystem", "du", "-s", "--raw"])
        .arg(dir)
        .output()
        .await
        .context("running btrfs fi du")?;

    if !output.status.success() {
        bail!("btrfs fi du failed");
    }

    // Parse output: "Total   Exclusive  Set shared  Filename"
    // Second line: "12345   6789       ..."
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let Ok(exclusive) = parts[1].parse::<u64>() {
                return Ok(exclusive);
            }
        }
    }

    bail!("failed to parse btrfs fi du output")
}

/// Calculate actual disk usage of directory (recursively)
/// Uses st_blocks to get real disk usage, accounting for btrfs CoW/reflinks
fn calculate_dir_size(
    dir: &std::path::Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64>> + Send + '_>> {
    Box::pin(async move {
        let mut total = 0u64;

        if !dir.exists() {
            return Ok(0);
        }

        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            if metadata.is_file() {
                // Use blocks * 512 for actual disk usage (like du)
                // This accounts for sparse files and btrfs reflinks
                total += metadata.blocks() * 512;
            } else if metadata.is_dir() {
                // Recurse into subdirectories
                total += calculate_dir_size(&entry.path()).await.unwrap_or(0);
            }
        }

        Ok(total)
    })
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

/// Whether `snapshots prune` reclaims this snapshot.
///
/// Without `--all` it reclaims only the content-addressed cache, which is what `System`
/// means. Anything a caller named — `snapshot create --tag`, `podman prepare --tag` — is a
/// `User` snapshot and survives, because a golden snapshot that took ten minutes to build
/// must not disappear on the next prune.
pub(crate) fn prune_reclaims(
    config: &SnapshotConfig,
    prune_all: bool,
    image: Option<&str>,
) -> bool {
    if !prune_all && config.snapshot_type != SnapshotType::System {
        return false;
    }
    match image {
        Some(image) => config.metadata.image == image,
        None => true,
    }
}

/// A snapshot prune decided to delete, with the generation that decision read.
struct PruneCandidate {
    name: String,
    config: SnapshotConfig,
    generation: SnapshotGeneration,
}

/// The snapshots under `manager` that `prune_reclaims` selects.
///
/// The snapshot directory also holds a create's staging directories: `<key>.creating`,
/// which holds the new generation's `config.json` until it is renamed to `<key>`, and
/// `<key>.old`, the generation a replacement moved aside. Neither is a snapshot, and the
/// creator holds `<key>.lock`, not their own lock, so a delete of one runs under the
/// creator. A staging directory's config names the snapshot it belongs to, not the
/// directory, so a directory is a snapshot only when its config names it.
async fn prune_candidates(
    manager: &SnapshotManager,
    prune_all: bool,
    image: Option<&str>,
) -> Result<Vec<PruneCandidate>> {
    let mut candidates = Vec::new();
    for name in manager.list_snapshots().await? {
        if let Ok((config, generation)) = manager.load_snapshot_with_generation(&name).await {
            if config.name != name {
                continue;
            }
            if prune_reclaims(&config, prune_all, image) {
                candidates.push(PruneCandidate {
                    name,
                    config,
                    generation,
                });
            }
        }
    }
    Ok(candidates)
}

struct PruneOutcome {
    deleted: usize,
    /// Candidates a create replaced after prune read them, kept.
    retained: usize,
    failed: usize,
}

/// Delete each candidate if it is still the generation prune decided on. The decision
/// was an unlocked read; the delete takes the generation lock and compares, so a
/// generation installed in between is kept rather than deleted in its predecessor's
/// place.
async fn prune_delete(manager: &SnapshotManager, candidates: Vec<PruneCandidate>) -> PruneOutcome {
    let mut outcome = PruneOutcome {
        deleted: 0,
        retained: 0,
        failed: 0,
    };
    for candidate in candidates {
        let name = candidate.name;
        match manager
            .delete_snapshot_if_generation(&name, &candidate.generation)
            .await
        {
            Ok(true) => {
                outcome.deleted += 1;
                info!("Deleted {}", name);
            }
            Ok(false) => {
                outcome.retained += 1;
                info!("Kept {}: it changed after prune selected it", name);
            }
            Err(e) => {
                outcome.failed += 1;
                tracing::warn!("Failed to delete {}: {}", name, e);
            }
        }
    }
    outcome
}

/// Delete snapshots (system-only by default, or all with --all)
async fn cmd_snapshots_prune(args: SnapshotsPruneArgs) -> Result<()> {
    let prune_all = args.all;
    let type_desc = if prune_all { "all" } else { "system" };

    info!("Pruning {} snapshots", type_desc);

    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshots_to_delete =
        prune_candidates(&snapshot_manager, prune_all, args.image.as_deref()).await?;

    if snapshots_to_delete.is_empty() {
        println!("No {} snapshots to prune.", type_desc);
        return Ok(());
    }

    // Calculate total size
    let mut total_size = 0u64;
    for candidate in &snapshots_to_delete {
        let dir = paths::snapshot_dir().join(&candidate.name);
        total_size += calculate_dir_size(&dir).await.unwrap_or(0);
    }

    // Confirm deletion unless --force
    if !args.force {
        println!(
            "Found {} {} snapshot(s) totaling {}:",
            snapshots_to_delete.len(),
            type_desc,
            format_size(total_size)
        );
        for candidate in &snapshots_to_delete {
            println!(
                "  {} ({}, {})",
                candidate.name, candidate.config.snapshot_type, candidate.config.metadata.image
            );
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

    let PruneOutcome {
        deleted,
        retained,
        failed,
    } = prune_delete(&snapshot_manager, snapshots_to_delete).await;

    println!(
        "Pruned {} {} snapshot(s), freed ~{}",
        deleted,
        type_desc,
        format_size(total_size)
    );

    if retained > 0 {
        println!(
            "Kept {} snapshot(s) replaced after they were selected",
            retained
        );
    }

    if failed > 0 {
        bail!("Failed to delete {} snapshot(s)", failed);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(snapshot_type: SnapshotType, image: &str) -> SnapshotConfig {
        let mut vm_state =
            crate::state::VmState::new("vm-prune".to_string(), image.to_string(), 1, 512);
        vm_state.config.image = image.to_string();
        vm_state.config.source_vsock_socket_path =
            Some(std::path::PathBuf::from("/run/test-vsock/vsock.sock"));
        crate::commands::common::build_snapshot_config(
            &vm_state,
            "snap",
            snapshot_type,
            std::path::Path::new("/snapshots/snap"),
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap()
    }

    /// `snapshots ls --json` names each directory a snapshot maps and whether it is
    /// read-only, so a caller can tell which `snapshot run --map` the snapshot takes
    /// without reading its config.
    #[test]
    fn the_listing_names_the_directories_a_snapshot_maps() {
        let mut snapshot = config(SnapshotType::User, "nginx:alpine");
        snapshot.metadata.volumes = vec![
            crate::storage::SnapshotVolumeConfig {
                host_path: "/host/creds".into(),
                guest_path: "/var/creds".to_string(),
                read_only: true,
                vsock_port: 5000,
                portable: true,
            },
            crate::storage::SnapshotVolumeConfig {
                host_path: "/host/data".into(),
                guest_path: "/data".to_string(),
                read_only: false,
                vsock_port: 5001,
                portable: true,
            },
        ];
        let listed = serde_json::to_value(snapshot_maps(&snapshot)).unwrap();
        assert_eq!(
            listed,
            serde_json::json!([
                {"guest_path": "/var/creds", "read_only": true, "portable": true},
                {"guest_path": "/data", "read_only": false, "portable": true},
            ])
        );
    }

    #[test]
    fn default_prune_reclaims_the_cache_and_keeps_named_snapshots() {
        let cache = config(SnapshotType::System, "nginx:alpine");
        let named = config(SnapshotType::User, "nginx:alpine");

        assert!(prune_reclaims(&cache, false, None));
        assert!(!prune_reclaims(&named, false, None));
        assert!(prune_reclaims(&named, true, None));
    }

    /// Writes `config` as `<root>/<dir>/config.json`, the way a create writes
    /// it into its staging directory before the rename.
    fn write_config(root: &std::path::Path, dir: &str, config: &SnapshotConfig) {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            serde_json::to_vec_pretty(config).unwrap(),
        )
        .unwrap();
    }

    fn cache_snapshot(name: &str, vm_id: &str) -> SnapshotConfig {
        let mut snapshot = config(SnapshotType::System, "nginx:alpine");
        snapshot.name = name.to_string();
        snapshot.vm_id = vm_id.to_string();
        snapshot.generation_id = uuid::Uuid::new_v4();
        snapshot
    }

    /// A create writes `config.json` into `<key>.creating` before it renames
    /// the directory to `<key>`, and a replacement moves the previous
    /// generation aside to `<key>.old`. Neither is a snapshot: the creator
    /// holds `<key>.lock`, not `<key>.creating.lock`, so a prune that took the
    /// staging directory for a cache entry removed it under the creator, and
    /// the create failed at its rename.
    ///
    /// RED BEFORE THE FIX (issue 1180): both staging directories were selected
    /// as System snapshots and deleted.
    #[tokio::test]
    async fn prune_never_deletes_a_snapshot_that_is_being_created_or_replaced() {
        let root = tempfile::tempdir().unwrap();
        let manager = SnapshotManager::new(root.path().to_path_buf());
        write_config(
            root.path(),
            "key.creating",
            &cache_snapshot("key", "vm-new"),
        );
        write_config(root.path(), "key.old", &cache_snapshot("key", "vm-old"));
        // A published snapshot whose own tag ends in a staging suffix is
        // still pruned: the name its config records is the directory's.
        write_config(root.path(), "tag.old", &cache_snapshot("tag.old", "vm-tag"));

        let candidates = prune_candidates(&manager, false, None).await.unwrap();
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["tag.old"]);
        let outcome = prune_delete(&manager, candidates).await;
        assert_eq!((outcome.deleted, outcome.failed), (1, 0));
        for staging in ["key.creating", "key.old"] {
            assert!(
                root.path().join(staging).join("config.json").exists(),
                "prune removed the staging directory {staging}"
            );
        }
        assert!(!root.path().join("tag.old").exists());
    }

    /// Prune decides from an unlocked read and deletes later, under the
    /// generation lock. A create that installs a new generation of the same key
    /// in between holds that lock until its rename, so the delete runs after
    /// it and must find the generation it decided on, not the replacement.
    ///
    /// RED BEFORE THE FIX (issue 1180): the delete removed whatever generation
    /// was installed when it ran.
    #[tokio::test]
    async fn prune_retains_a_generation_installed_after_its_decision() {
        let root = tempfile::tempdir().unwrap();
        let manager = SnapshotManager::new(root.path().to_path_buf());
        manager
            .save_snapshot(cache_snapshot("key", "vm-decided"))
            .await
            .unwrap();
        let candidates = prune_candidates(&manager, false, None).await.unwrap();
        assert_eq!(candidates.len(), 1);

        manager
            .save_snapshot(cache_snapshot("key", "vm-replacement"))
            .await
            .unwrap();
        let outcome = prune_delete(&manager, candidates).await;
        assert_eq!((outcome.deleted, outcome.failed), (0, 0));
        let retained = manager.load_snapshot("key").await.unwrap();
        assert_eq!(retained.vm_id, "vm-replacement");
    }

    #[test]
    fn the_image_filter_applies_to_both_types() {
        let cache = config(SnapshotType::System, "nginx:alpine");

        assert!(prune_reclaims(&cache, false, Some("nginx:alpine")));
        assert!(!prune_reclaims(&cache, false, Some("redis:7")));
        // --all widens the type, not the image.
        assert!(!prune_reclaims(
            &config(SnapshotType::User, "nginx:alpine"),
            true,
            Some("redis:7")
        ));
    }
}
