//! Test Ctrl+C (SIGINT) handling when sent via terminal
//!
//! This test verifies that pressing Ctrl+C in a terminal properly sends SIGINT
//! to fcvm and the signal is handled correctly.
//!
//! The key difference from test_exec.rs is that we send the interrupt character
//! (^C, 0x03) through the PTY master, which causes the kernel to send SIGINT
//! to the foreground process group - exactly like pressing Ctrl+C in a real terminal.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use nix::pty::openpty;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{close, dup2, fork, ForkResult};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::time::Duration;

/// Test that Ctrl+C (sent via PTY) reaches fcvm and triggers signal handling
#[tokio::test]
async fn test_ctrlc_via_terminal() -> Result<()> {
    println!("\nTest: Ctrl+C via terminal (PTY)");
    println!("================================");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("ctrlc-term");

    // Build command args - use bridged mode for reliable testing
    let args = [
        fcvm_path.to_str().unwrap(),
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "alpine:latest",
        "sleep",
        "120", // Long sleep so we can interrupt it
    ];

    println!("Running: {} {}", args[0], args[1..].join(" "));

    // Create PTY pair
    let pty = openpty(None, None).context("opening PTY")?;

    // Set up terminal attributes on slave BEFORE fork
    // Enable ISIG so Ctrl+C generates SIGINT
    unsafe {
        let slave_fd = pty.slave.as_raw_fd();
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave_fd, &mut termios) == 0 {
            // Enable ISIG so interrupt character generates SIGINT
            termios.c_lflag |= libc::ISIG;
            // Set VINTR to ^C (0x03) - this is the default but be explicit
            termios.c_cc[libc::VINTR] = 0x03;
            libc::tcsetattr(slave_fd, libc::TCSANOW, &termios);
        }
    }

    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // Fork: child runs fcvm, parent sends Ctrl+C
    let child_pid = match unsafe { fork() }.context("forking")? {
        ForkResult::Child => {
            // Child: set up PTY as controlling terminal and run fcvm
            unsafe {
                // Create new session - this makes us the session leader
                libc::setsid();

                // Set the PTY slave as our controlling terminal
                // TIOCSCTTY with arg 0 means "make this my controlling terminal"
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);

                // Make ourselves the foreground process group
                libc::tcsetpgrp(slave_fd, libc::getpid());

                // Redirect stdio to PTY slave
                dup2(slave_fd, 0).ok();
                dup2(slave_fd, 1).ok();
                dup2(slave_fd, 2).ok();

                // Close original fds
                if slave_fd > 2 {
                    close(slave_fd).ok();
                }
                close(master_fd).ok();
            }

            // Exec fcvm
            use std::ffi::CString;
            let prog = CString::new(args[0]).unwrap();
            let c_args: Vec<CString> = args.iter().map(|s| CString::new(*s).unwrap()).collect();
            nix::unistd::execvp(&prog, &c_args).expect("execvp failed");
            unreachable!()
        }
        ForkResult::Parent { child } => {
            // Close slave in parent
            close(slave_fd).ok();
            child
        }
    };

    // Parent: wait for VM to start, then send Ctrl+C via PTY
    let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };

    // Set non-blocking for reads
    unsafe {
        let flags = libc::fcntl(master_fd, libc::F_GETFL);
        libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // CRITICAL: Enable ISIG on the master AFTER child has set up
    // The child's setsid() + TIOCSCTTY may affect terminal settings.
    // Without ISIG, writing 0x03 to PTY won't generate SIGINT.
    std::thread::sleep(Duration::from_millis(500)); // Let child set up
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(master_fd, &mut termios) == 0 {
            termios.c_lflag |= libc::ISIG;
            termios.c_cc[libc::VINTR] = 0x03;
            libc::tcsetattr(master_fd, libc::TCSANOW, &termios);
            println!("  Enabled ISIG on PTY master");
        }
    }

    // Wait for "Container ready" in PTY output
    println!("Waiting for container to be ready...");
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut vm_started = false;

    let mut last_print = std::time::Instant::now();
    while std::time::Instant::now() < deadline {
        // Drain PTY output (non-blocking)
        match master.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                output.extend_from_slice(&buf[..n]);
                let output_str = String::from_utf8_lossy(&output);
                // "Container ready notification received" appears when container is up
                if output_str.contains("Container ready") {
                    vm_started = true;
                    println!("  Container ready!");
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }

        // Print progress every 10 seconds
        if last_print.elapsed() > Duration::from_secs(10) {
            println!("  ... waiting ({} bytes output)", output.len());
            last_print = std::time::Instant::now();
        }

        // Check if child exited early
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => {
                println!("  Child exited early with code {}", code);
                let output_str = String::from_utf8_lossy(&output);
                println!("  Output so far: {}", output_str);
                anyhow::bail!("fcvm exited before VM started");
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                anyhow::bail!("fcvm killed by signal {:?} before VM started", sig);
            }
            _ => {}
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    if !vm_started {
        // Kill child and fail
        kill(child_pid, Signal::SIGKILL).ok();
        let output_str = String::from_utf8_lossy(&output);
        println!("Output so far:\n{}", output_str);
        anyhow::bail!("Timeout waiting for VM to start");
    }

    // Give it a moment to fully initialize
    std::thread::sleep(Duration::from_secs(2));

    // Send Ctrl+C (0x03) through the PTY
    // This is exactly what happens when you press Ctrl+C in a real terminal
    println!("Sending Ctrl+C (0x03) via PTY...");
    master.write_all(&[0x03]).context("writing Ctrl+C")?;
    master.flush().context("flushing")?;

    // Wait for fcvm to handle the signal and exit
    println!("Waiting for fcvm to handle signal and exit...");
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);
    let mut exit_status = None;

    loop {
        // Continue reading output
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }

        // Check if child exited
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => {
                println!("  fcvm exited with code {}", code);
                exit_status = Some(WaitStatus::Exited(child_pid, code));
                break;
            }
            Ok(WaitStatus::Signaled(_, sig, core)) => {
                println!("  fcvm killed by signal {:?}", sig);
                exit_status = Some(WaitStatus::Signaled(child_pid, sig, core));
                break;
            }
            _ => {}
        }

        if start.elapsed() > timeout {
            println!("TIMEOUT: fcvm didn't exit after Ctrl+C");
            kill(child_pid, Signal::SIGKILL).ok();
            anyhow::bail!("Timeout waiting for fcvm to handle Ctrl+C");
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    let output_str = String::from_utf8_lossy(&output);
    println!("\n=== Output ===\n{}", output_str);

    // Check exit status - fcvm should exit cleanly (code 0) after Ctrl+C
    let exited_cleanly = matches!(exit_status, Some(WaitStatus::Exited(_, 0)));

    println!("\n=== Results ===");
    println!("Exit status: {:?}", exit_status);
    println!("Clean exit after Ctrl+C: {}", exited_cleanly);

    if exited_cleanly {
        println!("✓ SUCCESS: Ctrl+C via PTY caused clean exit!");
        Ok(())
    } else {
        anyhow::bail!(
            "FAILURE: fcvm did not exit cleanly after Ctrl+C. Status: {:?}",
            exit_status
        )
    }
}
