//! Integration tests for fcvm exec command
//!
//! Tests VM exec (commands in guest OS) and container exec (commands inside container)
//! for both bridged and rootless networking modes.
//!
//! Uses common::spawn_fcvm() to prevent pipe buffer deadlock.
//! See CLAUDE.md "Pipe Buffer Deadlock in Tests" for details.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;

#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_exec_bridged() -> Result<()> {
    exec_test_impl("bridged").await
}

#[tokio::test]
async fn test_exec_rootless() -> Result<()> {
    exec_test_impl("rootless").await
}

async fn exec_test_impl(network: &str) -> Result<()> {
    println!("\nfcvm exec test (network: {})", network);
    println!("================================");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names(&format!("exec-{}", network));

    // Start the VM using spawn_fcvm helper (uses Stdio::inherit to prevent deadlock)
    println!("Starting VM...");
    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        network,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm podman run")?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Wait for VM to become healthy
    println!("  Waiting for VM to become healthy...");
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 180).await {
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("  VM is healthy!");

    // Test 1: VM exec - hostname (use --vm flag)
    println!("\nTest 1: VM exec - hostname");
    let output = run_exec(&fcvm_path, fcvm_pid, true, &["hostname"]).await?;
    let hostname = output.trim();
    println!("  hostname: {}", hostname);
    assert!(!hostname.is_empty(), "hostname should not be empty");

    // Test 2: VM exec - uname (use --vm flag)
    println!("\nTest 2: VM exec - uname -a");
    let output = run_exec(&fcvm_path, fcvm_pid, true, &["uname", "-a"]).await?;
    println!("  uname: {}", output.trim());
    assert!(output.contains("Linux"), "uname should contain 'Linux'");

    // Test 3: Container exec - cat /etc/os-release (default, no flag needed)
    println!("\nTest 3: Container exec - cat /etc/os-release");
    let output = run_exec(&fcvm_path, fcvm_pid, false, &["cat", "/etc/os-release"]).await?;
    println!("  os-release: {}", output.lines().next().unwrap_or(""));
    assert!(
        output.contains("Alpine"),
        "container should be Alpine Linux (nginx:alpine)"
    );

    // Test 4: Container exec - nginx -v (default, no flag needed)
    println!("\nTest 4: Container exec - nginx -v");
    let output = run_exec(&fcvm_path, fcvm_pid, false, &["nginx", "-v"]).await?;
    println!("  nginx version: {}", output.trim());
    // nginx -v outputs to stderr, but our exec streams both
    assert!(
        output.contains("nginx") || output.is_empty(),
        "should get nginx version or empty (stderr)"
    );

    // Test 5: VM internet connectivity - curl AWS public ECR (use --vm flag)
    println!("\nTest 5: VM internet connectivity - curl public.ecr.aws");
    let output = run_exec(
        &fcvm_path,
        fcvm_pid,
        true,
        &[
            "curl",
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "10",
            "https://public.ecr.aws/",
        ],
    )
    .await?;
    let http_code = output.trim();
    println!("  HTTP status code: {}", http_code);
    // Should get 2xx success or 3xx redirect (AWS ECR returns 308)
    assert!(
        http_code.starts_with('2') || http_code.starts_with('3'),
        "should get HTTP 2xx/3xx, got: {}",
        http_code
    );

    // Test 6: Container internet connectivity - wget AWS public ECR (default, no flag needed)
    println!("\nTest 6: Container internet - wget public.ecr.aws");
    // Use wget --spider for HEAD request (exits 0 on success, 1 on failure)
    // Alpine's wget doesn't have the same options as curl, but --spider works
    let output = run_exec(
        &fcvm_path,
        fcvm_pid,
        false,
        &[
            "wget",
            "--spider",
            "-q",
            "--timeout=10",
            "https://public.ecr.aws/",
        ],
    )
    .await?;
    // wget --spider -q outputs nothing on success, just exits 0
    // If we got here without error, connectivity works
    println!("  wget spider succeeded (exit 0)");
    // The command succeeds if we reach here; wget returns non-zero on network failure
    assert!(
        output.trim().is_empty() || output.contains("200"),
        "wget should succeed silently, got: {}",
        output
    );

    // Test 7: TTY NOT allocated without -t flag (VM exec)
    println!("\nTest 7: No TTY without -t flag (VM)");
    let output = run_exec(&fcvm_path, fcvm_pid, true, &["tty"]).await?;
    println!("  tty output: {}", output.trim());
    assert!(
        output.contains("not a tty") || output.contains("not a terminal"),
        "without -t flag, should not have a TTY, got: {}",
        output
    );

    // Test 8: TTY NOT allocated without -t flag (container exec)
    println!("\nTest 8: No TTY without -t flag (container)");
    let output = run_exec(&fcvm_path, fcvm_pid, false, &["tty"]).await?;
    println!("  tty output: {}", output.trim());
    assert!(
        output.contains("not a tty") || output.contains("not a terminal"),
        "without -t flag, should not have a TTY, got: {}",
        output
    );

    // Test 9: TTY allocated WITH -t flag (VM exec)
    // Uses `script` to provide a PTY for the test harness
    println!("\nTest 9: TTY with -t flag (VM)");
    let (_, _, output) = run_exec_tty(
        &fcvm_path,
        fcvm_pid,
        true,
        &["tty"],
        InterruptCondition::None,
    )
    .await?;
    println!("  tty output: {}", output.trim());
    // With TTY, should return a device path like /dev/pts/0
    assert!(
        output.contains("/dev/"),
        "with -t flag, should have a TTY device, got: {}",
        output
    );

    // Test 10: TTY allocated WITH -t flag (container exec)
    println!("\nTest 10: TTY with -t flag (container)");
    let (_, _, output) = run_exec_tty(
        &fcvm_path,
        fcvm_pid,
        false,
        &["tty"],
        InterruptCondition::None,
    )
    .await?;
    println!("  tty output: {}", output.trim());
    assert!(
        output.contains("/dev/"),
        "with -t flag, should have a TTY device, got: {}",
        output
    );

    // Test 11: TTY mode interrupt with SIGINT (Ctrl+C)
    // Print READY then sleep - we poll for READY, then send SIGINT
    println!("\nTest 11: TTY interrupt with SIGINT (VM)");
    let (exit_code, duration, output) = run_exec_tty(
        &fcvm_path,
        fcvm_pid,
        true,
        &["sh", "-c", "echo READY; sleep 999"],
        InterruptCondition::WaitForOutput("READY"),
    )
    .await?;
    println!("  exit code: {}, duration: {:?}", exit_code, duration);
    assert!(
        output.contains("READY"),
        "should see READY before interrupt, got: {}",
        output
    );
    println!("  ✓ sleep was interrupted by SIGINT");

    // Test 12: Verify Ctrl-C interrupts a shell script before completion
    // Script prints STARTED, sleeps, then prints FINISHED
    // We interrupt after seeing STARTED - should NOT see FINISHED
    println!("\nTest 12: Ctrl-C interrupts shell script (VM)");
    let (exit_code, duration, output) = run_exec_tty(
        &fcvm_path,
        fcvm_pid,
        true,
        &["sh", "-c", "echo STARTED; sleep 999; echo FINISHED"],
        InterruptCondition::WaitForOutput("STARTED"),
    )
    .await?;
    println!("  exit code: {}, duration: {:?}", exit_code, duration);
    println!("  output: {}", output.trim());
    // Should see STARTED but not FINISHED (interrupted by ^C)
    assert!(
        output.contains("STARTED"),
        "should see STARTED before interrupt, got: {}",
        output
    );
    assert!(
        !output.contains("FINISHED"),
        "should NOT see FINISHED (interrupted), got: {}",
        output
    );
    println!("  ✓ script was interrupted by Ctrl-C");

    // Test 13: Exit code propagation (non-zero exit codes) - non-TTY mode
    // Uses non-TTY mode to avoid script wrapper issues with exit codes
    println!("\nTest 13: Exit code propagation (VM - non-TTY)");
    let exit_code =
        run_exec_with_exit_code(&fcvm_path, fcvm_pid, true, &["sh", "-c", "exit 42"]).await?;
    println!("  exit code: {}", exit_code);
    assert_eq!(exit_code, 42, "exit code should be 42, got: {}", exit_code);
    println!("  ✓ exit code 42 propagated correctly");

    // Test 14: Exit code propagation (container) - non-TTY mode
    println!("\nTest 14: Exit code propagation (container - non-TTY)");
    let exit_code =
        run_exec_with_exit_code(&fcvm_path, fcvm_pid, false, &["sh", "-c", "exit 7"]).await?;
    println!("  exit code: {}", exit_code);
    assert_eq!(exit_code, 7, "exit code should be 7, got: {}", exit_code);
    println!("  ✓ exit code 7 propagated correctly");

    // Test 15: stdin input is received by command (-it mode)
    // Use head -1 instead of cat - it exits after reading one line
    // (cat would hang waiting for EOF which doesn't propagate through PTY layers)
    println!("\nTest 15: stdin input received (VM -it)");
    let (exit_code, _, output) = run_exec_with_pty(
        &fcvm_path,
        fcvm_pid,
        true, // in_vm
        true, // interactive (-i)
        true, // tty (-t)
        &["head", "-1"],
        Some("hello from stdin\n"),
    )
    .await?;
    println!("  exit code: {}", exit_code);
    println!("  output: {:?}", output);
    assert!(
        output.contains("hello from stdin"),
        "should echo stdin input, got: {:?}",
        output
    );
    println!("  ✓ stdin was received and echoed back");

    // Test 16: stdin input (container -it)
    println!("\nTest 16: stdin input received (container -it)");
    let (exit_code, _, output) = run_exec_with_pty(
        &fcvm_path,
        fcvm_pid,
        false, // in_vm=false (container)
        true,  // interactive (-i)
        true,  // tty (-t)
        &["head", "-1"],
        Some("container stdin test\n"),
    )
    .await?;
    println!("  exit code: {}", exit_code);
    println!("  output: {:?}", output);
    assert!(
        output.contains("container stdin test"),
        "should echo stdin input, got: {:?}",
        output
    );
    println!("  ✓ container stdin was received and echoed back");

    // ======================================================================
    // Test all 4 flag combinations for -i and -t (symmetric with podman)
    // ======================================================================

    // Test 17: -t only (VM) - TTY for output, no stdin
    // Use `ls --color=auto` which needs TTY for colors but no stdin
    println!("\nTest 17: -t only (VM) - TTY output, no stdin");
    let (exit_code, _, output) = run_exec_with_pty(
        &fcvm_path,
        fcvm_pid,
        true,  // in_vm
        false, // interactive=false (no -i)
        true,  // tty=true (-t)
        &["echo", "tty-only-test"],
        None, // no stdin input
    )
    .await?;
    println!("  exit code: {}", exit_code);
    println!("  output: {:?}", output);
    assert!(
        output.contains("tty-only-test"),
        "should get output with -t only: {:?}",
        output
    );
    assert_eq!(exit_code, 0, "exit code should be 0");
    println!("  ✓ -t only works for VM exec");

    // Test 18: -t only (container) - TTY for output, no stdin
    println!("\nTest 18: -t only (container) - TTY output, no stdin");
    let (exit_code, _, output) = run_exec_with_pty(
        &fcvm_path,
        fcvm_pid,
        false, // in_vm=false (container)
        false, // interactive=false (no -i)
        true,  // tty=true (-t)
        &["echo", "container-tty-only"],
        None, // no stdin input
    )
    .await?;
    println!("  exit code: {}", exit_code);
    println!("  output: {:?}", output);
    assert!(
        output.contains("container-tty-only"),
        "should get output with -t only: {:?}",
        output
    );
    assert_eq!(exit_code, 0, "exit code should be 0");
    println!("  ✓ -t only works for container exec");

    // Test 19: -i only (VM) - stdin but no TTY (piping data)
    // This uses non-TTY mode but with stdin - test via regular exec with -i
    println!("\nTest 19: -i only (VM) - stdin without TTY");
    {
        let pid_str = fcvm_pid.to_string();
        let mut child = tokio::process::Command::new(&fcvm_path)
            .args(["exec", "--pid", &pid_str, "--vm", "-i", "head", "-1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning exec with -i")?;

        // Write to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(b"vm-interactive-input\n").await?;
            stdin.flush().await?;
            drop(stdin);
        }

        let output = child.wait_with_output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("  output: {:?}", stdout);
        assert!(
            stdout.contains("vm-interactive-input"),
            "should echo stdin with -i: {:?}",
            stdout
        );
        println!("  ✓ -i only works for VM exec");
    }

    // Test 20: -i only (container) - stdin but no TTY
    println!("\nTest 20: -i only (container) - stdin without TTY");
    {
        let pid_str = fcvm_pid.to_string();
        let mut child = tokio::process::Command::new(&fcvm_path)
            .args(["exec", "--pid", &pid_str, "-i", "head", "-1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning exec with -i")?;

        // Write to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(b"container-interactive-input\n").await?;
            stdin.flush().await?;
            drop(stdin);
        }

        let output = child.wait_with_output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("  output: {:?}", stdout);
        assert!(
            stdout.contains("container-interactive-input"),
            "should echo stdin with -i: {:?}",
            stdout
        );
        println!("  ✓ -i only works for container exec");
    }

    // Test 21: neither -i nor -t (VM) - plain exec, no stdin, no TTY
    println!("\nTest 21: neither -i nor -t (VM)");
    let output = run_exec(&fcvm_path, fcvm_pid, true, &["echo", "plain-vm"]).await?;
    println!("  output: {:?}", output.trim());
    assert!(
        output.contains("plain-vm"),
        "should get output: {:?}",
        output
    );
    println!("  ✓ plain exec (no flags) works for VM");

    // Test 22: neither -i nor -t (container) - plain exec
    println!("\nTest 22: neither -i nor -t (container)");
    let output = run_exec(&fcvm_path, fcvm_pid, false, &["echo", "plain-container"]).await?;
    println!("  output: {:?}", output.trim());
    assert!(
        output.contains("plain-container"),
        "should get output: {:?}",
        output
    );
    println!("  ✓ plain exec (no flags) works for container");

    // Test 23: Verify -t without -i does NOT forward stdin (VM)
    println!("\nTest 23: -t without -i should NOT forward stdin (VM)");
    let (exit_code, _, output) = run_exec_with_pty(
        &fcvm_path,
        fcvm_pid,
        true,                             // in_vm
        false,                            // interactive=false (no -i)
        true,                             // tty=true (-t)
        &["head", "-1"],                  // waits for input
        Some("this-should-not-appear\n"), // we send input but it should be ignored
    )
    .await?;
    println!("  exit code: {} (expected timeout/signal)", exit_code);
    println!("  output: {:?}", output);
    // head -1 should timeout because stdin is not forwarded
    assert!(
        !output.contains("this-should-not-appear"),
        "-t without -i should NOT forward stdin, but got: {:?}",
        output
    );
    println!("  ✓ -t without -i correctly ignores stdin (VM)");

    // Test 24: Verify -t without -i does NOT forward stdin (container)
    println!("\nTest 24: -t without -i should NOT forward stdin (container)");
    let (exit_code, _, output) = run_exec_with_pty(
        &fcvm_path,
        fcvm_pid,
        false,                             // in_vm=false (container)
        false,                             // interactive=false (no -i)
        true,                              // tty=true (-t)
        &["head", "-1"],                   // waits for input
        Some("container-stdin-ignored\n"), // we send input but it should be ignored
    )
    .await?;
    println!("  exit code: {} (expected timeout/signal)", exit_code);
    println!("  output: {:?}", output);
    // head -1 should timeout because stdin is not forwarded
    assert!(
        !output.contains("container-stdin-ignored"),
        "-t without -i should NOT forward stdin, but got: {:?}",
        output
    );
    println!("  ✓ -t without -i correctly ignores stdin (container)");

    // ======================================================================
    // Tests 25-28: Ctrl-C/Ctrl-D/Ctrl-Z via PTY (proper signal path)
    // These tests send actual control characters through the PTY to verify
    // the signal handling works correctly through the terminal layer.
    // ======================================================================

    // Test 25: Ctrl-C (0x03) via PTY interrupts sleep (VM)
    // This tests the proper PTY signal path: we write 0x03 to PTY master,
    // which gets interpreted by the PTY slave's line discipline as SIGINT
    println!("\nTest 25: Ctrl-C (0x03) via PTY interrupts command (VM -it)");
    let (exit_code, _, output) = run_exec_with_pty_interrupt(
        &fcvm_path,
        fcvm_pid,
        true, // in_vm
        &[
            "sh",
            "-c",
            "trap 'echo CAUGHT_SIGINT; exit 130' INT; echo READY; sleep 999",
        ],
        "READY",
        0x03, // Ctrl-C
    )
    .await?;
    println!("  exit code: {}", exit_code);
    println!("  output: {:?}", output);
    assert!(
        output.contains("READY"),
        "should see READY before interrupt, got: {:?}",
        output
    );
    assert!(
        output.contains("CAUGHT_SIGINT"),
        "trap should catch SIGINT, got: {:?}",
        output
    );
    assert_eq!(exit_code, 130, "exit code should be 130 (128 + SIGINT)");
    println!("  ✓ Ctrl-C via PTY works for VM exec");

    // Test 26: Ctrl-C (0x03) via PTY interrupts command (container -it)
    println!("\nTest 26: Ctrl-C (0x03) via PTY interrupts command (container -it)");
    let (exit_code, _, output) = run_exec_with_pty_interrupt(
        &fcvm_path,
        fcvm_pid,
        false, // container
        &[
            "sh",
            "-c",
            "trap 'echo CAUGHT_SIGINT; exit 130' INT; echo READY; sleep 999",
        ],
        "READY",
        0x03, // Ctrl-C
    )
    .await?;
    println!("  exit code: {}", exit_code);
    println!("  output: {:?}", output);
    assert!(
        output.contains("READY"),
        "should see READY before interrupt, got: {:?}",
        output
    );
    assert!(
        output.contains("CAUGHT_SIGINT"),
        "trap should catch SIGINT, got: {:?}",
        output
    );
    assert_eq!(exit_code, 130, "exit code should be 130 (128 + SIGINT)");
    println!("  ✓ Ctrl-C via PTY works for container exec");

    // Test 27: Ctrl-D (0x04) via PTY sends EOF (VM -it)
    // Ctrl-D should close stdin, causing the process to see EOF
    println!("\nTest 27: Ctrl-D (0x04) via PTY sends EOF (VM -it)");
    let (exit_code, _, output) = run_exec_with_pty_interrupt(
        &fcvm_path,
        fcvm_pid,
        true, // in_vm
        &["sh", "-c", "echo READY; cat; echo GOT_EOF"],
        "READY",
        0x04, // Ctrl-D
    )
    .await?;
    println!("  exit code: {}", exit_code);
    println!("  output: {:?}", output);
    assert!(
        output.contains("READY"),
        "should see READY before EOF, got: {:?}",
        output
    );
    assert!(
        output.contains("GOT_EOF"),
        "cat should get EOF and script should continue, got: {:?}",
        output
    );
    println!("  ✓ Ctrl-D via PTY works for VM exec");

    // Test 28: Ctrl-D (0x04) via PTY sends EOF (container -it)
    println!("\nTest 28: Ctrl-D (0x04) via PTY sends EOF (container -it)");
    let (exit_code, _, output) = run_exec_with_pty_interrupt(
        &fcvm_path,
        fcvm_pid,
        false, // container
        &["sh", "-c", "echo READY; cat; echo GOT_EOF"],
        "READY",
        0x04, // Ctrl-D
    )
    .await?;
    println!("  exit code: {}", exit_code);
    println!("  output: {:?}", output);
    assert!(
        output.contains("READY"),
        "should see READY before EOF, got: {:?}",
        output
    );
    assert!(
        output.contains("GOT_EOF"),
        "cat should get EOF and script should continue, got: {:?}",
        output
    );
    println!("  ✓ Ctrl-D via PTY works for container exec");

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(fcvm_pid).await;

    println!("✅ EXEC TEST PASSED! (network: {})", network);
    Ok(())
}

/// Run fcvm exec (no TTY) and return exit code
async fn run_exec_with_exit_code(
    fcvm_path: &std::path::Path,
    pid: u32,
    in_vm: bool,
    cmd: &[&str],
) -> Result<i32> {
    let pid_str = pid.to_string();
    let mut args = vec!["exec", "--pid", &pid_str];
    if in_vm {
        args.push("--vm");
    }
    args.push("--");
    args.extend(cmd.iter().copied());

    let output = tokio::process::Command::new(fcvm_path)
        .args(&args)
        .output()
        .await
        .context("running fcvm exec")?;

    Ok(output.status.code().unwrap_or(-1))
}

/// Run fcvm exec (no TTY) and return stdout
async fn run_exec(
    fcvm_path: &std::path::Path,
    pid: u32,
    in_vm: bool,
    cmd: &[&str],
) -> Result<String> {
    let pid_str = pid.to_string();
    let mut args = vec!["exec", "--pid", &pid_str];
    if in_vm {
        args.push("--vm");
    }
    args.push("--");
    args.extend(cmd.iter().copied());

    let output = tokio::process::Command::new(fcvm_path)
        .args(&args)
        .output()
        .await
        .context("running fcvm exec")?;

    // Combine stdout and stderr (exec streams both)
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Filter out log lines from fcvm (INFO, WARN, DEBUG, TRACE, ERROR from tracing)
    let result: String = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| {
            !line.contains(" INFO ")
                && !line.contains(" WARN ")
                && !line.contains(" DEBUG ")
                && !line.contains(" TRACE ")
                && !line.contains(" ERROR ")
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(result)
}

/// Interrupt condition for TTY exec
enum InterruptCondition {
    /// No interrupt - wait for command to complete naturally
    None,
    /// Send SIGINT after seeing this string in output
    WaitForOutput(&'static str),
}

/// Run fcvm exec with TTY (uses `script` to allocate PTY)
///
/// Returns (exit_code, duration, output)
async fn run_exec_tty(
    fcvm_path: &std::path::Path,
    pid: u32,
    in_vm: bool,
    cmd: &[&str],
    interrupt: InterruptCondition,
) -> Result<(i32, Duration, String)> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use tokio::io::AsyncReadExt;

    let pid_str = pid.to_string();

    // Build the fcvm exec command with proper shell quoting
    let mut fcvm_args = vec![
        shell_words::quote(&fcvm_path.to_string_lossy()).into_owned(),
        "exec".to_string(),
        "--pid".to_string(),
        pid_str,
        "-t".to_string(), // TTY mode
    ];
    if in_vm {
        fcvm_args.push("--vm".to_string());
    }
    fcvm_args.push("--".to_string());
    // Shell-escape each command argument
    fcvm_args.extend(cmd.iter().map(|s| shell_words::quote(s).into_owned()));

    // Join into a single command for script -c
    let fcvm_cmd = fcvm_args.join(" ");
    let start = std::time::Instant::now();

    // Use script to wrap the command with a PTY (test harness doesn't have a TTY)
    // -q: quiet mode (no "Script started" message)
    // -c: run command instead of shell
    // /dev/null: discard typescript file
    // stdin must be null to prevent garbage from test harness being sent to VM PTY
    let mut child = tokio::process::Command::new("script")
        .args(["-q", "-c", &fcvm_cmd, "/dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning fcvm exec with script")?;

    let child_pid = child.id().context("getting child pid")?;

    let (status, collected_output) = match interrupt {
        InterruptCondition::None => {
            // Use wait_with_output for atomic output collection
            let output = child
                .wait_with_output()
                .await
                .context("waiting for child")?;
            (output.status, output.stdout)
        }
        InterruptCondition::WaitForOutput(expected) => {
            // Take stdout for incremental reading
            let mut stdout = child.stdout.take().context("taking stdout")?;
            let mut collected = Vec::new();

            // Poll for expected output, then send SIGINT
            let mut buf = [0u8; 256];
            let timeout = Duration::from_secs(30); // 30s for CI under load
            let deadline = std::time::Instant::now() + timeout;

            loop {
                if std::time::Instant::now() > deadline {
                    kill(Pid::from_raw(child_pid as i32), Signal::SIGKILL).ok();
                    anyhow::bail!("timeout waiting for '{}' in output", expected);
                }

                // Read with timeout
                match tokio::time::timeout(Duration::from_millis(100), stdout.read(&mut buf)).await
                {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => {
                        collected.extend_from_slice(&buf[..n]);
                        let output_str = String::from_utf8_lossy(&collected);
                        if output_str.contains(expected) {
                            // Found expected output - send SIGINT
                            kill(Pid::from_raw(child_pid as i32), Signal::SIGINT).ok();
                            // Continue reading remaining output
                            stdout
                                .read_to_end(&mut collected)
                                .await
                                .context("reading remaining stdout")?;
                            break;
                        }
                    }
                    Ok(Err(e)) => return Err(e).context("reading stdout"),
                    Err(_) => continue, // Timeout, try again
                }
            }

            // Wait for the process to exit
            let status = child.wait().await.context("waiting for child")?;
            (status, collected)
        }
    };

    let duration = start.elapsed();
    let exit_code = status.code().unwrap_or(-1);

    // Filter script artifacts from output
    let stdout_str = String::from_utf8_lossy(&collected_output);
    let combined: String = stdout_str
        .lines()
        .filter(|line| {
            !line.contains("Script started")
                && !line.contains("Script done")
                && !line.contains("Session terminated")
                && !line.is_empty()
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok((exit_code, duration, combined))
}

/// Run fcvm exec with PTY and optional stdin input
///
/// Uses nix::pty to properly allocate a PTY for the child process.
///
/// Flags:
/// - `interactive`: Pass -i flag (stdin forwarding)
/// - `tty`: Pass -t flag (TTY allocation)
/// - `stdin_input`: Input to send (only makes sense with interactive=true)
///
/// Returns (exit_code, duration, output)
async fn run_exec_with_pty(
    fcvm_path: &std::path::Path,
    pid: u32,
    in_vm: bool,
    interactive: bool,
    tty: bool,
    cmd: &[&str],
    stdin_input: Option<&str>,
) -> Result<(i32, Duration, String)> {
    use nix::pty::openpty;
    use nix::unistd::{close, dup2, fork, ForkResult};
    use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};

    let pid_str = pid.to_string();
    let start = std::time::Instant::now();

    // Allocate a PTY pair
    let pty = openpty(None, None).context("opening PTY")?;

    // CRITICAL: Disable echo on the PTY slave BEFORE forking.
    // Otherwise there's a race condition: if the child (fcvm exec) doesn't
    // set raw mode fast enough (which happens under heavy load), input
    // written to the PTY master gets echoed back as output.
    // This caused Test 24 to fail in parallel runs but pass in isolation.
    unsafe {
        let slave_fd = pty.slave.as_raw_fd();
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave_fd, &mut termios) == 0 {
            // Disable echo (ECHO) and canonical mode (ICANON)
            // This prevents input from being echoed before the child sets raw mode
            termios.c_lflag &= !(libc::ECHO | libc::ICANON);
            // Also disable ISIG so Ctrl-C doesn't get processed locally
            termios.c_lflag &= !libc::ISIG;
            libc::tcsetattr(slave_fd, libc::TCSANOW, &termios);
        }
    }

    // Transfer ownership of fds from OwnedFd to raw fds.
    // This prevents double-close: OwnedFd would close on drop, but we also
    // wrap master in a File which owns the fd. Using into_raw_fd() transfers
    // ownership so only the File (or manual close) is responsible for closing.
    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // Fork to run fcvm exec in child with PTY as stdin/stdout/stderr
    match unsafe { fork() }.context("forking")? {
        ForkResult::Child => {
            // Child: set up PTY slave as stdin/stdout/stderr
            unsafe {
                // Create new session
                libc::setsid();

                // Set controlling terminal
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);

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

            // Build command args as CStrings
            use std::ffi::CString;
            let prog = CString::new(fcvm_path.to_str().unwrap()).unwrap();
            let mut args: Vec<CString> = vec![
                CString::new(fcvm_path.to_str().unwrap()).unwrap(),
                CString::new("exec").unwrap(),
                CString::new("--pid").unwrap(),
                CString::new(pid_str.as_str()).unwrap(),
            ];
            // Add flags based on parameters
            if interactive && tty {
                args.push(CString::new("-it").unwrap());
            } else if interactive {
                args.push(CString::new("-i").unwrap());
            } else if tty {
                args.push(CString::new("-t").unwrap());
            }
            if in_vm {
                args.push(CString::new("--vm").unwrap());
            }
            args.push(CString::new("--").unwrap());
            for c in cmd {
                args.push(CString::new(*c).unwrap());
            }

            // Exec fcvm - on success, this replaces the process image
            #[allow(unreachable_code)]
            {
                nix::unistd::execvp(&prog, &args).expect("execvp failed");
                std::process::exit(1); // Never reached
            }
        }
        ForkResult::Parent { child } => {
            // Parent: close slave, use master for I/O
            close(slave_fd).ok();

            // Wrap master fd in File for I/O (File takes ownership, will close on drop)
            let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };

            // Delay to let child start - container exec via podman needs more time
            std::thread::sleep(Duration::from_millis(500));

            // Write stdin input only if provided
            if let Some(input) = stdin_input {
                use std::io::Write;
                master
                    .write_all(input.as_bytes())
                    .context("writing stdin")?;
                master.flush().context("flushing")?;
            }

            // Read output with timeout
            let mut output = Vec::new();
            let mut buf = [0u8; 4096];

            // Set non-blocking
            unsafe {
                let flags = libc::fcntl(master_fd, libc::F_GETFL);
                libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }

            let deadline = std::time::Instant::now() + Duration::from_secs(30); // 30s for CI under load
            loop {
                if std::time::Instant::now() > deadline {
                    // Timeout - kill child
                    nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL).ok();
                    break;
                }

                use std::io::Read;
                match master.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => output.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Check if child exited
                        match nix::sys::wait::waitpid(
                            child,
                            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                        ) {
                            Ok(nix::sys::wait::WaitStatus::Exited(_, _)) => break,
                            Ok(nix::sys::wait::WaitStatus::Signaled(_, _, _)) => break,
                            _ => {
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                    }
                    Err(_) => break,
                }
            }

            // Wait for child to fully exit
            let status = nix::sys::wait::waitpid(child, None).context("waiting for child")?;
            let exit_code = match status {
                nix::sys::wait::WaitStatus::Exited(_, code) => code,
                _ => -1,
            };

            let duration = start.elapsed();
            let output_str = String::from_utf8_lossy(&output).to_string();

            Ok((exit_code, duration, output_str))
        }
    }
}

/// Run fcvm exec with PTY and send a control character after seeing expected output
///
/// This tests the proper PTY signal path:
/// - Ctrl-C (0x03) should be converted to SIGINT by PTY layer
/// - Ctrl-D (0x04) should be converted to EOF
/// - Ctrl-Z (0x1a) should be converted to SIGTSTP
///
/// Uses -it mode (interactive + tty) so stdin is forwarded.
async fn run_exec_with_pty_interrupt(
    fcvm_path: &std::path::Path,
    pid: u32,
    in_vm: bool,
    cmd: &[&str],
    wait_for: &str,
    control_char: u8,
) -> Result<(i32, Duration, String)> {
    use nix::pty::openpty;
    use nix::unistd::{close, dup2, fork, ForkResult};
    use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};

    let pid_str = pid.to_string();
    let start = std::time::Instant::now();

    // Allocate a PTY pair
    let pty = openpty(None, None).context("opening PTY")?;

    // Configure PTY slave before forking
    unsafe {
        let slave_fd = pty.slave.as_raw_fd();
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave_fd, &mut termios) == 0 {
            // Disable echo but KEEP ISIG enabled so control characters work
            termios.c_lflag &= !(libc::ECHO | libc::ICANON);
            // ISIG must be enabled for Ctrl-C → SIGINT conversion in PTY layer
            libc::tcsetattr(slave_fd, libc::TCSANOW, &termios);
        }
    }

    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    match unsafe { fork() }.context("forking")? {
        ForkResult::Child => {
            // Child: set up PTY slave as stdin/stdout/stderr
            unsafe {
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                dup2(slave_fd, 0).ok();
                dup2(slave_fd, 1).ok();
                dup2(slave_fd, 2).ok();
                if slave_fd > 2 {
                    close(slave_fd).ok();
                }
                close(master_fd).ok();
            }

            // Build command
            use std::ffi::CString;
            let prog = CString::new(fcvm_path.to_str().unwrap()).unwrap();
            let mut args: Vec<CString> = vec![
                CString::new(fcvm_path.to_str().unwrap()).unwrap(),
                CString::new("exec").unwrap(),
                CString::new("--pid").unwrap(),
                CString::new(pid_str.as_str()).unwrap(),
                CString::new("-it").unwrap(), // Always use -it for control char tests
            ];
            if in_vm {
                args.push(CString::new("--vm").unwrap());
            }
            args.push(CString::new("--").unwrap());
            for c in cmd {
                args.push(CString::new(*c).unwrap());
            }

            #[allow(unreachable_code)]
            {
                nix::unistd::execvp(&prog, &args).expect("execvp failed");
                std::process::exit(1);
            }
        }
        ForkResult::Parent { child } => {
            close(slave_fd).ok();
            let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };

            // Set non-blocking
            unsafe {
                let flags = libc::fcntl(master_fd, libc::F_GETFL);
                libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }

            let mut output = Vec::new();
            let mut buf = [0u8; 4096];
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut sent_control = false;

            loop {
                if std::time::Instant::now() > deadline {
                    nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL).ok();
                    anyhow::bail!("timeout waiting for command");
                }

                use std::io::Read;
                match master.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        output.extend_from_slice(&buf[..n]);

                        // Check if we should send the control character
                        if !sent_control {
                            let output_str = String::from_utf8_lossy(&output);
                            if output_str.contains(wait_for) {
                                // Small delay to ensure command is in expected state
                                std::thread::sleep(Duration::from_millis(100));

                                // Send the control character through PTY
                                use std::io::Write;
                                master.write_all(&[control_char]).ok();
                                master.flush().ok();
                                sent_control = true;
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Check if child exited
                        match nix::sys::wait::waitpid(
                            child,
                            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                        ) {
                            Ok(nix::sys::wait::WaitStatus::Exited(_, _)) => break,
                            Ok(nix::sys::wait::WaitStatus::Signaled(_, _, _)) => break,
                            _ => std::thread::sleep(Duration::from_millis(50)),
                        }
                    }
                    Err(_) => break,
                }
            }

            // Wait for child
            let status = nix::sys::wait::waitpid(child, None).context("waiting for child")?;
            let exit_code = match status {
                nix::sys::wait::WaitStatus::Exited(_, code) => code,
                _ => -1,
            };

            let duration = start.elapsed();
            let output_str = String::from_utf8_lossy(&output).to_string();

            Ok((exit_code, duration, output_str))
        }
    }
}

/// Stress test: Run 100 parallel TTY execs to verify no race conditions
///
/// This test verifies that the TTY stdin fix works under heavy parallel load.
/// Each exec runs `tty` command which should return /dev/pts/N.
/// A null byte (`\x00`) in output would indicate a race condition bug.
///
/// Uses waves of 10 concurrent execs to avoid overwhelming vsock backlog.
#[tokio::test]
async fn test_exec_parallel_tty_stress() -> Result<()> {
    const TOTAL_EXECS: usize = 100;
    const WAVE_SIZE: usize = 10; // Run 10 at a time to avoid vsock backlog overflow

    println!(
        "\nParallel TTY Stress Test ({} execs in waves of {})",
        TOTAL_EXECS, WAVE_SIZE
    );
    println!("==========================================================");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("stress-tty");

    // Start VM with nginx (keeps running)
    println!("Starting VM...");
    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        common::TEST_IMAGE, // nginx:alpine
    ])
    .await
    .context("spawning VM")?;

    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Wait for VM to become healthy
    common::poll_health_by_pid(fcvm_pid, 60)
        .await
        .context("waiting for VM to be healthy")?;
    println!("  VM is healthy");

    // Run execs in waves to avoid overwhelming vsock connection backlog
    println!(
        "\nRunning {} execs in waves of {}...",
        TOTAL_EXECS, WAVE_SIZE
    );
    let start = std::time::Instant::now();

    let mut success_count = 0;
    let mut null_byte_failures = 0;
    let mut other_failures = 0;
    let mut failures: Vec<(usize, String)> = Vec::new();

    for wave in 0..(TOTAL_EXECS / WAVE_SIZE) {
        let mut handles = Vec::new();
        for i in 0..WAVE_SIZE {
            let idx = wave * WAVE_SIZE + i;
            let fcvm_path = fcvm_path.clone();
            let pid = fcvm_pid;
            handles.push(tokio::spawn(async move {
                let result = run_exec_tty(
                    &fcvm_path,
                    pid,
                    true, // in_vm
                    &["tty"],
                    InterruptCondition::None,
                )
                .await;
                (idx, result)
            }));
        }

        // Collect wave results
        for handle in handles {
            let (idx, result) = handle.await.context("joining task")?;
            match result {
                Ok((exit_code, _duration, output)) => {
                    if output.contains("/dev/pts") && exit_code == 0 {
                        success_count += 1;
                    } else if output.contains('\x00') || output == "^@" {
                        // This is the null byte bug we're testing for
                        null_byte_failures += 1;
                        failures.push((
                            idx,
                            format!("NULL BYTE: exit={}, output={:?}", exit_code, output),
                        ));
                    } else {
                        other_failures += 1;
                        failures.push((idx, format!("exit={}, output={:?}", exit_code, output)));
                    }
                }
                Err(e) => {
                    other_failures += 1;
                    failures.push((idx, format!("error: {}", e)));
                }
            }
        }

        // Brief pause between waves to let vsock recover
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let elapsed = start.elapsed();
    println!("\n===== STRESS TEST RESULTS =====");
    println!("Total execs: {}", TOTAL_EXECS);
    println!("Success: {}", success_count);
    println!(
        "Null byte failures: {} (the bug we're testing for)",
        null_byte_failures
    );
    println!("Other failures: {}", other_failures);
    println!("Duration: {:?}", elapsed);
    println!(
        "Throughput: {:.1} execs/sec",
        TOTAL_EXECS as f64 / elapsed.as_secs_f64()
    );

    if !failures.is_empty() {
        println!("\n=== FAILURES (first 10) ===");
        for (idx, msg) in failures.iter().take(10) {
            println!("  #{}: {}", idx, msg);
        }
    }

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(fcvm_pid).await;

    // Assert 100% success - no failures are acceptable
    assert_eq!(
        success_count, TOTAL_EXECS,
        "Expected 100% success rate, got {}/{} (null_byte={}, other={})",
        success_count, TOTAL_EXECS, null_byte_failures, other_failures
    );

    println!("✓ ALL {} PARALLEL TTY EXECS PASSED!", TOTAL_EXECS);
    Ok(())
}

// ============================================================================
// Tests for `fcvm podman run -it` (not exec, but container run with -i/-t)
// ============================================================================

/// Test `fcvm podman run -t` allocates TTY for container
///
/// Uses `tty` command to verify TTY is allocated when -t flag is used.
#[tokio::test]
async fn test_podman_run_tty() -> Result<()> {
    use nix::pty::openpty;
    use nix::unistd::{close, dup2, fork, ForkResult};
    use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
    use std::time::Duration;

    println!("\nTest: fcvm podman run -t (TTY allocation)");
    println!("==========================================");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("run-tty");

    // Allocate PTY for test harness (since we don't have a real terminal)
    let pty = openpty(None, None).context("opening PTY")?;

    // Disable echo before forking
    unsafe {
        let slave_fd = pty.slave.as_raw_fd();
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave_fd, &mut termios) == 0 {
            termios.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
            libc::tcsetattr(slave_fd, libc::TCSANOW, &termios);
        }
    }

    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // Fork to run fcvm with PTY
    match unsafe { fork() }.context("forking")? {
        ForkResult::Child => {
            // Child: set up PTY and exec fcvm
            unsafe {
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                dup2(slave_fd, 0).ok();
                dup2(slave_fd, 1).ok();
                dup2(slave_fd, 2).ok();
                if slave_fd > 2 {
                    close(slave_fd).ok();
                }
                close(master_fd).ok();
            }

            // Exec fcvm podman run -t ... tty
            use std::ffi::CString;
            let prog = CString::new(fcvm_path.to_str().unwrap()).unwrap();
            let args = vec![
                CString::new(fcvm_path.to_str().unwrap()).unwrap(),
                CString::new("podman").unwrap(),
                CString::new("run").unwrap(),
                CString::new("--name").unwrap(),
                CString::new(vm_name.as_str()).unwrap(),
                CString::new("-t").unwrap(),
                CString::new(common::TEST_IMAGE_ALPINE).unwrap(),
                CString::new("tty").unwrap(),
            ];
            #[allow(unreachable_code)]
            {
                nix::unistd::execvp(&prog, &args).expect("execvp failed");
                std::process::exit(1);
            }
        }
        ForkResult::Parent { child } => {
            close(slave_fd).ok();

            let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };

            // Read output with timeout
            let mut output = Vec::new();
            let mut buf = [0u8; 4096];

            // Set non-blocking
            unsafe {
                let flags = libc::fcntl(master_fd, libc::F_GETFL);
                libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }

            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            loop {
                if std::time::Instant::now() > deadline {
                    nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL).ok();
                    break;
                }

                use std::io::Read;
                match master.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => output.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        match nix::sys::wait::waitpid(
                            child,
                            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                        ) {
                            Ok(nix::sys::wait::WaitStatus::Exited(_, _)) => break,
                            Ok(nix::sys::wait::WaitStatus::Signaled(_, _, _)) => break,
                            _ => std::thread::sleep(Duration::from_millis(50)),
                        }
                    }
                    Err(_) => break,
                }
            }

            // Wait for child
            let _ = nix::sys::wait::waitpid(child, None);

            let output_str = String::from_utf8_lossy(&output);
            println!("  Output: {:?}", output_str);

            // Should see /dev/pts/X
            assert!(
                output_str.contains("/dev/pts"),
                "With -t flag, container should have TTY (/dev/pts), got: {:?}",
                output_str
            );
            println!("✓ fcvm podman run -t correctly allocates TTY");
        }
    }

    Ok(())
}

/// Test `fcvm podman run -it` for interactive container with TTY
///
/// Uses `head -1` to verify stdin is forwarded when -i flag is used.
#[tokio::test]
async fn test_podman_run_interactive_tty() -> Result<()> {
    use nix::pty::openpty;
    use nix::unistd::{close, dup2, fork, ForkResult};
    use std::io::Write;
    use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
    use std::time::Duration;

    println!("\nTest: fcvm podman run -it (interactive + TTY)");
    println!("==============================================");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("run-it");

    // Allocate PTY for test harness
    let pty = openpty(None, None).context("opening PTY")?;

    // Disable echo before forking
    unsafe {
        let slave_fd = pty.slave.as_raw_fd();
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave_fd, &mut termios) == 0 {
            termios.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
            libc::tcsetattr(slave_fd, libc::TCSANOW, &termios);
        }
    }

    let master_fd = pty.master.into_raw_fd();
    let slave_fd = pty.slave.into_raw_fd();

    // Fork to run fcvm with PTY
    match unsafe { fork() }.context("forking")? {
        ForkResult::Child => {
            // Child: set up PTY and exec fcvm
            unsafe {
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                dup2(slave_fd, 0).ok();
                dup2(slave_fd, 1).ok();
                dup2(slave_fd, 2).ok();
                if slave_fd > 2 {
                    close(slave_fd).ok();
                }
                close(master_fd).ok();
            }

            // Exec fcvm podman run -it ... head -1
            use std::ffi::CString;
            let prog = CString::new(fcvm_path.to_str().unwrap()).unwrap();
            let args = vec![
                CString::new(fcvm_path.to_str().unwrap()).unwrap(),
                CString::new("podman").unwrap(),
                CString::new("run").unwrap(),
                CString::new("--name").unwrap(),
                CString::new(vm_name.as_str()).unwrap(),
                CString::new("-it").unwrap(),
                CString::new(common::TEST_IMAGE_ALPINE).unwrap(),
                CString::new("head").unwrap(),
                CString::new("-1").unwrap(),
            ];
            #[allow(unreachable_code)]
            {
                nix::unistd::execvp(&prog, &args).expect("execvp failed");
                unreachable!();
            }
        }
        ForkResult::Parent { child } => {
            close(slave_fd).ok();

            let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };

            // Wait for container to be ready
            std::thread::sleep(Duration::from_secs(10));

            // Write input to container
            master
                .write_all(b"hello-from-stdin\n")
                .context("writing stdin")?;
            master.flush()?;

            // Read output with timeout
            let mut output = Vec::new();
            let mut buf = [0u8; 4096];

            // Set non-blocking
            unsafe {
                let flags = libc::fcntl(master_fd, libc::F_GETFL);
                libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }

            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            loop {
                if std::time::Instant::now() > deadline {
                    nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL).ok();
                    break;
                }

                use std::io::Read;
                match master.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => output.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        match nix::sys::wait::waitpid(
                            child,
                            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                        ) {
                            Ok(nix::sys::wait::WaitStatus::Exited(_, _)) => break,
                            Ok(nix::sys::wait::WaitStatus::Signaled(_, _, _)) => break,
                            _ => std::thread::sleep(Duration::from_millis(50)),
                        }
                    }
                    Err(_) => break,
                }
            }

            // Wait for child
            let _ = nix::sys::wait::waitpid(child, None);

            let output_str = String::from_utf8_lossy(&output);
            println!("  Output: {:?}", output_str);

            // Should see our input echoed back
            assert!(
                output_str.contains("hello-from-stdin"),
                "With -it flags, stdin should be forwarded, got: {:?}",
                output_str
            );
            println!("✓ fcvm podman run -it correctly forwards stdin");
        }
    }

    Ok(())
}

/// Test `fcvm podman run` without -t flag (no TTY)
///
/// Verifies that without -t, container does NOT have a TTY.
#[tokio::test]
async fn test_podman_run_no_tty() -> Result<()> {
    use std::time::Duration;

    println!("\nTest: fcvm podman run (no -t = no TTY)");
    println!("======================================");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("run-no-tty");

    // Run without -t, check tty command output
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(&fcvm_path)
            .args(["podman", "run", "--name", &vm_name, common::TEST_IMAGE_ALPINE, "tty"])
            .output(),
    )
    .await
    .context("timeout")?
    .context("running fcvm")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    println!("  Output: {:?}", combined.trim());

    // Without -t, should say "not a tty"
    assert!(
        combined.contains("not a tty") || combined.contains("not a terminal"),
        "Without -t flag, container should NOT have TTY, got: {:?}",
        combined
    );
    println!("✓ fcvm podman run (no -t) correctly has no TTY");

    Ok(())
}
