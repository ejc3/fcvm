use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

use super::namespace::exec_in_namespace;

/// Creates a veth pair and moves guest side into a namespace
///
/// Creates a pair of virtual ethernet devices. The host side remains in the
/// root namespace, while the guest side is moved into the VM's namespace.
pub async fn create_veth_pair(
    host_veth: &str,
    guest_veth: &str,
    ns_name: &str,
) -> Result<()> {
    info!(
        host = %host_veth,
        guest = %guest_veth,
        namespace = %ns_name,
        "creating veth pair"
    );

    // Create veth pair in root namespace
    let output = Command::new("sudo")
        .args([
            "ip",
            "link",
            "add",
            host_veth,
            "type",
            "veth",
            "peer",
            "name",
            guest_veth,
        ])
        .output()
        .await
        .context("executing ip link add veth")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to create veth pair: {}", stderr);
    }

    // Move guest side into namespace
    let output = Command::new("sudo")
        .args(["ip", "link", "set", guest_veth, "netns", ns_name])
        .output()
        .await
        .context("moving veth to namespace")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Cleanup host veth on failure
        let _ = Command::new("sudo")
            .args(["ip", "link", "del", host_veth])
            .output()
            .await;
        anyhow::bail!("failed to move veth to namespace: {}", stderr);
    }

    Ok(())
}

/// Configures the host side of a veth pair
///
/// Sets up the host-side veth interface with an IP address and brings it up.
pub async fn setup_host_veth(veth_name: &str, ip_with_cidr: &str) -> Result<()> {
    info!(veth = %veth_name, ip = %ip_with_cidr, "configuring host veth");

    // Bring up the interface
    let output = Command::new("sudo")
        .args(["ip", "link", "set", veth_name, "up"])
        .output()
        .await
        .context("bringing up host veth")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to bring up host veth {}: {}", veth_name, stderr);
    }

    // Assign IP address
    let output = Command::new("sudo")
        .args(["ip", "addr", "add", ip_with_cidr, "dev", veth_name])
        .output()
        .await
        .context("assigning IP to host veth")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" - IP already assigned
        if !stderr.contains("File exists") {
            anyhow::bail!("failed to assign IP to host veth {}: {}", veth_name, stderr);
        }
    }

    Ok(())
}

/// Configures the guest side of a veth pair inside a namespace
///
/// Sets up the guest-side veth with an IP address, default route, and brings it up.
/// All operations happen inside the VM's network namespace.
pub async fn setup_guest_veth_in_ns(
    ns_name: &str,
    veth_name: &str,
    ip_with_cidr: &str,
    gateway_ip: &str,
) -> Result<()> {
    info!(
        namespace = %ns_name,
        veth = %veth_name,
        ip = %ip_with_cidr,
        gateway = %gateway_ip,
        "configuring guest veth in namespace"
    );

    // Bring up loopback interface
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", "lo", "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("failed to bring up loopback (may already be up): {}", stderr);
    }

    // Bring up veth interface
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", veth_name, "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to bring up guest veth {} in namespace: {}",
            veth_name,
            stderr
        );
    }

    // Assign IP address
    let output = exec_in_namespace(ns_name, &["ip", "addr", "add", ip_with_cidr, "dev", veth_name])
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" - IP already assigned
        if !stderr.contains("File exists") {
            anyhow::bail!(
                "failed to assign IP to guest veth {} in namespace: {}",
                veth_name,
                stderr
            );
        }
    }

    // Add default route via gateway
    let output = exec_in_namespace(
        ns_name,
        &["ip", "route", "add", "default", "via", gateway_ip],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" - route already exists
        if !stderr.contains("File exists") {
            warn!(
                "failed to add default route in namespace (may already exist): {}",
                stderr
            );
        }
    }

    Ok(())
}

/// Creates a TAP device inside a namespace
///
/// Creates a TAP device that Firecracker will use. The TAP is created inside
/// the VM's namespace so it's isolated from other VMs.
pub async fn create_tap_in_ns(ns_name: &str, tap_name: &str) -> Result<()> {
    info!(
        namespace = %ns_name,
        tap = %tap_name,
        "creating TAP device in namespace"
    );

    // Create TAP device
    let output = exec_in_namespace(
        ns_name,
        &["ip", "tuntap", "add", tap_name, "mode", "tap"],
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to create TAP device {} in namespace: {}",
            tap_name,
            stderr
        );
    }

    // Bring up TAP device
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", tap_name, "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to bring up TAP device {} in namespace: {}",
            tap_name,
            stderr
        );
    }

    Ok(())
}

/// Connects TAP device directly to veth interface inside namespace
///
/// Since we're skipping the bridge (user's choice), we need to route packets
/// between TAP and veth. We do this by enabling IP forwarding in the namespace
/// and setting up proper routing.
pub async fn connect_tap_to_veth(
    ns_name: &str,
    tap_name: &str,
    veth_name: &str,
    tap_ip_with_cidr: &str,
) -> Result<()> {
    info!(
        namespace = %ns_name,
        tap = %tap_name,
        veth = %veth_name,
        tap_ip = %tap_ip_with_cidr,
        "connecting TAP to veth in namespace"
    );

    // Assign IP to TAP device (this will be the guest's IP)
    let output = exec_in_namespace(
        ns_name,
        &["ip", "addr", "add", tap_ip_with_cidr, "dev", tap_name],
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" - IP already assigned
        if !stderr.contains("File exists") {
            anyhow::bail!(
                "failed to assign IP to TAP {} in namespace: {}",
                tap_name,
                stderr
            );
        }
    }

    // Enable IP forwarding in namespace (allows packets between TAP and veth)
    let output = exec_in_namespace(
        ns_name,
        &["sysctl", "-w", "net.ipv4.ip_forward=1"],
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("failed to enable IP forwarding in namespace: {}", stderr);
    }

    // Add route for TAP subnet via veth (so return packets work)
    // This is needed because the guest will send to TAP, but return traffic
    // needs to route back through veth
    let output = exec_in_namespace(
        ns_name,
        &["ip", "route", "add", tap_ip_with_cidr, "dev", tap_name],
    )
    .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" - route may already exist
        if !stderr.contains("File exists") {
            warn!("failed to add TAP route in namespace: {}", stderr);
        }
    }

    Ok(())
}

/// Deletes a veth pair
///
/// Deleting the host side automatically removes the peer (if it still exists).
/// If the peer was moved to a namespace that was deleted, this still works.
pub async fn delete_veth_pair(host_veth: &str) -> Result<()> {
    info!(veth = %host_veth, "deleting veth pair");

    let output = Command::new("sudo")
        .args(["ip", "link", "del", host_veth])
        .output()
        .await
        .context("deleting veth pair")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "Cannot find device" - already deleted
        if stderr.contains("Cannot find device") {
            warn!(veth = %host_veth, "veth already deleted");
            return Ok(());
        }
        anyhow::bail!("failed to delete veth {}: {}", host_veth, stderr);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::namespace::{create_namespace, delete_namespace};

    #[tokio::test]
    async fn test_veth_lifecycle() {
        let ns_name = "fcvm-test-veth";
        let host_veth = "veth-host-test";
        let guest_veth = "veth-ns-test";

        // Cleanup from previous runs
        let _ = delete_veth_pair(host_veth).await;
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        create_namespace(ns_name).await.unwrap();

        // Create veth pair
        create_veth_pair(host_veth, guest_veth, ns_name)
            .await
            .unwrap();

        // Setup host side
        setup_host_veth(host_veth, "172.30.0.1/30")
            .await
            .unwrap();

        // Setup guest side
        setup_guest_veth_in_ns(ns_name, guest_veth, "172.30.0.2/30", "172.30.0.1")
            .await
            .unwrap();

        // Verify host veth exists
        let output = Command::new("ip")
            .args(["link", "show", host_veth])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());

        // Verify guest veth exists in namespace
        let output = exec_in_namespace(ns_name, &["ip", "link", "show", guest_veth])
            .await
            .unwrap();
        assert!(output.status.success());

        // Cleanup
        delete_veth_pair(host_veth).await.unwrap();
        delete_namespace(ns_name).await.unwrap();
    }

    #[tokio::test]
    async fn test_tap_creation() {
        let ns_name = "fcvm-test-tap";
        let tap_name = "tap-test";

        // Cleanup from previous runs
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        create_namespace(ns_name).await.unwrap();

        // Create TAP device
        create_tap_in_ns(ns_name, tap_name).await.unwrap();

        // Verify TAP exists in namespace
        let output = exec_in_namespace(ns_name, &["ip", "link", "show", tap_name])
            .await
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(tap_name));

        // Cleanup
        delete_namespace(ns_name).await.unwrap();
    }
}
