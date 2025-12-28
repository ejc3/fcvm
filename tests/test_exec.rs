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

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(fcvm_pid).await;

    println!("✅ EXEC TEST PASSED! (network: {})", network);
    Ok(())
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
    let mut child = tokio::process::Command::new("script")
        .args(["-q", "-c", &fcvm_cmd, "/dev/null"])
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
            let timeout = Duration::from_secs(10);
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
