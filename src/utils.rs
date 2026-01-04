//! Utility functions for process management and system operations.

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};

/// Check if a process is alive by checking /proc/{pid} existence.
///
/// This is more reliable than sending signal 0 because it doesn't require
/// any special permissions - any user can check if /proc/{pid} exists.
///
/// # Arguments
/// * `pid` - Process ID to check
///
/// # Returns
/// `true` if the process exists, `false` otherwise
pub fn is_process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{}", pid)).exists()
}

/// Gracefully kill a process by sending SIGTERM first, then SIGKILL if needed.
///
/// This allows the process to run cleanup handlers (network teardown, file cleanup, etc.)
/// before being forcefully terminated.
///
/// # Arguments
/// * `pid` - Process ID to kill
/// * `timeout_ms` - Maximum time to wait for graceful shutdown (in milliseconds)
///
/// # Behavior
/// 1. Sends SIGTERM to allow graceful shutdown
/// 2. Waits up to `timeout_ms` for process to exit
/// 3. Sends SIGKILL if still running
pub fn graceful_kill(pid: u32, timeout_ms: u64) {
    // Send SIGTERM first for graceful shutdown
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output();

    // Wait for process to exit gracefully
    let interval = Duration::from_millis(100);
    let iterations = (timeout_ms / 100).max(1);

    for _ in 0..iterations {
        if !is_process_alive(pid) {
            return; // Process exited gracefully
        }
        thread::sleep(interval);
    }

    // Force kill if still running
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .output();
}

/// Async version of graceful_kill for use in async contexts.
///
/// Same behavior as `graceful_kill` but uses tokio for sleeping.
pub async fn graceful_kill_async(pid: u32, timeout_ms: u64) {
    // Send SIGTERM first for graceful shutdown
    let _ = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .await;

    // Wait for process to exit gracefully
    let interval = tokio::time::Duration::from_millis(100);
    let iterations = (timeout_ms / 100).max(1);

    for _ in 0..iterations {
        if !is_process_alive(pid) {
            return; // Process exited gracefully
        }
        tokio::time::sleep(interval).await;
    }

    // Force kill if still running
    let _ = tokio::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .output()
        .await;
}

/// Strip Firecracker timestamp and instance prefix from log lines.
/// Input:  "2025-11-15T17:18:55.027478889 [anonymous-instance:main] message"
/// Output: "message"
pub fn strip_firecracker_prefix(line: &str) -> &str {
    let mut result = line;

    // Strip timestamp if present (starts with year like "20XX-")
    if let Some(pos) = result.find(' ') {
        if result.starts_with("20") && result.chars().nth(4) == Some('-') {
            result = &result[pos + 1..];
        }
    }

    // Strip [anonymous-instance:xxx] prefix if present
    if result.starts_with('[') {
        if let Some(end_pos) = result.find("] ") {
            result = &result[end_pos + 2..];
        }
    }

    result
}

/// Spawn a command and stream its output via tracing logs.
///
/// Takes a closure that receives each line and logs it appropriately.
/// Returns the child process handle (caller must manage lifecycle).
pub fn spawn_streaming<F>(
    mut cmd: tokio::process::Command,
    log_line: F,
) -> anyhow::Result<tokio::process::Child>
where
    F: Fn(&str, bool) + Send + Sync + Clone + 'static,
{
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    // Stream stdout (is_stderr=false)
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let log = log_line.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                log(&line, false);
            }
        });
    }

    // Stream stderr (is_stderr=true)
    if let Some(stderr) = child.stderr.take() {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        let log = log_line;
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                log(&line, true);
            }
        });
    }

    Ok(child)
}

/// Run a command and stream its output via tracing at INFO/WARN level.
///
/// Simple version with a prefix. For custom logging logic, use spawn_streaming.
pub async fn run_streaming(
    cmd: tokio::process::Command,
    prefix: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    let prefix = prefix.to_string();
    let mut child = spawn_streaming(cmd, move |line, is_stderr| {
        if is_stderr {
            warn!("[{}] {}", prefix, line);
        } else {
            info!("[{}] {}", prefix, line);
        }
    })?;
    Ok(child.wait().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_alive_current_process() {
        // Current process should always be alive
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn test_is_process_alive_nonexistent() {
        // PID 4294967295 (u32::MAX) is extremely unlikely to exist
        assert!(!is_process_alive(u32::MAX));
    }

    #[test]
    fn test_is_process_alive_init() {
        // PID 1 (init/systemd) should always exist on Linux
        assert!(is_process_alive(1));
    }
}
