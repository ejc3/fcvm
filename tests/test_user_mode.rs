//! Tests for --user UID:GID mode.
//!
//! Verifies that the container inside the VM runs as the specified user
//! via rootless podman with --userns=keep-id.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Test that --user creates the specified user and runs the container as that user.
/// Verifies: user creation in VM, rootless podman, correct UID/GID in container.
/// Username is auto-injected from host $USER (no --env USER= needed).
#[tokio::test]
async fn test_user_mode_runs_as_specified_uid() -> Result<()> {
    println!("\nUser mode test: container runs as specified user");
    println!("================================================");

    let (vm_name, _, _, _) = common::unique_names("user-mode");
    let uid = 1000;
    let gid = 1000;
    // Username comes from host $USER, auto-injected by fcvm
    // fcvm resolves username from host /etc/passwd for the given UID (like podman keep-id)
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

    // Verify the container process runs as the specified UID
    println!("  Checking container user identity...");
    let id_output = common::exec_in_container(fcvm_pid, &["id"]).await?;
    println!("  Container id output: {}", id_output.trim());

    assert!(
        id_output.contains(&format!("uid={}", uid)),
        "Container should run as uid={}, got: {}",
        uid,
        id_output.trim()
    );
    assert!(
        id_output.contains(&format!("gid={}", gid)),
        "Container should run as gid={}, got: {}",
        gid,
        id_output.trim()
    );

    // Verify the user was created in the VM with the correct name (from host $USER)
    println!("  Checking VM user creation...");
    let passwd_output = common::exec_in_vm(fcvm_pid, &["grep", &username, "/etc/passwd"]).await?;
    println!("  /etc/passwd entry: {}", passwd_output.trim());
    assert!(
        passwd_output.contains(&username),
        "User '{}' should exist in VM /etc/passwd, got: {}",
        username,
        passwd_output.trim()
    );

    // Verify rootless podman storage is separate (under the user's home)
    println!("  Checking rootless podman storage...");
    let storage_output = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            &username,
            "--",
            "podman",
            "info",
            "--format",
            "{{.Store.GraphRoot}}",
        ],
    )
    .await?;
    println!("  Podman storage root: {}", storage_output.trim());
    assert!(
        !storage_output.contains("/var/lib/containers"),
        "Rootless podman should NOT use /var/lib/containers, got: {}",
        storage_output.trim()
    );

    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;

    println!("  PASSED!");
    Ok(())
}

/// Test that --user works with FUSE volume mounts (files readable as target user).
#[tokio::test]
async fn test_user_mode_fuse_mount_readable() -> Result<()> {
    println!("\nUser mode test: FUSE mount readable as user");
    println!("=============================================");

    let (vm_name, _, _, _) = common::unique_names("user-fuse");
    let uid = 1000;
    let gid = 1000;
    // fcvm resolves username from host /etc/passwd for the given UID (like podman keep-id)
    let username = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| format!("u{}", uid));

    // Create a temp directory with a test file
    let host_dir = tempfile::tempdir().context("creating temp dir")?;
    let test_file = host_dir.path().join("hello.txt");
    std::fs::write(&test_file, "hello from host").context("writing test file")?;

    let map_arg = format!("{}:/mnt/data", host_dir.path().display());

    println!("  Starting VM with --user and --map...");
    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--user",
        &format!("{}:{}", uid, gid),
        "--map",
        &map_arg,
        common::ALPINE_IMAGE,
        "sleep",
        "120",
    ])
    .await
    .context("spawning fcvm with --user and --map")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to become healthy...");

    common::poll_health_by_pid(fcvm_pid, 300).await?;
    println!("  VM is healthy");

    // The FUSE mount is at /mnt/data in the VM; the container bind-mounts it
    // Verify the file is readable inside the container
    println!("  Reading test file through FUSE mount in container...");
    let cat_output = common::exec_in_container(fcvm_pid, &["cat", "/mnt/data/hello.txt"]).await?;
    println!("  File content: {}", cat_output.trim());
    assert_eq!(
        cat_output.trim(),
        "hello from host",
        "FUSE mount should be readable as user in container"
    );

    // Verify the file is also readable in the VM as the target user
    println!("  Reading test file as target user in VM...");
    let vm_cat_output = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            &username,
            "--",
            "cat",
            "/mnt/data/hello.txt",
        ],
    )
    .await?;
    assert_eq!(
        vm_cat_output.trim(),
        "hello from host",
        "FUSE mount should be readable as target user in VM"
    );

    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;

    println!("  PASSED!");
    Ok(())
}
