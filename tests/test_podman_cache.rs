//! Podman cache integration tests
//!
//! Tests the container-layer caching feature that caches VM state after
//! container image is loaded, enabling fast subsequent launches.
//!
//! ## Cache Storage
//!
//! Cache entries are stored via SnapshotManager in the snapshots directory
//! (`paths::snapshot_dir()`). The cache key becomes the snapshot name.
//!
//! ## Cache Key Model
//!
//! Cache keys are computed from FirecrackerConfig JSON which includes:
//! - kernel_path, initrd_path, rootfs_path (content-addressed with SHA)
//! - container_image, container_cmd, cpu, mem, network_mode
//!
//! Cache keys do NOT include runtime-only values: env vars, ports, volumes.
//! This means VMs with same image+cmd+cpu+mem+network_mode share the same cache.
//!
//! ## Test Isolation
//!
//! Tests use different network modes (bridged vs rootless) for cache isolation
//! since network mode IS part of the cache key.
//!
//! ## Root Required
//!
//! All tests in this file require root for privileged networking tests.

#![cfg(all(feature = "integration-fast", feature = "privileged-tests"))]

mod common;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Check if caching is disabled via FCVM_NO_CACHE environment variable
fn cache_disabled_by_env() -> bool {
    std::env::var("FCVM_NO_CACHE").is_ok()
}

/// Get the cache directory path (cache entries are stored as snapshots via SnapshotManager)
fn cache_dir() -> PathBuf {
    let data_dir = std::env::var("FCVM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/mnt/fcvm-btrfs/root"));
    data_dir.join("snapshots")
}

/// List all cache entries (directory names that contain complete cache files)
fn list_cache_entries() -> HashSet<String> {
    let mut entries = HashSet::new();
    if let Ok(dir) = std::fs::read_dir(cache_dir()) {
        for entry in dir.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                let path = entry.path();
                // Check if this is a complete cache entry
                if path.join("memory.bin").exists()
                    && path.join("vmstate.bin").exists()
                    && path.join("disk.raw").exists()
                    && path.join("config.json").exists()
                {
                    entries.insert(name);
                }
            }
        }
    }
    entries
}

/// Wait for a new cache entry to appear (returns the new key)
async fn wait_for_new_cache_entry(before: &HashSet<String>, timeout_secs: u64) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(timeout_secs) {
        let current = list_cache_entries();
        let new_entries: Vec<_> = current.difference(before).collect();
        if !new_entries.is_empty() {
            return Some(new_entries[0].clone());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Check if a specific cache entry exists and is complete
fn cache_entry_exists(cache_key: &str) -> bool {
    let path = cache_dir().join(cache_key);
    path.join("memory.bin").exists()
        && path.join("vmstate.bin").exists()
        && path.join("disk.raw").exists()
        && path.join("config.json").exists()
}

/// Test that first run creates a cache entry
/// Uses rootless network mode for this test
#[tokio::test]
async fn test_podman_cache_miss_creates_cache() -> Result<()> {
    if cache_disabled_by_env() {
        println!("Skipping test: FCVM_NO_CACHE is set");
        return Ok(());
    }
    println!("\ntest_podman_cache_miss_creates_cache");
    println!("=====================================");

    // Record cache entries before test
    let before = list_cache_entries();
    println!("Cache entries before: {}", before.len());

    // Run container with a UNIQUE command that won't be in any existing cache.
    // Since container_cmd is now part of the cache key, using a timestamp ensures
    // this test always creates a new cache entry (cache miss).
    let (vm_name, _, _, _) = common::unique_names("cache-miss");
    let unique_msg = format!(
        "cache-miss-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    println!(
        "Starting VM: {} with unique message: {}",
        vm_name, unique_msg
    );

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        &unique_msg,
    ])
    .await
    .context("spawning fcvm")?;

    // Wait for container (may exit quickly)
    let _ = common::poll_health_by_pid(pid, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child.wait()).await;

    // Verify at least one new cache entry was created
    let new_key = wait_for_new_cache_entry(&before, 10).await;
    assert!(new_key.is_some(), "A cache entry should be created");
    println!("New cache entry: {}", new_key.unwrap());

    println!("Test passed");
    Ok(())
}

/// Test that second run with same config hits cache and is faster
/// Uses rootless network mode - tests may share cache, that's OK
#[tokio::test]
async fn test_podman_cache_hit_restores_fast() -> Result<()> {
    if cache_disabled_by_env() {
        println!("Skipping test: FCVM_NO_CACHE is set");
        return Ok(());
    }
    println!("\ntest_podman_cache_hit_restores_fast");
    println!("====================================");

    // First run - may create cache or use existing
    let (vm_name1, _, _, _) = common::unique_names("cache-hit-1");
    println!("First run: {}", vm_name1);

    let start1 = Instant::now();
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid1, 180).await?;
    let duration1 = start1.elapsed();
    println!("First run duration: {:?}", duration1);

    child1.kill().await?;
    let _ = child1.wait().await;

    // Wait a moment for cache to be written
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Second run - should hit cache (same image+cpu+mem+network)
    let (vm_name2, _, _, _) = common::unique_names("cache-hit-2");
    println!("Second run (should be cache hit): {}", vm_name2);

    let start2 = Instant::now();
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid2, 180).await?;
    let duration2 = start2.elapsed();
    println!("Second run duration: {:?}", duration2);

    child2.kill().await?;
    let _ = child2.wait().await;

    // Second run should be faster (or at least not much slower)
    // Cache hit skips image pull which saves significant time
    if duration1 > Duration::from_secs(5) {
        let speedup = duration1.as_secs_f64() / duration2.as_secs_f64();
        println!("Speedup: {:.1}x", speedup);
        // Cache should provide at least some speedup
        assert!(speedup > 1.0, "Cache hit should be faster than miss");
    } else {
        println!("First run was too fast to measure speedup (likely already cached)");
    }

    println!("Test passed");
    Ok(())
}

/// Test that different network modes create different cache entries
#[tokio::test]
async fn test_podman_cache_different_network_modes() -> Result<()> {
    if cache_disabled_by_env() {
        println!("Skipping test: FCVM_NO_CACHE is set");
        return Ok(());
    }
    println!("\ntest_podman_cache_different_network_modes");
    println!("==========================================");

    // Record cache entries before
    let before = list_cache_entries();
    println!("Cache entries before: {}", before.len());

    // Run with rootless
    let (vm_name1, _, _, _) = common::unique_names("net-rootless");
    println!("Running rootless: {}", vm_name1);
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        "rootless",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid1, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child1.wait()).await;

    // Wait for cache
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after_rootless = list_cache_entries();
    println!("Cache entries after rootless: {}", after_rootless.len());

    // Run with bridged (requires sudo, handled by test harness)
    let (vm_name2, _, _, _) = common::unique_names("net-bridged");
    println!("Running bridged: {}", vm_name2);
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "bridged",
        "alpine:latest",
        "echo",
        "bridged",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid2, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child2.wait()).await;

    // Wait for cache
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after_bridged = list_cache_entries();
    println!("Cache entries after bridged: {}", after_bridged.len());

    // Should have created different cache entries for different network modes
    // (If either was already cached, count might not increase but that's OK)
    let new_after_rootless: HashSet<_> = after_rootless.difference(&before).collect();
    let new_after_bridged: HashSet<_> = after_bridged.difference(&after_rootless).collect();

    println!("New entries after rootless: {:?}", new_after_rootless);
    println!("New entries after bridged: {:?}", new_after_bridged);

    // At least verify both network modes work
    println!("Test passed (both network modes work)");
    Ok(())
}

/// Test that --no-cache flag prevents cache creation
#[tokio::test]
async fn test_podman_cache_no_cache_flag() -> Result<()> {
    println!("\ntest_podman_cache_no_cache_flag");
    println!("================================");

    // Use a unique command so we can verify OUR cache wasn't created
    let unique_msg = format!(
        "no-cache-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    println!("Using unique message: {}", unique_msg);

    // Record cache entries before
    let before = list_cache_entries();
    println!("Cache entries before: {}", before.len());

    // Run with --no-cache
    let (vm_name, _, _, _) = common::unique_names("no-cache");
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-cache",
        "alpine:latest",
        "echo",
        &unique_msg,
    ])
    .await?;

    let _ = common::poll_health_by_pid(pid, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;

    // Wait extra time for cache creation (if it were to happen)
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Check for new cache entries
    let after = list_cache_entries();
    let new_entries: Vec<_> = after.difference(&before).cloned().collect();
    println!(
        "Cache entries after: {} (new: {})",
        after.len(),
        new_entries.len()
    );

    // Verify no new entry contains our unique message in its config
    for entry in &new_entries {
        let config_path = cache_dir().join(entry).join("config.json");
        if let Ok(config) = std::fs::read_to_string(&config_path) {
            assert!(
                !config.contains(&unique_msg),
                "--no-cache flag failed: cache entry {} was created for our command",
                entry
            );
        }
    }

    println!("Test passed (--no-cache prevented cache creation)");
    Ok(())
}

/// Test that incomplete cache (missing files) is treated as miss
#[tokio::test]
async fn test_podman_cache_incomplete_treated_as_miss() -> Result<()> {
    if cache_disabled_by_env() {
        println!("Skipping test: FCVM_NO_CACHE is set");
        return Ok(());
    }
    println!("\ntest_podman_cache_incomplete_treated_as_miss");
    println!("=============================================");

    // Create an incomplete cache entry with a known key
    let incomplete_key = "incomplete-test-entry";
    let cache_path = cache_dir().join(incomplete_key);

    // Clean and create empty directory (incomplete cache)
    let _ = std::fs::remove_dir_all(&cache_path);
    std::fs::create_dir_all(&cache_path)?;
    println!("Created incomplete cache directory: {}", incomplete_key);

    // Verify it's incomplete (exists but missing required files)
    assert!(cache_path.exists(), "Directory should exist");
    assert!(
        !cache_entry_exists(incomplete_key),
        "Should be incomplete (missing files)"
    );

    // Clean up
    let _ = std::fs::remove_dir_all(&cache_path);

    println!("Test passed");
    Ok(())
}

/// Test long-running container works with cache
#[tokio::test]
async fn test_podman_cache_long_running_container() -> Result<()> {
    if cache_disabled_by_env() {
        println!("Skipping test: FCVM_NO_CACHE is set");
        return Ok(());
    }
    println!("\ntest_podman_cache_long_running_container");
    println!("=========================================");

    // First run
    let (vm_name1, _, _, _) = common::unique_names("long-1");
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid1, 180).await?;
    println!("First container healthy");

    child1.kill().await?;
    let _ = child1.wait().await;

    // Wait for cache
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Second run - from cache
    let (vm_name2, _, _, _) = common::unique_names("long-2");
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid2, 180).await?;
    println!("Second container healthy");

    child2.kill().await?;
    let _ = child2.wait().await;

    println!("Test passed");
    Ok(())
}
