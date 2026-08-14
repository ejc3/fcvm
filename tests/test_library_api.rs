//! Library API integration tests — exercise start_vm, exec captured, and async connection
//!
//! These tests call fcvm's library API directly (in-process), not via subprocess.
//! Each test boots a full VM, so they take ~30-60s each.
//!
//! Uses multi_thread runtime because prepare_vm() does blocking I/O
//! (spawning processes, setting up namespaces) that needs concurrent task execution.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::Result;
use fcvm::cli::{NetworkMode, RunArgs};
use fcvm::commands::{exec, start_vm};

/// Build a minimal RunArgs for testing (rootless, no snapshot).
/// Uses nginx:alpine (long-running daemon) — alpine:latest exits immediately without TTY.
fn test_run_args(name: &str) -> RunArgs {
    RunArgs {
        name: name.to_string(),
        cpu: 2,
        mem: 1024,
        rootfs_size: "10G".to_string(),
        map: vec![],
        disk: vec![],
        disk_dir: vec![],
        nfs: vec![],
        env: vec![],
        cmd: None,
        publish: vec![],
        balloon: None,
        network: NetworkMode::Rootless,
        hypervisor: fcvm::cli::args::Hypervisor::Firecracker,
        health_check: None,
        privileged: false,
        interactive: false,
        tty: false,
        strace_agent: false,
        setup: false,
        kernel: None,
        kernel_profile: None,
        vsock_dir: None,
        no_snapshot: true,
        user: None,
        forward_localhost: vec![],
        hugepages: false,
        portable_volumes: false,
        image_mode: None,
        rootfs_type: None,
        label: vec![],
        non_blocking_output: false,
        health_check_timeout: 5,
        ipv6_prefix: None,
        dns: None,
        image: common::TEST_IMAGE.to_string(),
        command_args: vec![],
        rootfs_override: None,
        image_disk_override: None,
    }
}

/// Test: start_vm returns a valid VmHandle, VM becomes healthy, stop works
#[tokio::test(flavor = "multi_thread")]
async fn test_library_api_start_vm_and_stop() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("api-start");
    let args = test_run_args(&vm_name);
    let mut handle = start_vm(args).await?;

    println!(
        "start_vm returned: vm_id={}, name={}, pid={}",
        handle.vm_id, handle.name, handle.pid
    );

    assert!(!handle.vm_id.is_empty(), "vm_id should not be empty");
    assert_eq!(handle.name, vm_name);
    assert!(handle.pid > 0, "pid should be positive");

    // Wait for VM to become healthy
    common::poll_health_by_pid(handle.pid, 300).await?;
    println!("VM is healthy!");

    // Stop gracefully
    let exit_code = handle.stop().await?;
    assert!(
        exit_code.is_none(),
        "expected None exit code on cancel, got {:?}",
        exit_code
    );

    println!("VM stopped successfully");
    Ok(())
}

/// Test: run_exec_in_vm_captured returns captured stdout/stderr
#[tokio::test(flavor = "multi_thread")]
async fn test_library_api_exec_captured() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("api-exec");
    let args = test_run_args(&vm_name);

    let mut handle = start_vm(args).await?;
    println!("VM started: vm_id={}, pid={}", handle.vm_id, handle.pid);

    common::poll_health_by_pid(handle.pid, 300).await?;
    println!("VM healthy");

    let vsock = handle.vsock_socket_path();

    // Run a command that produces stdout
    let output = exec::run_exec_in_vm_captured(
        &vsock,
        &["echo".into(), "hello-api".into()],
        true, // in_container
        &handle.vm_id,
    )
    .await?;

    assert_eq!(output.exit_code, 0, "echo should exit 0");
    assert!(
        output.stdout.contains("hello-api"),
        "stdout should contain 'hello-api', got: {:?}",
        output.stdout
    );

    // Run a command that produces stderr and non-zero exit
    let output = exec::run_exec_in_vm_captured(
        &vsock,
        &["sh".into(), "-c".into(), "echo errmsg >&2; exit 42".into()],
        true,
        &handle.vm_id,
    )
    .await?;

    assert_eq!(output.exit_code, 42, "should exit 42");
    assert!(
        output.stderr.contains("errmsg"),
        "stderr should contain 'errmsg', got: {:?}",
        output.stderr
    );

    handle.stop().await?;
    Ok(())
}

/// Test: start_exec_session_async completes the ACK/GO handshake and returns a
/// usable tokio stream carrying post-GO exec responses (the WS bridge use case)
#[tokio::test(flavor = "multi_thread")]
async fn test_library_api_async_connection() -> Result<()> {
    use tokio::io::AsyncReadExt;

    let (vm_name, _, _, _) = common::unique_names("api-conn");
    let args = test_run_args(&vm_name);

    let mut handle = start_vm(args).await?;
    println!("VM started: vm_id={}, pid={}", handle.vm_id, handle.pid);

    common::poll_health_by_pid(handle.pid, 300).await?;
    println!("VM healthy");

    let vsock = handle.vsock_socket_path();

    // Start an async exec session (for WS bridge use case). The handshake
    // (request → ACK → GO) happens inside; the returned stream carries only
    // the exec responses.
    let request = exec::ExecRequest {
        command: vec!["echo".into(), "async-session".into()],
        in_container: true,
        interactive: false,
        tty: false,
    };
    let (mut stream, _guard) =
        exec::start_exec_session_async(&vsock, request, &handle.vm_id).await?;

    // Non-TTY responses are JSON lines; the agent closes after Exit, so read
    // to EOF and check both the output and the exit message arrived.
    let mut responses = String::new();
    stream.read_to_string(&mut responses).await?;
    assert!(
        responses.contains("async-session"),
        "responses should contain the echoed output, got: {responses:?}"
    );
    assert!(
        responses.contains("\"exit\""),
        "responses should contain an exit message, got: {responses:?}"
    );

    handle.stop().await?;
    Ok(())
}
