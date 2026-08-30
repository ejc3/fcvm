//! Two VMs whose ids share their leading hex digits must not land on one set
//! of host network names (#888).
//!
//! `truncate_id(vm_id, 8)` is `vm-` plus five hex digits, so 100 clones drew a
//! duplicate about 0.5% of the time. The duplicate was not detected: `ip netns
//! add` was allowed to adopt the existing namespace ("namespace already
//! exists, reusing"), after which the second VM's veth or TAP creation failed
//! on a name the first VM already held, and its cleanup deleted the namespace
//! the first VM was still running in.
//!
//! Root, because it creates network namespaces and veth pairs. No VM boots.

#![cfg(feature = "privileged-tests")]

use std::path::Path;

use fcvm::network::{BridgedNetwork, NetworkManager};

/// The pair from the x64 job in #888, verbatim. They share `e7f5d1`, so the
/// name they derive is identical no matter how wide the truncation is made
/// short of an interface name Linux would reject: only reserving the name can
/// separate them.
const VM_ID_A: &str = "vm-e7f5d11346f04cc280d9f9db7dc45124";
const VM_ID_B: &str = "vm-e7f5d1a1d4fd4728a1216b803888393c";

fn link_exists(name: &str) -> bool {
    Path::new("/sys/class/net").join(name).exists()
}

fn namespace_exists(name: &str) -> bool {
    Path::new("/var/run/netns").join(name).exists()
}

/// The host veth of a bridged VM, from the namespace it reserved.
fn host_veth_of(namespace: &str) -> String {
    format!(
        "veth0-{}",
        namespace
            .strip_prefix("fcvm-")
            .expect("bridged namespaces are named fcvm-<base>")
    )
}

#[tokio::test]
async fn two_vm_ids_sharing_leading_hex_digits_get_separate_network_names() {
    let mut a = BridgedNetwork::new(
        VM_ID_A.to_string(),
        format!("tap-{}", &VM_ID_A[..8]),
        vec![],
    )
    .with_dns_server(Some("127.0.0.53".to_string()));
    let mut b = BridgedNetwork::new(
        VM_ID_B.to_string(),
        format!("tap-{}", &VM_ID_B[..8]),
        vec![],
    )
    .with_dns_server(Some("127.0.0.53".to_string()));

    let setup_a = a.setup().await;
    // B's setup happens with A's network fully live, which is the situation
    // two concurrently starting clones are in.
    let setup_b = if setup_a.is_ok() {
        Some(b.setup().await)
    } else {
        None
    };

    // Read the host's view before tearing anything down.
    let observed = setup_a.as_ref().ok().map(|config_a| {
        let ns_a = a
            .namespace_id()
            .expect("A reserved a namespace")
            .to_string();
        let ns_b = b.namespace_id().map(str::to_string);
        (
            ns_a.clone(),
            config_a.tap_device.clone(),
            config_a.host_veth.clone(),
            namespace_exists(&ns_a),
            link_exists(&host_veth_of(&ns_a)),
            ns_b,
        )
    });

    let _ = b.cleanup().await;
    let _ = a.cleanup().await;

    let config_a = setup_a.expect("baseline VM A must set up");
    let config_b = setup_b
        .expect("A set up, so B must have been attempted")
        .expect("VM B must set up alongside A, not collide with it");
    let (ns_a, tap_a, veth_a, ns_a_alive, veth_a_alive, ns_b) = observed.expect("A set up");
    let ns_b = ns_b.expect("B reserved a namespace");

    assert_ne!(ns_a, ns_b, "B reused A's network namespace");
    assert_ne!(
        tap_a, config_b.tap_device,
        "B reused A's TAP name ({tap_a})"
    );
    assert_ne!(veth_a, config_b.host_veth, "B reused A's host veth");
    assert_ne!(
        config_a.host_ip, config_b.host_ip,
        "B reused A's host veth address"
    );

    // A must still have been there when B finished: adopting A's namespace put
    // B's teardown in charge of it.
    assert!(
        ns_a_alive,
        "A's namespace {ns_a} was gone once B finished setting up"
    );
    assert!(
        veth_a_alive,
        "A's host veth {} was gone once B finished setting up",
        host_veth_of(&ns_a)
    );
}
