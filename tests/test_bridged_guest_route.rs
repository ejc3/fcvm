//! A restored bridged VM's host route to its guest address leaves a running VM's guest
//! address alone (#1882).
//!
//! A bridged VM restored from a snapshot keeps the guest address it had when the snapshot
//! was taken, and `veth::add_host_route_to_guest` gives the host a `/32` route to it through
//! the restored VM's own veth. A cold-booted bridged VM holds a `/30` on its veth and its
//! published ports DNAT to its guest address, so a `/32` for that same address took every
//! connection to those ports to the restored VM instead: in CI run 43569425463 a cache-hit
//! restore of a snapshot whose guest had the same address took it from a running VM, and
//! the gym's next connection to the running VM's published port was refused.
//!
//! Two veths stand in for the two VMs, in a network namespace of the test's own: one
//! holding the running VM's `/30`, and one whose `/30` holds the restored VM's next hop.
//! The test calls the function fcvm calls and asks the kernel where the running VM's guest
//! address routes.

#![cfg(feature = "privileged-tests")]

use anyhow::{Context, Result};
use std::process::Command;

fn run(args: &[&str]) -> Result<String> {
    let output = Command::new(args[0])
        .args(&args[1..])
        .output()
        .with_context(|| format!("running {args:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The network namespace, deleted when dropped.
struct Namespace(String);

impl Drop for Namespace {
    fn drop(&mut self) {
        let _ = Command::new("ip").args(["netns", "del", &self.0]).status();
    }
}

/// The device `ip route get` names for `address`.
fn routed_dev(address: &str) -> Result<String> {
    let out = run(&["ip", "route", "get", address])?;
    let words: Vec<&str> = out.split_whitespace().collect();
    words
        .windows(2)
        .find(|w| w[0] == "dev")
        .map(|w| w[1].to_string())
        .with_context(|| format!("`ip route get {address}` names no device: {out:?}"))
}

#[test]
fn a_restored_vm_leaves_a_running_vms_guest_address_on_its_veth() -> Result<()> {
    let id = std::process::id() % 100_000;
    let ns = Namespace(format!("fcvm-groute-{id}"));
    run(&["ip", "netns", "add", &ns.0])?;
    let path = format!("/run/netns/{}", ns.0);

    // Everything below runs on a thread in the namespace, and so does every `ip` that
    // fcvm's function spawns from it.
    std::thread::spawn(move || -> Result<()> {
        let file = std::fs::File::open(&path).with_context(|| format!("opening {path}"))?;
        nix::sched::setns(&file, nix::sched::CloneFlags::CLONE_NEWNET)
            .with_context(|| format!("entering {path}"))?;
        run(&["ip", "link", "set", "lo", "up"])?;

        // The running VM: a cold boot's host veth, holding the host end of its /30. Its
        // guest is the /30 peer.
        let (running, running_peer) = (format!("veth0-r{id}"), format!("rp{id}"));
        let guest = "172.30.250.2";
        // The restored VM: its host veth's /30 holds the namespace end it routes through.
        let (restored, restored_peer) = (format!("veth0-c{id}"), format!("cp{id}"));
        let via = "10.250.0.2";
        for (dev, peer, host_end) in [
            (&running, &running_peer, "172.30.250.1/30"),
            (&restored, &restored_peer, "10.250.0.1/30"),
        ] {
            run(&[
                "ip", "link", "add", dev, "type", "veth", "peer", "name", peer,
            ])?;
            run(&["ip", "addr", "add", host_end, "dev", dev])?;
            run(&["ip", "link", "set", dev, "up"])?;
            run(&["ip", "link", "set", peer, "up"])?;
        }
        anyhow::ensure!(
            routed_dev(guest)? == running,
            "before any restore the guest address does not route through the running VM's veth"
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(fcvm::network::veth::add_host_route_to_guest(
            &restored, guest, via,
        ))?;
        let after = routed_dev(guest)?;
        anyhow::ensure!(
            after == running,
            "a restored VM's host route took the running VM's guest address {guest}: it \
             routes through {after}, not {running}, so the running VM's published ports \
             reach the restored VM"
        );

        // Once no running VM holds the address, the restored VM takes it.
        run(&["ip", "link", "del", &running])?;
        runtime.block_on(fcvm::network::veth::add_host_route_to_guest(
            &restored, guest, via,
        ))?;
        let taken = routed_dev(guest)?;
        anyhow::ensure!(
            taken == restored,
            "with no running VM on {guest} the restored VM's route is not there: it routes \
             through {taken}, not {restored}"
        );
        Ok(())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the namespace thread panicked"))?
}
