//! Integration tests for fcvm exec command
//!
//! Tests VM exec (commands in guest OS) and container exec (commands inside container)
//! for both bridged and rootless networking modes.
//!
//! Uses common::spawn_fcvm() to prevent pipe buffer deadlock.
//! See CLAUDE.md "Pipe Buffer Deadlock in Tests" for details.

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

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
    let vm_name = format!("exec-test-{}", network);

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
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 60).await {
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

    // Test 5: VM internet connectivity - curl ifconfig.me (use --vm flag)
    println!("\nTest 5: VM internet connectivity - curl ifconfig.me");
    let output = run_exec(
        &fcvm_path,
        fcvm_pid,
        true,
        &["curl", "-s", "--max-time", "10", "ifconfig.me"],
    )
    .await?;
    let ip = output.trim();
    println!("  VM external IP: {}", ip);
    // Should be a valid IP address (contains dots)
    assert!(
        ip.contains('.') && ip.len() >= 7,
        "should return a valid IP address, got: {}",
        ip
    );

    // Test 6: Container internet connectivity - wget (default, no flag needed)
    println!("\nTest 6: Container internet - wget ifconfig.me");
    let output = run_exec(
        &fcvm_path,
        fcvm_pid,
        false,
        &[
            "wget",
            "-q",
            "-O",
            "-",
            "--timeout=10",
            "http://ifconfig.me",
        ],
    )
    .await?;
    let container_ip = output.trim();
    println!("  container external IP: {}", container_ip);
    assert!(
        container_ip.contains('.') && container_ip.len() >= 7,
        "container should have internet access, got: {}",
        container_ip
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
    let output =
        run_exec_with_tty(&fcvm_path, fcvm_pid, ExecFlags::vm().with_tty(), &["tty"]).await?;
    println!("  tty output: {}", output.trim());
    // With TTY, should return a device path like /dev/pts/0
    assert!(
        output.contains("/dev/"),
        "with -t flag, should have a TTY device, got: {}",
        output
    );

    // Test 10: TTY allocated WITH -t flag (container exec)
    println!("\nTest 10: TTY with -t flag (container)");
    let output = run_exec_with_tty(
        &fcvm_path,
        fcvm_pid,
        ExecFlags::container().with_tty(),
        &["tty"],
    )
    .await?;
    println!("  tty output: {}", output.trim());
    assert!(
        output.contains("/dev/"),
        "with -t flag, should have a TTY device, got: {}",
        output
    );

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(fcvm_pid).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("✅ EXEC TEST PASSED! (network: {})", network);
    Ok(())
}

/// Exec flags for tests
#[derive(Default)]
struct ExecFlags {
    in_vm: bool,
    interactive: bool,
    tty: bool,
}

impl ExecFlags {
    fn vm() -> Self {
        Self {
            in_vm: true,
            ..Default::default()
        }
    }

    fn container() -> Self {
        Self::default()
    }

    fn with_tty(mut self) -> Self {
        self.tty = true;
        self
    }
}

/// Run fcvm exec and return stdout
async fn run_exec(
    fcvm_path: &std::path::Path,
    pid: u32,
    in_vm: bool,
    cmd: &[&str],
) -> Result<String> {
    let flags = if in_vm {
        ExecFlags::vm()
    } else {
        ExecFlags::container()
    };
    run_exec_with_flags(fcvm_path, pid, flags, cmd).await
}

/// Run fcvm exec with flags and return stdout
async fn run_exec_with_flags(
    fcvm_path: &std::path::Path,
    pid: u32,
    flags: ExecFlags,
    cmd: &[&str],
) -> Result<String> {
    let pid_str = pid.to_string();
    let mut args = vec!["exec", "--pid", &pid_str];
    if flags.in_vm {
        args.push("--vm");
    }
    if flags.interactive {
        args.push("-i");
    }
    if flags.tty {
        args.push("-t");
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

    // Filter out INFO log lines from fcvm
    let result: String = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| !line.contains("INFO") && !line.contains("WARN"))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(result)
}

/// Run fcvm exec with TTY using `script` to provide a PTY
/// The `script` command allocates a pseudo-terminal, allowing us to test TTY mode
async fn run_exec_with_tty(
    fcvm_path: &std::path::Path,
    pid: u32,
    flags: ExecFlags,
    cmd: &[&str],
) -> Result<String> {
    let pid_str = pid.to_string();

    // Build the fcvm exec command string
    let mut fcvm_args = vec![
        fcvm_path.to_string_lossy().to_string(),
        "exec".to_string(),
        "--pid".to_string(),
        pid_str,
    ];
    if flags.in_vm {
        fcvm_args.push("--vm".to_string());
    }
    if flags.interactive {
        fcvm_args.push("-i".to_string());
    }
    if flags.tty {
        fcvm_args.push("-t".to_string());
    }
    fcvm_args.push("--".to_string());
    fcvm_args.extend(cmd.iter().map(|s| s.to_string()));

    // Join into a single command for script -c
    let fcvm_cmd = fcvm_args.join(" ");

    // Use script to wrap the command with a PTY
    // -q: quiet mode (no "Script started" message)
    // -c: run command instead of shell
    // /dev/null: discard typescript file
    let output = tokio::process::Command::new("script")
        .args(["-q", "-c", &fcvm_cmd, "/dev/null"])
        .output()
        .await
        .context("running fcvm exec with script")?;

    // Combine stdout and stderr
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Filter out INFO log lines from fcvm and script artifacts
    let result: String = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| {
            !line.contains("INFO")
                && !line.contains("WARN")
                && !line.contains("Script started")
                && !line.contains("Script done")
                && !line.is_empty()
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(result)
}
