//! `--ipv6-address` and unique VM names, end to end, in routed mode.
//!
//! Routed mode needs root and a routable IPv6 prefix: FCVM_IPV6_PREFIX, or the /64 it
//! detects on the host. The test learns the prefix in use from fcvm's own refusal of an
//! address outside it, so it runs under whichever of the two the host provides.

#![cfg(all(feature = "integration-slow", feature = "privileged-tests"))]

mod common;

use anyhow::{Context, Result};

/// An address in the documentation prefix (RFC 3849), which no host routes.
const OUTSIDE_ANY_PREFIX: &str = "2001:db8:ffff:ffff::2";

/// A routed VM started with `--ipv6-address` holds that address, a second VM asking for
/// it or for the VM's name is refused without disturbing the first, and a clone restored
/// from the VM's snapshot with another `--ipv6-address` holds the new address in place of
/// the old one.
#[tokio::test]
async fn test_routed_vm_and_clone_take_the_given_ipv6_address() -> Result<()> {
    let (base_name, clone_name, snapshot_name, _) = common::unique_names("routed-addr");
    let fcvm_path = common::find_fcvm_binary()?;

    // 1. An address outside the prefix in use is refused, and the refusal names the
    //    prefix: the addresses below are chosen inside it.
    let probe = fcvm_output(
        &fcvm_path,
        &[
            "podman",
            "run",
            "--name",
            &format!("{base_name}-probe"),
            "--network",
            "routed",
            "--ipv6-address",
            OUTSIDE_ANY_PREFIX,
            common::TEST_IMAGE,
        ],
    )
    .await?;
    anyhow::ensure!(
        !probe.status.success(),
        "a routed VM started at {OUTSIDE_ANY_PREFIX}, outside any routed prefix"
    );
    let stderr = String::from_utf8_lossy(&probe.stderr);
    let prefix = stderr
        .split("is outside ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .with_context(|| format!("the refusal names no prefix:\n{stderr}"))?
        .to_string();
    // Two addresses in the last eight bits of the prefix (it leaves at least eight),
    // apart from other runs of this test by this process's pid.
    let suffix = 0x10 + (std::process::id() % 0x70) * 2;
    let base_address = address_in(&prefix, suffix)?;
    let clone_address = address_in(&prefix, suffix + 1)?;
    println!("  prefix {prefix}, VM {base_address}, clone {clone_address}");

    let mut running: Vec<u32> = Vec::new();
    let result = async {
        // 2. The VM holds the address it was given.
        let (_base_child, base_pid) = common::spawn_fcvm_with_logs(
            &[
                "podman",
                "run",
                "--name",
                &base_name,
                "--network",
                "routed",
                "--ipv6-address",
                &base_address,
                common::TEST_IMAGE,
            ],
            &base_name,
        )
        .await
        .context("spawning the VM")?;
        running.push(base_pid);
        common::poll_health_by_pid(base_pid, 300)
            .await
            .context("the VM did not become healthy")?;
        assert_holds_address(base_pid, &base_address, None).await?;

        // 3. A second VM is refused the name and the address, and the first keeps both.
        let same_name = fcvm_output(
            &fcvm_path,
            &["podman", "run", "--name", &base_name, common::TEST_IMAGE],
        )
        .await?;
        let stderr = String::from_utf8_lossy(&same_name.stderr);
        anyhow::ensure!(
            !same_name.status.success()
                && stderr.contains(&format!("already named '{base_name}' (pid {base_pid}")),
            "a second VM named {base_name} was not refused with the first one's pid:\n{stderr}"
        );
        let same_address = fcvm_output(
            &fcvm_path,
            &[
                "podman",
                "run",
                "--name",
                &format!("{base_name}-dup"),
                "--network",
                "routed",
                "--ipv6-address",
                &base_address,
                common::TEST_IMAGE,
            ],
        )
        .await?;
        let stderr = String::from_utf8_lossy(&same_address.stderr);
        anyhow::ensure!(
            !same_address.status.success()
                && stderr.contains(&format!(
                    "{base_address} is held by live VM '{base_name}' (pid {base_pid})"
                )),
            "a second VM at {base_address} was not refused naming the first:\n{stderr}"
        );
        assert_holds_address(base_pid, &base_address, None).await?;

        // 4. A clone restored with another address holds it instead of the snapshot's.
        let snap = fcvm_output(
            &fcvm_path,
            &[
                "snapshot",
                "create",
                "--pid",
                &base_pid.to_string(),
                "--tag",
                &snapshot_name,
            ],
        )
        .await?;
        anyhow::ensure!(
            snap.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&snap.stderr)
        );
        common::kill_process(base_pid).await;
        running.retain(|pid| *pid != base_pid);

        let (_serve_child, serve_pid) = common::spawn_fcvm_with_logs(
            &["snapshot", "serve", &snapshot_name],
            &format!("{snapshot_name}-serve"),
        )
        .await
        .context("spawning the memory server")?;
        running.push(serve_pid);
        common::poll_serve_state_by_pid(serve_pid, 30).await?;
        let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
            &[
                "snapshot",
                "run",
                "--pid",
                &serve_pid.to_string(),
                "--name",
                &clone_name,
                "--ipv6-address",
                &clone_address,
            ],
            &clone_name,
        )
        .await
        .context("spawning the clone")?;
        running.push(clone_pid);
        common::poll_health_by_pid(clone_pid, 120)
            .await
            .context("the clone did not become healthy")?;
        assert_holds_address(clone_pid, &clone_address, Some(&base_address)).await
    }
    .await;

    for pid in running.into_iter().rev() {
        common::kill_process(pid).await;
    }
    result
}

async fn fcvm_output(fcvm: &std::path::Path, args: &[&str]) -> Result<std::process::Output> {
    tokio::process::Command::new(fcvm)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running fcvm {args:?}"))
}

/// The address `suffix` inside `prefix` (CIDR, as fcvm's refusal prints it).
fn address_in(prefix: &str, suffix: u32) -> Result<String> {
    let (network, len) = prefix
        .split_once('/')
        .with_context(|| format!("{prefix} is not CIDR"))?;
    let network = u128::from(network.parse::<std::net::Ipv6Addr>()?);
    let len: u32 = len.parse()?;
    anyhow::ensure!(len <= 120, "{prefix} leaves fewer than eight host bits");
    Ok(std::net::Ipv6Addr::from(network | u128::from(suffix)).to_string())
}

/// The VM at `pid` holds `address`: fcvm records it, the host routes it through the VM's
/// veth and proxies NDP for it, and the guest has it on eth0. `not` is an address the
/// guest must no longer hold.
async fn assert_holds_address(pid: u32, address: &str, not: Option<&str>) -> Result<()> {
    let recorded = common::get_network_field(pid, "guest_ipv6").await?;
    anyhow::ensure!(
        recorded == format!("{address}/128"),
        "fcvm recorded {recorded} for the VM given {address}"
    );
    let veth = common::get_network_field(pid, "host_veth").await?;
    let route = host_ip(&["-6", "route", "show", &format!("{address}/128")]).await?;
    anyhow::ensure!(
        route.split_whitespace().any(|word| word == veth),
        "the host does not route {address} through the VM's veth {veth}: {}",
        route.trim()
    );
    let proxy = host_ip(&["-6", "neigh", "show", "proxy"]).await?;
    anyhow::ensure!(
        proxy
            .lines()
            .any(|line| line.split_whitespace().next() == Some(address)),
        "the host has no proxy-NDP entry for {address}:\n{proxy}"
    );
    let eth0 = common::exec_in_vm(
        pid,
        &["PATH=/usr/sbin:/usr/bin:/sbin:/bin ip -6 addr show dev eth0 scope global"],
    )
    .await
    .context("reading the guest's eth0 addresses")?;
    anyhow::ensure!(
        eth0.contains(&format!("inet6 {address}/")),
        "the guest's eth0 does not hold {address}:\n{eth0}"
    );
    if let Some(old) = not {
        anyhow::ensure!(
            !eth0.contains(&format!("inet6 {old}/")),
            "the guest's eth0 still holds {old}:\n{eth0}"
        );
    }
    Ok(())
}

async fn host_ip(args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("ip")
        .args(args)
        .output()
        .await
        .with_context(|| format!("running ip {args:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "ip {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
