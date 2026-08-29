//! Test that host DNS servers are passed to the guest for direct resolution.
//!
//! In bridged mode, the host's real DNS servers are passed to the guest via the
//! fcvm_dns= boot parameter. In rootless/pasta mode, DNS goes through pasta's
//! forwarder (10.0.2.3) which forwards to the host resolver — that's tested
//! implicitly by the sanity tests. This test verifies the bridged mode path.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use fcvm::network::{nameservers_from_sources, ResolvSource, ETC_RESOLV_CONF, RESOLV_CONF_SOURCES};

/// Verify the guest gets the host's real DNS servers (not the default 10.0.2.3)
/// and can resolve hostnames directly through them.
#[tokio::test]
async fn test_guest_has_host_dns_servers() -> Result<()> {
    println!("\nTest host DNS servers passed to guest");
    println!("======================================");

    // Read the host's resolvers through the same sources the launch path
    // reads. Committing to the first readable file here is what #875 was:
    // a stub-only /run/systemd/resolve/resolv.conf hides the usable
    // /etc/resolv.conf, this test skips, and the bridged path it exists to
    // cover goes unexercised.
    let sources = RESOLV_CONF_SOURCES.map(ResolvSource::read);
    let host_nameservers = match nameservers_from_sources(&sources) {
        Ok(servers) => servers,
        Err(e) => {
            // No source names a server a VM could reach, so a bridged launch
            // has nothing to forward and fails before the guest boots. Nothing
            // about the guest is under test on such a host.
            println!("  SKIP: {e:#}");
            return Ok(());
        }
    };

    println!("  Host nameservers: {:?}", host_nameservers);

    // Bridged mode forwards one resolver to the guest: GuestBootInputs::for_launch
    // truncates host_dns to the first entry, which network_config.dns_server
    // carries onto the cmdline. Assert on that entry, not the whole list.
    let forwarded = &host_nameservers[0];

    let (vm_name, _, _, _) = common::unique_names("host-dns");

    let (_, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--no-snapshot",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;

    common::poll_health_by_pid(pid, 300).await?;
    println!("  VM healthy");

    // Check guest's resolv.conf
    let guest_resolv = common::exec_in_vm(pid, &["cat", ETC_RESOLV_CONF]).await?;
    println!("  Guest resolv.conf:\n{}", guest_resolv.trim());

    // Verify guest has the host's nameservers (not 10.0.2.3)
    assert!(
        !guest_resolv.contains("10.0.2.3"),
        "Guest should have host DNS servers, not the default 10.0.2.3"
    );

    assert!(
        guest_resolv.contains(forwarded),
        "Guest resolv.conf missing the host nameserver bridged forwards ({}): {}",
        forwarded,
        guest_resolv.trim()
    );

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
