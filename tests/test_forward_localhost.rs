//! Test --forward-localhost flag: VM localhost reaches host services.
//!
//! Starts a TCP server on host 127.0.0.1, runs a VM with --forward-localhost
//! that connects to localhost:port from inside the container.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::io::Write;
use std::net::TcpListener;

/// Test that --forward-localhost makes container's 127.0.0.1 reach host services.
#[tokio::test]
async fn test_forward_localhost() -> Result<()> {
    println!("\nTest --forward-localhost");
    println!("========================");

    // Start a TCP server on host 127.0.0.1 (only reachable via loopback)
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    println!("  Host server on 127.0.0.1:{}", port);

    // Accept one connection in background (with timeout)
    let accept_handle = std::thread::spawn(move || -> bool {
        listener.set_nonblocking(false).expect("set_nonblocking");
        // 15s accept timeout
        unsafe {
            let tv = libc::timeval {
                tv_sec: 15,
                tv_usec: 0,
            };
            libc::setsockopt(
                std::os::unix::io::AsRawFd::as_raw_fd(&listener),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of_val(&tv) as u32,
            );
        }
        match listener.accept() {
            Ok((mut conn, _)) => {
                let _ = conn.write_all(b"HELLO_FROM_HOST\n");
                true
            }
            Err(_) => false,
        }
    });

    let port_str = port.to_string();
    let (vm_name, _, _, _) = common::unique_names("fwd-localhost");

    // Run container command that connects to localhost:port
    // This matches the exact manual test that works
    let fcvm_path = common::find_fcvm_binary()?;
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--forward-localhost",
            &port_str,
            "--no-snapshot",
            common::TEST_IMAGE,
            "--",
            "sh",
            "-c",
            &format!("nc -w5 127.0.0.1 {} 2>&1 || echo FAILED", port),
        ])
        .output()
        .await
        .context("running fcvm")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("  stdout: {}", stdout.trim());

    let accepted = accept_handle.join().unwrap_or(false);
    println!("  server accepted: {}", accepted);

    assert!(
        stdout.contains("HELLO_FROM_HOST"),
        "VM localhost should reach host (got stdout={}, stderr={})",
        stdout.trim(),
        &stderr[..std::cmp::min(200, stderr.len())]
    );

    println!("✅ FORWARD LOCALHOST TEST PASSED!");
    Ok(())
}
