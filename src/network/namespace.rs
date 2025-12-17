use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;
use tracing::{debug, warn};

/// Creates a named network namespace
///
/// This uses `ip netns add` to create a persistent namespace in /var/run/netns/.
/// The namespace will survive even if no processes are in it.
pub async fn create_namespace(ns_name: &str) -> Result<()> {
    debug!(namespace = %ns_name, "creating network namespace");

    let output = Command::new("sudo")
        .args(["ip", "netns", "add", ns_name])
        .output()
        .await
        .context("executing ip netns add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" error - namespace already created
        if stderr.contains("File exists") {
            warn!(namespace = %ns_name, "namespace already exists, reusing");
            return Ok(());
        }
        anyhow::bail!("failed to create namespace {}: {}", ns_name, stderr);
    }

    Ok(())
}

/// Deletes a named network namespace
///
/// Removes the namespace via `ip netns del`. This will fail if processes
/// are still running in the namespace.
pub async fn delete_namespace(ns_name: &str) -> Result<()> {
    debug!(namespace = %ns_name, "deleting network namespace");

    let output = Command::new("sudo")
        .args(["ip", "netns", "del", ns_name])
        .output()
        .await
        .context("executing ip netns del")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "No such file" error - namespace already gone
        if stderr.contains("Cannot remove") || stderr.contains("No such file") {
            warn!(namespace = %ns_name, "namespace doesn't exist or already deleted");
            return Ok(());
        }
        anyhow::bail!("failed to delete namespace {}: {}", ns_name, stderr);
    }

    Ok(())
}

/// Checks if a namespace exists
pub async fn namespace_exists(ns_name: &str) -> bool {
    let ns_path = format!("/var/run/netns/{}", ns_name);
    Path::new(&ns_path).exists()
}

/// Executes a command inside a network namespace
///
/// Wrapper around `ip netns exec` for running commands in an isolated namespace.
/// Returns the command output.
pub async fn exec_in_namespace(ns_name: &str, command: &[&str]) -> Result<std::process::Output> {
    if command.is_empty() {
        anyhow::bail!("command cannot be empty");
    }

    let mut args = vec!["ip", "netns", "exec", ns_name];
    args.extend_from_slice(command);

    let output = Command::new("sudo")
        .args(&args)
        .output()
        .await
        .with_context(|| format!("executing command in namespace {}: {:?}", ns_name, command))?;

    Ok(output)
}

/// Lists all network namespaces
#[allow(dead_code)]
pub async fn list_namespaces() -> Result<Vec<String>> {
    let output = Command::new("ip")
        .args(["netns", "list"])
        .output()
        .await
        .context("executing ip netns list")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to list namespaces: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let namespaces: Vec<String> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            // Format is "name (id: N)" or just "name"
            line.split_whitespace().next().unwrap_or("").to_string()
        })
        .collect();

    Ok(namespaces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_namespace_lifecycle() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("Skipping test_namespace_lifecycle - requires root");
            return;
        }

        let ns_name = "fcvm-test-ns";

        // Clean up if exists from previous test
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        create_namespace(ns_name).await.unwrap();
        assert!(namespace_exists(ns_name).await);

        // Creating again should be idempotent
        create_namespace(ns_name).await.unwrap();

        // Delete namespace
        delete_namespace(ns_name).await.unwrap();
        assert!(!namespace_exists(ns_name).await);

        // Deleting again should be idempotent
        delete_namespace(ns_name).await.unwrap();
    }

    #[tokio::test]
    async fn test_exec_in_namespace() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("Skipping test_exec_in_namespace - requires root");
            return;
        }

        let ns_name = "fcvm-test-exec";

        // Clean up if exists
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        create_namespace(ns_name).await.unwrap();

        // Execute command in namespace
        let output = exec_in_namespace(ns_name, &["ip", "link", "show"])
            .await
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Should at least have loopback interface
        assert!(stdout.contains("lo:"));

        // Cleanup
        delete_namespace(ns_name).await.unwrap();
    }
}
