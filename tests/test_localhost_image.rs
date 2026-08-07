//! Integration test for localhost/ container images
//!
//! Tests all three image delivery modes:
//! - Overlay (default): Pre-built overlay storage mounted as additionalImageStore
//! - Archive: Docker archive imported via podman load at boot
//! - Btrfs: Pre-built btrfs image mounted as graphroot (requires btrfs kernel)

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Test with overlay mode (default, additionalImageStore)
#[tokio::test]
async fn test_localhost_overlay_mode() -> Result<()> {
    run_localhost_test(Some("overlay"), "localhost-overlay").await
}

/// Test with archive mode (podman load at boot)
#[tokio::test]
async fn test_localhost_archive_mode() -> Result<()> {
    run_localhost_test(Some("archive"), "localhost-archive").await
}

/// Test with btrfs mode (pre-built btrfs image as graphroot, requires btrfs kernel)
#[tokio::test]
async fn test_localhost_btrfs_mode() -> Result<()> {
    run_localhost_test_with_kernel(Some("btrfs"), Some("btrfs"), "localhost-btrfs").await
}

/// Test with default mode (auto-detect, should be overlay without btrfs kernel)
#[tokio::test]
async fn test_localhost_default_mode() -> Result<()> {
    run_localhost_test(None, "localhost-default").await
}

/// Unique image name for each test to avoid parallel podman build races.
/// When multiple tests build the same tag concurrently, `podman save` can see
/// a digest change mid-export, causing "image changed while it was being exported".
fn image_name_for_suffix(suffix: &str) -> String {
    format!("localhost/test-hello-{}", suffix)
}

async fn run_localhost_test(image_mode: Option<&str>, suffix: &str) -> Result<()> {
    run_localhost_test_with_kernel(image_mode, None, suffix).await
}

async fn run_localhost_test_with_kernel(
    image_mode: Option<&str>,
    kernel_profile: Option<&str>,
    suffix: &str,
) -> Result<()> {
    let mode_label = image_mode.unwrap_or("default (auto-detect)");
    let image_name = image_name_for_suffix(suffix);
    // Unique CMD output per test so each image has a distinct content digest
    // (see build_test_image for why the digest must differ, not just the name).
    let marker = format!("Hello from localhost container {}!", suffix);

    println!("\nLocalhost Image Test ({})", mode_label);
    println!("====================");

    // Find fcvm binary
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names(suffix);

    // Step 1: Build a test container image on the host (unique name + content per test)
    println!("Step 1: Building test container image {}...", image_name);
    build_test_image(&image_name, &marker).await?;

    // Step 2: Start VM with localhost image (rootless mode)
    println!(
        "Step 2: Starting VM with {} image ({})...",
        image_name, mode_label
    );
    let mut args = vec!["podman", "run", "--name", &vm_name];
    if let Some(mode) = image_mode {
        args.push("--image-mode");
        args.push(mode);
    }
    if let Some(profile) = kernel_profile {
        args.push("--kernel-profile");
        args.push(profile);
    }
    args.push(&image_name);

    let mut cmd = tokio::process::Command::new(&fcvm_path);
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    common::set_test_pdeathsig(&mut cmd);
    let mut child = cmd.spawn().context("spawning fcvm podman run")?;

    let fcvm_pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get child PID"))?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Monitor stdout for container output (goes directly to stdout without prefix)
    let stdout = child.stdout.take();
    let expected_marker = marker.clone();
    let stdout_task = tokio::spawn(async move {
        let mut found_hello = false;
        if let Some(stdout) = stdout {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[VM stdout] {}", line);
                // Check for this test's unique container output marker
                if line.contains(&expected_marker) {
                    found_hello = true;
                }
            }
        }
        found_hello
    });

    // Monitor stderr for exit status (logs still go to stderr)
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut exited_zero = false;
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[VM stderr] {}", line);
                // Check for container exit with code 0
                if line.contains("Container exit notification received")
                    && line.contains("exit_code=0")
                {
                    exited_zero = true;
                }
            }
        }
        exited_zero
    });

    // Wait for the process to exit (with timeout)
    // 120s to handle podman storage lock contention during parallel test runs
    let timeout = Duration::from_secs(120);
    let result = tokio::time::timeout(timeout, child.wait()).await;

    match result {
        Ok(Ok(status)) => {
            println!("  fcvm process exited with status: {}", status);
        }
        Ok(Err(e)) => {
            println!("  Error waiting for process: {}", e);
        }
        Err(_) => {
            println!(
                "  Timeout waiting for VM ({}s), killing...",
                timeout.as_secs()
            );
            common::kill_process(fcvm_pid).await;
        }
    }

    // Wait for output tasks
    let found_hello = stdout_task.await.unwrap_or(false);
    let container_exited_zero = stderr_task.await.unwrap_or(false);

    // Check results - verify we got the container output
    if found_hello {
        println!("\n  LOCALHOST IMAGE TEST PASSED! ({})", mode_label);
        println!("  - Container ran and printed: {}", marker);
        if container_exited_zero {
            println!("  - Container exited with code 0");
        }
        Ok(())
    } else {
        println!("\n  LOCALHOST IMAGE TEST FAILED! ({})", mode_label);
        println!("  - Did not find expected output: '{}'", marker);
        println!("  - Check logs above for error details");
        anyhow::bail!("Localhost image test failed ({})", mode_label)
    }
}

/// Build a simple test container image using podman with a unique tag and a
/// unique CMD marker.
///
/// Each test uses both its own image NAME and its own CONTENT. Unique names stop
/// concurrent tests from rebuilding the same tag while another is exporting it.
/// Unique content is just as important: fcvm caches the delivered image archive
/// keyed by content digest, and the guest loads it tagged with whichever name
/// first populated that digest. If two tests built identical content (same
/// digest) under different names, the second test's `podman run <its-name>`
/// would miss the cached archive (tagged with the first name) and fall back to
/// pulling from registry `localhost`, which fails. The `{marker}` makes each
/// image's content — and therefore its digest — distinct.
async fn build_test_image(image_name: &str, marker: &str) -> Result<()> {
    use std::io::Write;
    use tempfile::TempDir;

    // Create a temporary directory for the Containerfile
    let temp_dir = TempDir::new().context("creating temp dir")?;
    let containerfile_path = temp_dir.path().join("Containerfile");

    // Write a simple Containerfile (use ECR to avoid Docker Hub rate limits)
    let mut file = std::fs::File::create(&containerfile_path)?;
    writeln!(
        file,
        r#"FROM public.ecr.aws/nginx/nginx:alpine
CMD ["echo", "{marker}"]"#
    )?;

    // Build the image with podman
    let output = tokio::process::Command::new("podman")
        .args([
            "build",
            "-t",
            image_name,
            "-f",
            &containerfile_path.to_string_lossy(),
            temp_dir.path().to_str().unwrap(),
        ])
        .output()
        .await
        .context("running podman build")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to build test image: {}", stderr);
    }

    println!("  Built {} image", image_name);
    Ok(())
}

/// Get the immutable image ID (`sha256:…`) of a localhost image via podman.
async fn podman_image_id(image_name: &str) -> Result<String> {
    let out = tokio::process::Command::new("podman")
        .args(["image", "inspect", image_name, "--format", "{{.Id}}"])
        .output()
        .await
        .context("podman image inspect")?;
    anyhow::ensure!(
        out.status.success(),
        "podman inspect failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The container `Cmd` recorded in a docker-archive, via `skopeo inspect --config`.
async fn archive_config_cmd(archive: &std::path::Path) -> Result<Vec<String>> {
    let out = tokio::process::Command::new("skopeo")
        .args([
            "inspect",
            "--config",
            &format!("docker-archive:{}", archive.display()),
        ])
        .output()
        .await
        .context("skopeo inspect --config")?;
    anyhow::ensure!(
        out.status.success(),
        "skopeo inspect --config failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cfg: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing config")?;
    Ok(cfg["config"]["Cmd"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// The RepoTags recorded in a docker-archive's manifest.json (what `podman load` uses
/// to name the loaded image). Read directly from the tar — `skopeo inspect` does not
/// surface docker-archive RepoTags reliably.
async fn archive_repo_tags(archive: &std::path::Path) -> Result<Vec<String>> {
    let out = tokio::process::Command::new("tar")
        .args(["-xOf", &archive.to_string_lossy(), "manifest.json"])
        .output()
        .await
        .context("extracting manifest.json")?;
    anyhow::ensure!(out.status.success(), "tar extract manifest.json failed");
    let manifest: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing manifest.json")?;
    Ok(manifest
        .as_array()
        .and_then(|a| a.first())
        .and_then(|m| m["RepoTags"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// #598: the export must pin the immutable image ID's content even when the tag is
/// rebuilt between the inspect (cache-key capture) and the export — otherwise the
/// cached archive holds newer content than the digest used as its cache key. It must
/// also preserve the repo tag so the guest can `podman load` and run it by name.
#[tokio::test]
async fn test_export_pins_immutable_content_across_tag_rebuild() -> Result<()> {
    println!("\ntest_export_pins_immutable_content_across_tag_rebuild (#598)");
    let image_name = image_name_for_suffix("export-pin-598");
    let marker_v1 = "MARKER_V1_export598";
    let marker_v2 = "MARKER_V2_export598";

    // Build v1 and capture the immutable id fcvm would record at inspect time.
    build_test_image(&image_name, marker_v1).await?;
    let v1_id = podman_image_id(&image_name).await?;

    // Rebuild the SAME tag with different content. The tag now resolves to v2, but the
    // v1 image still exists under v1_id — exactly the race #598 describes.
    build_test_image(&image_name, marker_v2).await?;
    let v2_id = podman_image_id(&image_name).await?;
    anyhow::ensure!(v1_id != v2_id, "rebuild should produce a new image id");

    // Export by the v1 id (what fcvm captured before the rebuild).
    let tmp = tempfile::TempDir::new()?;
    let archive = tmp.path().join("out.docker.tar");
    fcvm::commands::podman::export_image_archive(&v1_id, &image_name, &archive)
        .await
        .context("export_image_archive")?;

    // The archive must hold v1's content, NOT the rebuilt v2.
    let cmd = archive_config_cmd(&archive).await?;
    assert!(
        cmd.iter().any(|s| s.contains(marker_v1)),
        "archive must hold v1 content; got Cmd {:?}",
        cmd
    );
    assert!(
        !cmd.iter().any(|s| s.contains(marker_v2)),
        "archive must NOT hold the rebuilt v2 content; got Cmd {:?}",
        cmd
    );

    // The original repo tag must be preserved for guest-side `podman load`. skopeo
    // normalizes an untagged reference to `:latest`, so accept either form.
    let repo_tags = archive_repo_tags(&archive).await?;
    let tagged = format!("{}:latest", image_name);
    assert!(
        repo_tags.iter().any(|t| t == &image_name || t == &tagged),
        "archive RepoTags must include {} (or {}); got {:?}",
        image_name,
        tagged,
        repo_tags
    );

    let _ = tokio::process::Command::new("podman")
        .args(["rmi", "-f", &image_name, &v1_id, &v2_id])
        .output()
        .await;
    println!("✅ export pinned v1 content across tag rebuild (#598)");
    Ok(())
}
