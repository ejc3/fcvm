//! Podman cache integration tests
//!
//! Tests the container-layer caching feature that caches VM state after
//! container image is loaded, enabling fast subsequent launches.
//!
//! ## Test Isolation Pattern
//!
//! Each test uses unique parameters (env vars) to generate unique cache keys.
//! Tests only verify their OWN cache entries, never global state.
//! This enables safe parallel execution.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Get the podman cache directory path
fn podman_cache_dir() -> PathBuf {
    PathBuf::from("/mnt/fcvm-btrfs/podman-cache")
}

/// Compute the cache key for given parameters (mirrors fcvm's compute_podman_cache_key)
fn compute_cache_key(image: &str, cmd: &[&str], env: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(image.as_bytes());
    // Default VM config: cpu=2, mem=2048, privileged=false, interactive=false, tty=false
    hasher.update(b"2"); // cpu (fcvm default)
    hasher.update(b"2048"); // mem (fcvm default)
    hasher.update(b"0"); // privileged
    hasher.update(b"0"); // interactive
    hasher.update(b"0"); // tty

    // Environment (sorted)
    let mut env_sorted: Vec<_> = env.iter().collect();
    env_sorted.sort();
    for e in env_sorted {
        hasher.update(e.as_bytes());
    }

    // Command
    hasher.update(cmd.join(" ").as_bytes());

    let result = hasher.finalize();
    hex::encode(&result[..6])
}

/// Check if a specific cache entry exists and is complete
fn cache_entry_exists(cache_key: &str) -> bool {
    let path = podman_cache_dir().join(cache_key);
    path.join("memory.bin").exists()
        && path.join("vmstate.bin").exists()
        && path.join("disk.raw").exists()
        && path.join("config.json").exists()
}

/// Delete a specific cache entry (for test isolation)
fn delete_cache_entry(cache_key: &str) {
    let path = podman_cache_dir().join(cache_key);
    let _ = std::fs::remove_dir_all(&path);
}

/// Wait for a cache entry to be created (with timeout)
async fn wait_for_cache_entry(cache_key: &str, timeout_secs: u64) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(timeout_secs) {
        if cache_entry_exists(cache_key) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Test that first run creates a cache entry
#[tokio::test]
async fn test_podman_cache_miss_creates_cache() -> Result<()> {
    println!("\ntest_podman_cache_miss_creates_cache");
    println!("=====================================");

    // Use unique env var to get unique cache key
    let test_id = format!("miss-{}", std::process::id());
    let env_var = format!("TEST_ID={}", test_id);
    let cache_key = compute_cache_key("alpine:latest", &["echo", "hello"], &[&env_var]);
    println!("Test cache key: {}", cache_key);

    // Clean our specific cache entry
    delete_cache_entry(&cache_key);
    assert!(
        !cache_entry_exists(&cache_key),
        "Cache should not exist initially"
    );

    // Run container
    let (vm_name, _, _, _) = common::unique_names("cache-miss");
    println!("Starting VM: {}", vm_name);

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--env",
        &env_var,
        "alpine:latest",
        "echo",
        "hello",
    ])
    .await
    .context("spawning fcvm")?;

    // Wait for container (may exit quickly)
    let _ = common::poll_health_by_pid(pid, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child.wait()).await;

    // Verify OUR cache entry was created
    let created = wait_for_cache_entry(&cache_key, 10).await;
    assert!(created, "Cache entry {} should be created", cache_key);
    println!("Cache entry verified: {}", cache_key);

    println!("Test passed");
    Ok(())
}

/// Test that second run with same config hits cache
#[tokio::test]
async fn test_podman_cache_hit_restores_fast() -> Result<()> {
    println!("\ntest_podman_cache_hit_restores_fast");
    println!("====================================");

    // Use unique env var to get unique cache key
    let test_id = format!("hit-{}", std::process::id());
    let env_var = format!("TEST_ID={}", test_id);
    let cache_key = compute_cache_key(common::TEST_IMAGE, &[], &[&env_var]);
    println!("Test cache key: {}", cache_key);

    // Clean our specific cache entry first
    delete_cache_entry(&cache_key);

    // First run - cache miss (creates cache)
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
        "--env",
        &env_var,
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid1, 180).await?;
    let duration1 = start1.elapsed();
    println!("First run duration: {:?}", duration1);

    child1.kill().await?;
    let _ = child1.wait().await;

    // Verify cache was created
    assert!(
        wait_for_cache_entry(&cache_key, 60).await,
        "Cache should be created"
    );
    println!("Cache entry created: {}", cache_key);

    // Second run - cache hit
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
        "--env",
        &env_var,
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid2, 180).await?;
    let duration2 = start2.elapsed();
    println!("Second run duration: {:?}", duration2);

    child2.kill().await?;
    let _ = child2.wait().await;

    // Second run should be faster (or at least not much slower)
    if duration1 > Duration::from_secs(5) {
        // Only check speedup if first run was slow enough to measure
        let speedup = duration1.as_secs_f64() / duration2.as_secs_f64();
        println!("Speedup: {:.1}x", speedup);
        // Cache should provide at least some speedup
        assert!(speedup > 1.0, "Cache hit should be faster than miss");
    } else {
        println!("First run was too fast to measure speedup meaningfully");
    }

    println!("Test passed");
    Ok(())
}

/// Test that different commands create different cache entries
#[tokio::test]
async fn test_podman_cache_different_commands() -> Result<()> {
    println!("\ntest_podman_cache_different_commands");
    println!("=====================================");

    let test_id = format!("cmd-{}", std::process::id());

    // Two different commands with same base env
    let env_var = format!("TEST_ID={}", test_id);
    let cache_key1 = compute_cache_key("alpine:latest", &["echo", "cmd1"], &[&env_var]);
    let cache_key2 = compute_cache_key("alpine:latest", &["echo", "cmd2"], &[&env_var]);

    println!("Cache key 1: {} (echo cmd1)", cache_key1);
    println!("Cache key 2: {} (echo cmd2)", cache_key2);
    assert_ne!(
        cache_key1, cache_key2,
        "Different commands should have different cache keys"
    );

    // Clean both
    delete_cache_entry(&cache_key1);
    delete_cache_entry(&cache_key2);

    // Run first command
    let (vm_name1, _, _, _) = common::unique_names("cmd-1");
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        "--env",
        &env_var,
        "alpine:latest",
        "echo",
        "cmd1",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid1, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child1.wait()).await;

    // Run second command
    let (vm_name2, _, _, _) = common::unique_names("cmd-2");
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        "--env",
        &env_var,
        "alpine:latest",
        "echo",
        "cmd2",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid2, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child2.wait()).await;

    // Both should have their own cache entries
    assert!(
        wait_for_cache_entry(&cache_key1, 10).await,
        "Cache 1 should exist"
    );
    assert!(
        wait_for_cache_entry(&cache_key2, 10).await,
        "Cache 2 should exist"
    );

    println!("Test passed");
    Ok(())
}

/// Test that different env vars create different cache entries
#[tokio::test]
async fn test_podman_cache_different_envs() -> Result<()> {
    println!("\ntest_podman_cache_different_envs");
    println!("================================");

    let test_id = format!("env-{}", std::process::id());

    let env_var_a = format!("TEST_ID={}-a", test_id);
    let env_var_b = format!("TEST_ID={}-b", test_id);
    let cache_key_a = compute_cache_key("alpine:latest", &["echo", "x"], &[&env_var_a]);
    let cache_key_b = compute_cache_key("alpine:latest", &["echo", "x"], &[&env_var_b]);

    println!("Cache key A: {}", cache_key_a);
    println!("Cache key B: {}", cache_key_b);
    assert_ne!(
        cache_key_a, cache_key_b,
        "Different envs should have different cache keys"
    );

    // Clean both
    delete_cache_entry(&cache_key_a);
    delete_cache_entry(&cache_key_b);

    // Run with env A
    let (vm_name_a, _, _, _) = common::unique_names("env-a");
    let (mut child_a, pid_a) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name_a,
        "--network",
        "rootless",
        "--env",
        &env_var_a,
        "alpine:latest",
        "echo",
        "x",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid_a, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child_a.wait()).await;

    // Run with env B
    let (vm_name_b, _, _, _) = common::unique_names("env-b");
    let (mut child_b, pid_b) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name_b,
        "--network",
        "rootless",
        "--env",
        &env_var_b,
        "alpine:latest",
        "echo",
        "x",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid_b, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child_b.wait()).await;

    // Both should have their own cache entries
    assert!(
        wait_for_cache_entry(&cache_key_a, 10).await,
        "Cache A should exist"
    );
    assert!(
        wait_for_cache_entry(&cache_key_b, 10).await,
        "Cache B should exist"
    );

    println!("Test passed");
    Ok(())
}

/// Test that --no-cache flag works
#[tokio::test]
async fn test_podman_cache_no_cache_flag() -> Result<()> {
    println!("\ntest_podman_cache_no_cache_flag");
    println!("================================");

    // Use unique env var
    let test_id = format!("nocache-{}", std::process::id());
    let env_var = format!("TEST_ID={}", test_id);
    let cache_key = compute_cache_key("alpine:latest", &["echo", "hi"], &[&env_var]);
    println!("Test cache key: {}", cache_key);

    // Clean our cache entry
    delete_cache_entry(&cache_key);

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
        "--env",
        &env_var,
        "alpine:latest",
        "echo",
        "hi",
    ])
    .await?;

    let _ = common::poll_health_by_pid(pid, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;

    // Should NOT create cache entry
    tokio::time::sleep(Duration::from_secs(2)).await; // Give it time to NOT create
    assert!(
        !cache_entry_exists(&cache_key),
        "--no-cache should not create cache"
    );

    println!("Test passed");
    Ok(())
}

/// Test that incomplete cache (missing files) is treated as miss
#[tokio::test]
async fn test_podman_cache_incomplete_treated_as_miss() -> Result<()> {
    println!("\ntest_podman_cache_incomplete_treated_as_miss");
    println!("=============================================");

    // Use unique env var
    let test_id = format!("incomplete-{}", std::process::id());
    let env_var = format!("TEST_ID={}", test_id);
    let cache_key = compute_cache_key("alpine:latest", &["echo", "recovered"], &[&env_var]);
    println!("Test cache key: {}", cache_key);

    // Create an incomplete cache entry (only config.json, missing other files)
    // This simulates an interrupted cache creation
    let cache_path = podman_cache_dir().join(&cache_key);
    std::fs::create_dir_all(&cache_path)?;
    // Don't create memory.bin, vmstate.bin, disk.raw - only the directory
    println!("Created incomplete cache directory (no files)");

    // Run - should treat incomplete cache as miss and boot fresh
    let (vm_name, _, _, _) = common::unique_names("incomplete");
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--env",
        &env_var,
        "alpine:latest",
        "echo",
        "recovered",
    ])
    .await?;

    let _ = common::poll_health_by_pid(pid, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child.wait()).await;

    // Should have created a valid cache after fresh boot
    assert!(
        wait_for_cache_entry(&cache_key, 60).await,
        "Should create valid cache after fresh boot"
    );

    println!("Test passed");
    Ok(())
}

/// Test long-running container (cache should include running container state)
#[tokio::test]
async fn test_podman_cache_long_running_container() -> Result<()> {
    println!("\ntest_podman_cache_long_running_container");
    println!("=========================================");

    // Use unique env var
    let test_id = format!("long-{}", std::process::id());
    let env_var = format!("TEST_ID={}", test_id);
    let cache_key = compute_cache_key(common::TEST_IMAGE, &[], &[&env_var]);
    println!("Test cache key: {}", cache_key);

    // Clean our cache entry
    delete_cache_entry(&cache_key);

    // First run - creates cache
    let (vm_name1, _, _, _) = common::unique_names("long-1");
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        "--env",
        &env_var,
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid1, 180).await?;
    println!("First container healthy");

    child1.kill().await?;
    let _ = child1.wait().await;

    // Verify cache created
    assert!(
        wait_for_cache_entry(&cache_key, 60).await,
        "Cache should be created"
    );

    // Second run - from cache
    let (vm_name2, _, _, _) = common::unique_names("long-2");
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        "--env",
        &env_var,
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid2, 180).await?;
    println!("Second container healthy (from cache)");

    child2.kill().await?;
    let _ = child2.wait().await;

    println!("Test passed");
    Ok(())
}
