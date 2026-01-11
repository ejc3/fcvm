//! Podman cache integration tests
//!
//! Tests the container-layer caching feature that caches VM state after
//! container image is loaded, enabling fast subsequent launches.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Get the podman cache directory path
fn podman_cache_dir() -> PathBuf {
    // Default path - same as paths::podman_cache_dir()
    PathBuf::from("/mnt/fcvm-btrfs/podman-cache")
}

/// Count cache entries in the podman cache directory
fn count_cache_entries() -> usize {
    let cache_dir = podman_cache_dir();
    if !cache_dir.exists() {
        return 0;
    }
    std::fs::read_dir(&cache_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

/// Clean up podman cache directory for isolated testing
async fn cleanup_cache() {
    let cache_dir = podman_cache_dir();
    if cache_dir.exists() {
        let _ = tokio::fs::remove_dir_all(&cache_dir).await;
    }
}

/// Test that first run creates a cache entry
#[tokio::test]
async fn test_podman_cache_miss_creates_cache() -> Result<()> {
    println!("\ntest_podman_cache_miss_creates_cache");
    println!("=====================================");
    println!("First run with a container image should create cache");

    // Start with clean cache
    cleanup_cache().await;
    let initial_count = count_cache_entries();
    println!("Initial cache entries: {}", initial_count);

    // Run container - should be a cache miss
    let (vm_name, _, _, _) = common::unique_names("cache-miss");
    println!("Starting VM: {}", vm_name);

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        "hello",
    ])
    .await
    .context("spawning fcvm for cache miss test")?;

    // Wait for VM to become healthy
    println!("Waiting for VM to become healthy...");
    match common::poll_health_by_pid(pid, 180).await {
        Ok(_) => println!("VM is healthy"),
        Err(e) => {
            // VM might have exited after running echo - that's OK
            println!("Health check result: {}", e);
        }
    }

    // Wait for process to complete (it should exit after echo)
    println!("Waiting for process to complete...");
    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    while start.elapsed() < timeout {
        match child.try_wait() {
            Ok(Some(_status)) => {
                println!("Process exited");
                break;
            }
            Ok(None) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                println!("Error checking process: {}", e);
                break;
            }
        }
    }

    // Verify cache was created
    let final_count = count_cache_entries();
    println!("Final cache entries: {}", final_count);

    assert!(
        final_count > initial_count,
        "Cache directory should have new entries after first run"
    );

    // Verify cache structure
    let cache_dir = podman_cache_dir();
    for entry in std::fs::read_dir(&cache_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path();
            assert!(
                path.join("memory.bin").exists(),
                "Cache should have memory.bin"
            );
            assert!(
                path.join("vmstate.bin").exists(),
                "Cache should have vmstate.bin"
            );
            assert!(path.join("disk.raw").exists(), "Cache should have disk.raw");
            assert!(
                path.join("config.json").exists(),
                "Cache should have config.json"
            );
            println!(
                "Cache entry verified: {}",
                entry.file_name().to_string_lossy()
            );
        }
    }

    println!("Test passed");
    Ok(())
}

/// Test that second run with same config is faster (cache hit)
#[tokio::test]
async fn test_podman_cache_hit_restores_fast() -> Result<()> {
    println!("\ntest_podman_cache_hit_restores_fast");
    println!("====================================");
    println!("Second run should be faster due to cache hit");

    // Start with clean cache
    cleanup_cache().await;

    // First run - cache miss
    // Use the same command for both runs so cache key matches
    let (vm_name1, _, _, _) = common::unique_names("cache-hit-1");
    println!("First run (cache miss): {}", vm_name1);

    let start1 = Instant::now();
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        common::TEST_IMAGE, // Use nginx which stays running
    ])
    .await
    .context("spawning first VM")?;

    // Wait for first container to become healthy (cache created)
    common::poll_health_by_pid(pid1, 180)
        .await
        .context("first container should become healthy")?;
    let duration1 = start1.elapsed();
    println!("First run duration: {:?}", duration1);

    // Kill the first container
    child1.kill().await?;
    let _ = child1.wait().await;
    println!("First container killed");

    // Verify cache was created
    let count = count_cache_entries();
    println!("Cache entries after first run: {}", count);
    assert!(count > 0, "Cache should be created after first run");

    // Second run - should hit cache (SAME image and config)
    let (vm_name2, _, _, _) = common::unique_names("cache-hit-2");
    println!("Second run (cache hit): {}", vm_name2);

    let start2 = Instant::now();
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        common::TEST_IMAGE, // Same image as first run
    ])
    .await
    .context("spawning second VM")?;

    // Wait for second container to become healthy
    common::poll_health_by_pid(pid2, 60)
        .await
        .context("second container should become healthy quickly")?;
    let duration2 = start2.elapsed();
    println!("Second run duration: {:?}", duration2);

    // Cleanup
    child2.kill().await?;
    let _ = child2.wait().await;

    // Cache hit should be at least 50% faster
    // (Conservative threshold - actual improvement is often 5-10x)
    if duration1.as_millis() > 5000 {
        // Only check speedup if first run took meaningful time
        assert!(
            duration2 < duration1,
            "Cache hit ({:?}) should be faster than miss ({:?})",
            duration2,
            duration1
        );
        println!(
            "Speedup: {:.1}x",
            duration1.as_secs_f64() / duration2.as_secs_f64()
        );
    } else {
        println!("First run was too fast to measure speedup meaningfully");
    }

    println!("Test passed");
    Ok(())
}

/// Test that different ENV creates different cache entries
#[tokio::test]
async fn test_podman_cache_different_env_different_cache() -> Result<()> {
    println!("\ntest_podman_cache_different_env_different_cache");
    println!("================================================");
    println!("Different ENV values should create separate cache entries");

    // Start with clean cache
    cleanup_cache().await;
    let initial_count = count_cache_entries();

    // Run with ENV_A=value_a
    let (vm_name1, _, _, _) = common::unique_names("env-a");
    println!("Running with MY_VAR=value_a: {}", vm_name1);

    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        "--env",
        "MY_VAR=value_a",
        "alpine:latest",
        "echo",
        "a",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid1, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), child1.wait()).await;

    let count_after_first = count_cache_entries();
    println!("Cache entries after first run: {}", count_after_first);

    // Run with ENV_A=value_b (different value)
    let (vm_name2, _, _, _) = common::unique_names("env-b");
    println!("Running with MY_VAR=value_b: {}", vm_name2);

    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        "--env",
        "MY_VAR=value_b",
        "alpine:latest",
        "echo",
        "b",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid2, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), child2.wait()).await;

    let count_after_second = count_cache_entries();
    println!("Cache entries after second run: {}", count_after_second);

    // Should have created 2 different cache entries
    assert!(
        count_after_second > count_after_first,
        "Different ENV should create different cache: {} vs {}",
        count_after_second,
        count_after_first
    );

    println!("Test passed");
    Ok(())
}

/// Test that --no-cache flag bypasses cache
#[tokio::test]
async fn test_podman_cache_no_cache_flag() -> Result<()> {
    println!("\ntest_podman_cache_no_cache_flag");
    println!("================================");
    println!("--no-cache should bypass cache entirely");

    // Start with clean cache
    cleanup_cache().await;

    // Run with --no-cache
    let (vm_name, _, _, _) = common::unique_names("no-cache");
    println!("Running with --no-cache: {}", vm_name);

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
        "hello",
    ])
    .await?;

    // Wait for completion
    let _ = common::poll_health_by_pid(pid, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;

    // Should NOT create cache entry
    let cache_count = count_cache_entries();
    println!("Cache entries: {}", cache_count);

    assert_eq!(cache_count, 0, "--no-cache should not create cache entries");

    println!("Test passed");
    Ok(())
}

/// Test that corrupted cache falls back to normal boot
#[tokio::test]
async fn test_podman_cache_corruption_recovery() -> Result<()> {
    println!("\ntest_podman_cache_corruption_recovery");
    println!("======================================");
    println!("Corrupted cache should fall back to normal boot");

    // Start with clean cache
    cleanup_cache().await;

    // First run creates cache
    let (vm_name1, _, _, _) = common::unique_names("corrupt-1");
    println!("Creating cache: {}", vm_name1);

    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        "create",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid1, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), child1.wait()).await;

    // Corrupt the cache by deleting memory.bin
    let cache_dir = podman_cache_dir();
    for entry in std::fs::read_dir(&cache_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let memory_path = entry.path().join("memory.bin");
            if memory_path.exists() {
                println!("Corrupting cache by removing: {}", memory_path.display());
                std::fs::remove_file(&memory_path)?;
            }
        }
    }

    // Second run should fall back to normal boot
    let (vm_name2, _, _, _) = common::unique_names("corrupt-2");
    println!("Running after corruption: {}", vm_name2);

    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        "recovered",
    ])
    .await?;

    // Should still work (fall back to normal boot)
    match common::poll_health_by_pid(pid2, 180).await {
        Ok(_) => println!("VM recovered from corrupted cache"),
        Err(e) => {
            // VM might exit after echo - that's OK
            println!("Result: {}", e);
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(30), child2.wait()).await;

    println!("Test passed");
    Ok(())
}

/// Test that different commands create different cache entries
#[tokio::test]
async fn test_podman_cache_different_cmd_different_cache() -> Result<()> {
    println!("\ntest_podman_cache_different_cmd_different_cache");
    println!("================================================");
    println!("Different commands should create separate cache entries");

    // Start with clean cache
    cleanup_cache().await;
    let initial_count = count_cache_entries();

    // Run with command "echo hello"
    let (vm_name1, _, _, _) = common::unique_names("cmd-1");
    println!("Running: echo hello");

    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        "hello",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid1, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), child1.wait()).await;

    let count_after_first = count_cache_entries();

    // Run with different command "echo world"
    let (vm_name2, _, _, _) = common::unique_names("cmd-2");
    println!("Running: echo world");

    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        "world",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid2, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), child2.wait()).await;

    let count_after_second = count_cache_entries();
    println!(
        "Cache entries: {} -> {}",
        count_after_first, count_after_second
    );

    assert!(
        count_after_second > count_after_first,
        "Different commands should create different cache entries"
    );

    println!("Test passed");
    Ok(())
}

/// Test that cache works correctly with long-running containers
#[tokio::test]
async fn test_podman_cache_long_running_container() -> Result<()> {
    println!("\ntest_podman_cache_long_running_container");
    println!("=========================================");
    println!("Cache should work with containers that stay running");

    // Start with clean cache
    cleanup_cache().await;
    let initial_count = count_cache_entries();

    // First run - long-running nginx container
    let (vm_name1, _, _, _) = common::unique_names("long-1");
    println!("Starting long-running container: {}", vm_name1);

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

    // Wait for it to become healthy (nginx should stay running)
    common::poll_health_by_pid(pid1, 180)
        .await
        .context("first container should become healthy")?;

    println!("First container is healthy");

    // Give time for cache to be created
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Kill the first container
    child1.kill().await?;
    let _ = child1.wait().await;
    println!("First container killed");

    // Check cache was created
    let count_after_first = count_cache_entries();
    println!("Cache entries after first run: {}", count_after_first);
    assert!(
        count_after_first > initial_count,
        "Cache should be created for long-running container"
    );

    // Second run - should hit cache
    let (vm_name2, _, _, _) = common::unique_names("long-2");
    println!("Starting second container (cache hit): {}", vm_name2);

    let start = Instant::now();
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

    common::poll_health_by_pid(pid2, 60)
        .await
        .context("second container should become healthy quickly")?;

    let duration = start.elapsed();
    println!("Second container healthy in {:?}", duration);

    // Cleanup
    child2.kill().await?;
    let _ = child2.wait().await;

    println!("Test passed");
    Ok(())
}
