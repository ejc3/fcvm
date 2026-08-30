use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, warn};

/// Directory `ip netns` keeps its named namespaces in.
const NETNS_DIR: &str = "/var/run/netns";

/// What `create_namespace` found.
///
/// A separate outcome rather than an error so a caller searching for a free
/// name can move on, and so no caller can mistake "somebody else's namespace"
/// for "mine".
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceCreation {
    /// This call created the namespace.
    Created,
    /// The name was already taken; this call created nothing.
    AlreadyExists,
}

/// The file `ip netns` uses to represent a named namespace.
pub fn namespace_path(ns_name: &str) -> PathBuf {
    Path::new(NETNS_DIR).join(ns_name)
}

/// Creates a named network namespace in /var/run/netns/.
///
/// The namespace survives even if no processes are in it.
///
/// An existing name is reported as `AlreadyExists`, never adopted. `ip netns
/// add` fails with EEXIST, which makes it the compare-and-swap that decides
/// which VM owns a name. Adopting instead put two VMs' interfaces in one
/// namespace: the second VM's veth or TAP creation failed on a name the first
/// VM already held, and its cleanup then deleted the namespace the first VM
/// was still running in (#888).
pub async fn create_namespace(ns_name: &str) -> Result<NamespaceCreation> {
    debug!(namespace = %ns_name, "creating network namespace");

    let output = Command::new("ip")
        .args(["netns", "add", ns_name])
        .output()
        .await
        .context("executing ip netns add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("File exists") {
            debug!(namespace = %ns_name, "namespace name is taken by another VM");
            return Ok(NamespaceCreation::AlreadyExists);
        }
        anyhow::bail!("failed to create namespace {}: {}", ns_name, stderr);
    }

    Ok(NamespaceCreation::Created)
}

/// Deletes a named network namespace
///
/// Removes the namespace via `ip netns del`. This will fail if processes
/// are still running in the namespace.
pub async fn delete_namespace(ns_name: &str) -> Result<()> {
    debug!(namespace = %ns_name, "deleting network namespace");

    let output = Command::new("ip")
        .args(["netns", "del", ns_name])
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
    namespace_path(ns_name).exists()
}

/// Executes a command inside a network namespace
///
/// Wrapper around `ip netns exec` for running commands in an isolated namespace.
/// Returns the command output.
pub async fn exec_in_namespace(ns_name: &str, command: &[&str]) -> Result<std::process::Output> {
    if command.is_empty() {
        anyhow::bail!("command cannot be empty");
    }

    let mut args = vec!["netns", "exec", ns_name];
    args.extend_from_slice(command);

    let output = Command::new("ip")
        .args(&args)
        .output()
        .await
        .with_context(|| format!("executing command in namespace {}: {:?}", ns_name, command))?;

    Ok(output)
}

/// Executes a command inside a network namespace and fails on non-zero exit.
///
/// Like [`exec_in_namespace`], but bails with the command, exit status, and
/// stderr when the inner command exits non-zero. Use for configuration
/// commands whose failure must not be silently ignored. Callers that need to
/// inspect the output themselves should use [`exec_in_namespace`] directly.
pub async fn exec_in_namespace_checked(ns_name: &str, command: &[&str]) -> Result<()> {
    let output = exec_in_namespace(ns_name, command).await?;
    if !output.status.success() {
        anyhow::bail!(
            "command {:?} in namespace {} failed ({}): {}",
            command,
            ns_name,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
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

#[cfg(all(test, feature = "privileged-tests"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_namespace_lifecycle() {
        let ns_name = "fcvm-test-ns";

        // Clean up if exists from previous test
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        assert_eq!(
            create_namespace(ns_name).await.unwrap(),
            NamespaceCreation::Created
        );
        assert!(namespace_exists(ns_name).await);

        // Creating again must report the name as taken, never adopt it (#888)
        assert_eq!(
            create_namespace(ns_name).await.unwrap(),
            NamespaceCreation::AlreadyExists
        );

        // Delete namespace
        delete_namespace(ns_name).await.unwrap();
        assert!(!namespace_exists(ns_name).await);

        // Deleting again should be idempotent
        delete_namespace(ns_name).await.unwrap();
    }

    // Requires CAP_SYS_ADMIN to remount /sys in new namespace (doesn't work in containers)
    #[tokio::test]
    async fn test_exec_in_namespace() {
        let ns_name = "fcvm-test-exec";

        // Clean up if exists
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        assert_eq!(
            create_namespace(ns_name).await.unwrap(),
            NamespaceCreation::Created
        );

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
