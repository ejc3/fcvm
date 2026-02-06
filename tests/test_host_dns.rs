//! Test that host DNS servers are passed to the guest for direct resolution.
//!
//! On IPv6-only hosts, slirp4netns's built-in DNS proxy (10.0.2.3) can't
//! forward to IPv6 nameservers. This test verifies that fc-agent writes
//! the host's real nameservers into /etc/resolv.conf via the fcvm_dns=
//! boot parameter.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};

/// Verify the guest gets the host's real DNS servers (not slirp's 10.0.2.3)
/// and can resolve hostnames directly through them.
#[tokio::test]
async fn test_guest_has_host_dns_servers() -> Result<()> {
    println!("\nTest host DNS servers passed to guest");
    println!("======================================");

    // Read host's DNS servers for comparison
    let host_resolv = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
        .or_else(|_| std::fs::read_to_string("/etc/resolv.conf"))
        .context("reading host resolv.conf")?;

    let host_nameservers: Vec<&str> = host_resolv
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver "))
        .filter(|s| !s.starts_with("127."))
        .collect();

    println!("  Host nameservers: {:?}", host_nameservers);

    if host_nameservers.is_empty() {
        println!("  SKIP: no non-localhost nameservers on host");
        return Ok(());
    }

    let (vm_name, _, _, _) = common::unique_names("host-dns");

    let (_, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--no-snapshot",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;

    common::poll_health_by_pid(pid, 300).await?;
    println!("  VM healthy");

    // Check guest's resolv.conf
    let guest_resolv = common::exec_in_vm(pid, &["cat", "/etc/resolv.conf"]).await?;
    println!("  Guest resolv.conf:\n{}", guest_resolv.trim());

    // Verify guest has the host's nameservers (not 10.0.2.3)
    assert!(
        !guest_resolv.contains("10.0.2.3"),
        "Guest should have host DNS servers, not slirp's 10.0.2.3"
    );

    for ns in &host_nameservers {
        assert!(
            guest_resolv.contains(ns),
            "Guest resolv.conf missing host nameserver: {}",
            ns
        );
    }

    // Verify DNS actually works by resolving a hostname
    let result = common::exec_in_vm(pid, &["nslookup", "facebook.com"]).await;
    println!("  nslookup facebook.com: {:?}", result);
    assert!(
        result.is_ok(),
        "DNS resolution should work with host nameservers"
    );

    common::kill_process(pid).await;
    println!("✅ HOST DNS TEST PASSED!");
    Ok(())
}
