//! Test that --user mode pulls registry images into rootless storage.
//!
//! Registry images (non-localhost/) are always pulled inside the VM via `podman pull`.
//! With --user, this must run as the target user so the image ends up in rootless
//! podman storage. (localhost/ images go through host-side export/archive instead.)

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Test that --user pulls registry images into rootless storage (not root's).
/// This verifies pull_image() uses the rootless podman prefix.
#[tokio::test]
async fn test_user_mode_pull_image_into_rootless_storage() -> Result<()> {
    println!("\nUser mode test: registry image pulled into rootless storage");
    println!("===========================================================");

    let (vm_name, _, _, _) = common::unique_names("user-pull");
    let uid = 1000;
    let gid = 1000;
    let username = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| format!("u{}", uid));

    println!(
        "  Starting VM with --user {}:{} (username={})...",
        uid, gid, username
    );
    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--user",
        &format!("{}:{}", uid, gid),
        common::ALPINE_IMAGE,
        "sleep",
        "120",
    ])
    .await
    .context("spawning fcvm with --user")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to become healthy...");

    common::poll_health_by_pid(fcvm_pid, 300).await?;
    println!("  VM is healthy");

    // Verify the image was pulled into rootless storage (not root's storage)
    println!("  Checking image is in rootless storage...");
    let images_output = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            &username,
            "--",
            "podman",
            "images",
            "--format",
            "{{.Repository}}:{{.Tag}}",
        ],
    )
    .await?;
    println!("  Rootless images: {}", images_output.trim());
    assert!(
        images_output.contains("alpine"),
        "Alpine image should be in rootless storage, got: {}",
        images_output.trim()
    );

    // Verify root's storage does NOT have the image (it should be rootless-only)
    let root_images = common::exec_in_vm(
        fcvm_pid,
        &["podman", "images", "--format", "{{.Repository}}:{{.Tag}}"],
    )
    .await?;
    println!("  Root images: {}", root_images.trim());
    assert!(
        !root_images.contains("alpine"),
        "Alpine image should NOT be in root's storage, got: {}",
        root_images.trim()
    );

    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;

    println!("  PASSED!");
    Ok(())
}
