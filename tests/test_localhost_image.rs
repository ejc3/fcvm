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

/// Overlay mode with a container user. The guest runs rootless podman as that user,
/// which cannot open a store built by root podman (as it is when fcvm runs as root)
/// unless fc-agent shows the store through an id-mapped mount.
#[tokio::test]
async fn test_localhost_overlay_mode_with_user() -> Result<()> {
    run_localhost_test_with_kernel(
        Some("overlay"),
        None,
        Some("1000:1000"),
        "localhost-overlay-user",
    )
    .await
}

/// The file the store tests look for: in the image store, and nowhere in the user's own
/// podman storage. It is owned by root in the image.
const STORE_MARKER: &str = "fcvm-store-marker";
/// A file the image gives to 1234:1234.
const STORE_CHOWNED: &str = "fcvm-store-chowned";

/// Overlay mode with a container user runs the container on the image's layers in the
/// store, with the ownership the image gives its files. The guest's rootless podman runs
/// it with `--userns=keep-id`; when the store's layers do not carry the ids that mapping
/// wants, podman copies every file of the image into the container's layer in the user's
/// storage to change its owner, which for a large image fills the guest's disk
/// ("storage-chown-by-maps ... no space left on device"). The marker file is in the
/// store, must not be under /home, and the container sees each file's owner as the
/// image set it.
#[tokio::test]
async fn test_localhost_overlay_mode_with_user_runs_on_the_store() -> Result<()> {
    run_on_the_store("1000:1000", "localhost-overlay-user-store").await
}

async fn run_on_the_store(user: &str, suffix: &str) -> Result<()> {
    let image_name = image_name_for_suffix(suffix);
    build_marker_image(&image_name, suffix).await?;

    let (vm_name, _, _, _) = common::unique_names(suffix);
    let args = [
        "podman",
        "run",
        "--name",
        &vm_name,
        "--image-mode",
        "overlay",
        "--user",
        user,
        &image_name,
    ];
    let (mut child, fcvm_pid) = common::spawn_fcvm(&args).await.context("spawning fcvm")?;

    let checked = async {
        common::poll_health_by_pid(fcvm_pid, 180)
            .await
            .context("VM failed to become healthy")?;
        let in_store = common::exec_in_vm(
            fcvm_pid,
            &[&format!(
                "find /mnt/image-store/overlay -name {STORE_MARKER} | wc -l"
            )],
        )
        .await
        .context("looking for the marker in the image store")?;
        let in_user_storage = common::exec_in_vm(
            fcvm_pid,
            &[&format!("find /home -name {STORE_MARKER} | wc -l")],
        )
        .await
        .context("looking for the marker in the user's storage")?;
        let owners = common::exec_in_container(
            fcvm_pid,
            &[&format!("stat -c %u:%g /{STORE_MARKER} /{STORE_CHOWNED}")],
        )
        .await
        .context("reading the owners the container sees")?;
        println!(
            "  marker copies: {} in the image store, {} under /home; owners in the container: {}",
            in_store.trim(),
            in_user_storage.trim(),
            owners.split_whitespace().collect::<Vec<_>>().join(" ")
        );
        anyhow::ensure!(
            in_store.trim() == "1",
            "the image store should hold the marker once, found {:?}",
            in_store.trim()
        );
        anyhow::ensure!(
            in_user_storage.trim() == "0",
            "podman copied the image's files into the user's storage ({:?} copies of the \
             marker under /home): the store's layers do not carry the ids keep-id wants",
            in_user_storage.trim()
        );
        anyhow::ensure!(
            owners.split_whitespace().collect::<Vec<_>>() == ["0:0", "1234:1234"],
            "the container sees the image's files owned by {:?}, where the image sets 0:0 \
             and 1234:1234",
            owners.trim()
        );
        Ok(())
    }
    .await;

    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;
    checked
}

/// Overlay mode on a btrfs root grows the root filesystem to `--rootfs-size`. The host
/// extends the root disk; on a btrfs root the guest grows the filesystem into it, and
/// overlay mode must do that too, because the container's writable layer lives on the
/// root filesystem.
#[tokio::test]
async fn test_localhost_overlay_mode_grows_a_btrfs_root() -> Result<()> {
    let suffix = "localhost-overlay-btrfs-grow";
    let image_name = image_name_for_suffix(suffix);
    build_marker_image(&image_name, suffix).await?;

    let (vm_name, _, _, _) = common::unique_names(suffix);
    let args = [
        "podman",
        "run",
        "--name",
        &vm_name,
        "--kernel-profile",
        "btrfs",
        "--image-mode",
        "overlay",
        "--rootfs-size",
        "30G",
        &image_name,
    ];
    let (mut child, fcvm_pid) = common::spawn_fcvm(&args).await.context("spawning fcvm")?;

    let checked = async {
        common::poll_health_by_pid(fcvm_pid, 180)
            .await
            .context("VM failed to become healthy")?;
        let root = common::exec_in_vm(
            fcvm_pid,
            &["findmnt -n -o FSTYPE / && df -B1 --output=avail / | tail -1"],
        )
        .await
        .context("reading the root filesystem")?;
        let mut lines = root.split_whitespace();
        let (fstype, avail) = (lines.next().unwrap_or(""), lines.next().unwrap_or(""));
        let avail: u64 = avail
            .parse()
            .with_context(|| format!("parsing df output {root:?}"))?;
        println!(
            "  root: {fstype}, {:.1} GiB available",
            avail as f64 / (1u64 << 30) as f64
        );
        anyhow::ensure!(
            fstype == "btrfs",
            "the btrfs profile's root should be btrfs, got {fstype:?}"
        );
        // --rootfs-size is the free space asked for; allow 5% for filesystem overhead.
        let wanted = 30u64 << 30;
        anyhow::ensure!(
            avail >= wanted / 100 * 95,
            "the root filesystem has {:.1} GiB available, where --rootfs-size asked for 30 GiB",
            avail as f64 / (1u64 << 30) as f64
        );
        Ok(())
    }
    .await;

    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;
    checked
}

/// Test with archive mode (podman load at boot)
#[tokio::test]
async fn test_localhost_archive_mode() -> Result<()> {
    run_localhost_test(Some("archive"), "localhost-archive").await
}

/// Test with btrfs mode (pre-built btrfs image as graphroot, requires btrfs kernel)
#[tokio::test]
async fn test_localhost_btrfs_mode() -> Result<()> {
    run_localhost_test_with_kernel(Some("btrfs"), Some("btrfs"), None, "localhost-btrfs").await
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
    run_localhost_test_with_kernel(image_mode, None, None, suffix).await
}

async fn run_localhost_test_with_kernel(
    image_mode: Option<&str>,
    kernel_profile: Option<&str>,
    user: Option<&str>,
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
    if let Some(user) = user {
        args.push("--user");
        args.push(user);
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
    build_image(
        image_name,
        &format!("FROM public.ecr.aws/nginx/nginx:alpine\nCMD [\"echo\", \"{marker}\"]\n"),
        &[],
    )
    .await
}

/// Build `image_name` from `containerfile` (use ECR to avoid Docker Hub rate limits),
/// with `files` (name, content) in the build context.
async fn build_image(image_name: &str, containerfile: &str, files: &[(&str, &str)]) -> Result<()> {
    let temp_dir = tempfile::TempDir::new().context("creating temp dir")?;
    std::fs::write(temp_dir.path().join("Containerfile"), containerfile)?;
    for (name, content) in files {
        std::fs::write(temp_dir.path().join(name), content)?;
    }
    let output = tokio::process::Command::new("podman")
        .args(["build", "-t", image_name])
        .arg(temp_dir.path())
        .output()
        .await
        .context("running podman build")?;
    anyhow::ensure!(
        output.status.success(),
        "Failed to build test image: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("  Built {} image", image_name);
    Ok(())
}

/// Build an image that stays running and holds [`STORE_MARKER`], owned by root, and
/// [`STORE_CHOWNED`], owned by 1234:1234, at its root. `suffix` goes into the marker's
/// content, so each test's image has its own digest.
async fn build_marker_image(image_name: &str, suffix: &str) -> Result<()> {
    build_image(
        image_name,
        &format!(
            "FROM public.ecr.aws/nginx/nginx:alpine\n\
             COPY {STORE_MARKER} /{STORE_MARKER}\n\
             COPY --chown=1234:1234 {STORE_MARKER} /{STORE_CHOWNED}\n\
             CMD [\"sleep\", \"3600\"]\n"
        ),
        &[(STORE_MARKER, &format!("{suffix}\n"))],
    )
    .await
}

/// A VM that runs a localhost image in overlay mode attaches the image's storage image, a
/// file of the host's image cache, and its snapshot records that path and the SHA-256 of
/// the file. Exported, the snapshot comes back on a host that has neither it nor the
/// storage image: `snapshot import` installs the disk at the recorded path as the build the
/// snapshot recorded, and the container of a clone restored from the imported snapshot
/// reads a file of the image, which is in the store on that disk.
#[tokio::test]
async fn test_localhost_overlay_snapshot_export_delete_import_restores() -> Result<()> {
    let suffix = "overlay-import";
    let image_name = image_name_for_suffix(suffix);
    build_marker_image(&image_name, suffix).await?;
    let (baseline, clone, snapshot, _) = common::unique_names(suffix);
    let fcvm_path = common::find_fcvm_binary()?;
    let snapshots = fcvm::storage::SnapshotManager::new(fcvm::paths::snapshot_dir());
    // On the store, not in /tmp: the archive holds the guest's memory image.
    let archive_dir = tempfile::Builder::new()
        .prefix("overlay-import-test-")
        .tempdir_in(fcvm::paths::data_dir())
        .context("creating a directory for the archive")?;
    let archive = archive_dir.path().join("snapshot.archive");

    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline,
            "--no-snapshot",
            "--image-mode",
            "overlay",
            &image_name,
        ],
        &baseline,
    )
    .await
    .context("spawning the VM to snapshot")?;
    let mut baseline_running = true;
    let mut clone_pid = None;
    // The storage image of this test's own image, removed afterwards whatever the outcome.
    let mut image_disk = None;
    let result = async {
        common::poll_health_by_pid(baseline_pid, 180).await?;
        common::create_snapshot_by_pid(baseline_pid, &snapshot).await?;
        common::kill_process(baseline_pid).await;
        baseline_running = false;

        let exported = snapshots.load_snapshot(&snapshot).await?;
        let disk = exported
            .metadata
            .image_disk_path
            .clone()
            .context("the snapshot records no image disk")?;
        let identity = exported
            .metadata
            .image_disk_identity
            .clone()
            .context("the snapshot records no image disk identity")?;
        anyhow::ensure!(
            disk.parent() == Some(fcvm::paths::image_cache_dir().as_path()) && disk.is_file(),
            "the snapshot's image disk {} is not a file of the image cache",
            disk.display()
        );
        image_disk = Some(disk.clone());
        let disk_len = std::fs::metadata(&disk)?.len();

        let mut export = tokio::process::Command::new(&fcvm_path);
        export
            .args(["snapshot", "export", &snapshot, "--output"])
            .arg(&archive);
        // Must not outlive a test that nextest killed: it holds the snapshot's lock.
        common::set_test_pdeathsig(&mut export);
        let out = export.output().await.context("running snapshot export")?;
        anyhow::ensure!(
            out.status.success(),
            "snapshot export failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // The importing host: no snapshot and no storage image.
        common::delete_snapshot(&snapshot).await?;
        std::fs::remove_file(&disk)
            .with_context(|| format!("removing the image disk {}", disk.display()))?;

        let mut import = tokio::process::Command::new(&fcvm_path);
        import.args(["snapshot", "import", "--input"]).arg(&archive);
        // Must not outlive a test that nextest killed: it holds the tag's lock.
        common::set_test_pdeathsig(&mut import);
        let out = import.output().await.context("running snapshot import")?;
        anyhow::ensure!(
            out.status.success(),
            "snapshot import failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let installed = std::fs::metadata(&disk)
            .with_context(|| format!("the import did not install {}", disk.display()))?;
        anyhow::ensure!(
            installed.len() == disk_len,
            "the installed image disk is {} bytes and the exported one was {disk_len}",
            installed.len()
        );
        // Hashed here, from the file's bytes. `fcvm::utils::verify_image_disk_identity`
        // answers from the digest the import recorded for the file and reads none of it.
        let installed_identity = {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            use std::io::Read;
            let mut file = std::fs::File::open(&disk)
                .with_context(|| format!("opening the installed {}", disk.display()))?;
            // sha2 0.11 hashers are not `io::Write`, so the file is fed in by hand.
            let mut buffer = vec![0u8; 1 << 20];
            loop {
                let read = file
                    .read(&mut buffer)
                    .with_context(|| format!("reading the installed {}", disk.display()))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            format!("sha256:{}", hex::encode(hasher.finalize()))
        };
        anyhow::ensure!(
            installed_identity == identity,
            "the installed image disk {} has the identity {installed_identity} and the \
             snapshot recorded {identity}",
            disk.display()
        );

        let (_clone_child, pid) = common::spawn_fcvm_with_logs(
            &["snapshot", "run", "--snapshot", &snapshot, "--name", &clone],
            &clone,
        )
        .await
        .context("spawning a clone of the imported snapshot")?;
        clone_pid = Some(pid);
        common::poll_health_by_pid(pid, 180)
            .await
            .context("the restore of the imported snapshot did not start a healthy clone")?;
        let marker = common::exec_in_container(pid, &[&format!("cat /{STORE_MARKER}")])
            .await
            .context("reading the image's marker file in the clone's container")?;
        anyhow::ensure!(
            marker.trim() == suffix,
            "the clone's container read {:?} from the image's marker file, which holds \
             {suffix:?}",
            marker.trim()
        );
        Ok(())
    }
    .await;
    if let Some(pid) = clone_pid {
        common::kill_process(pid).await;
    }
    if baseline_running {
        common::kill_process(baseline_pid).await;
    }
    common::delete_snapshot(&snapshot).await.ok();
    if let Some(disk) = image_disk {
        std::fs::remove_file(disk).ok();
    }
    result
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
    fcvm::commands::podman::export_image_archive(&v1_id, &image_name, &archive, None)
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
