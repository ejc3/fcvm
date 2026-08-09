//! `--publish` must reach a guest service bound to 127.0.0.1 — without putting
//! the guest's whole loopback surface on the wire, and without colliding with
//! fcvm's own machinery.
//!
//! A published packet arrives on the guest's external interface. A service on
//! `0.0.0.0` receives it; one on `127.0.0.1` does not — and there was no way to
//! reach it. `--forward-localhost` runs the other direction (guest -> host), so
//! aiming it at such a port hijacks the guest's own listener instead of exposing
//! it.
//!
//! That gap is why the Chromium benchmark carried a socat relay inside every
//! clone: Chromium ignores `--remote-debugging-address` and binds
//! `127.0.0.1:9222` regardless (measured on 151.0.7922.71 — the flag is present
//! in `/proc/<pid>/cmdline` while `/proc/net/tcp` shows `0100007F:2406` and
//! nothing else). The direct DNAT removes the relay's per-clone process and
//! byte-path hop. A withdrawn request-path A/B did not establish that the relay
//! caused connection drops.
//!
//! These run ROOTLESS and need no privilege: the rules are installed by fc-agent
//! inside the guest's own network namespace, so no host root is involved.
//!
//! NOT TESTED HERE, and why: proving the containment rule actually drops an
//! attacker's frame needs a packet addressed to `127.0.0.1` injected onto the
//! guest's L2 segment — you cannot produce that with a socket, because the host
//! stack routes `127.0.0.1` to its own loopback. It takes a raw frame written to
//! the tap from inside the VM's netns. This file asserts the rule is installed
//! with the exact match that makes it correct; `fc-agent/src/network.rs`'s unit
//! tests pin the argument vector.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// A loopback-only listener on this port proves the feature.
const LOOPBACK_PORT: u16 = 9231;
/// The transparent egress proxy's guest listener — must never be published.
const EGRESS_PROXY_PORT: u16 = 12345;

#[derive(Deserialize)]
struct VmDisplay {
    #[serde(flatten)]
    vm: fcvm::state::VmState,
    #[allow(dead_code)]
    stale: bool,
}

fn wait_healthy_publish_ip(fcvm_path: &std::path::Path, pid: u32) -> Result<String> {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(180) {
        std::thread::sleep(common::POLL_INTERVAL);
        let out = Command::new(fcvm_path)
            .args(["ls", "--json", "--pid", &pid.to_string()])
            .output()
            .context("running fcvm ls")?;
        if !out.status.success() {
            continue;
        }
        let Ok(vms) = serde_json::from_str::<Vec<VmDisplay>>(&String::from_utf8_lossy(&out.stdout))
        else {
            continue;
        };
        // Require Healthy specifically. `health_status` is PRESENT while its value
        // is still Unknown or Unhealthy, so testing that the field exists declares
        // success before the guest can answer anything.
        if let Some(d) = vms.first() {
            if matches!(d.vm.health_status, fcvm::state::HealthStatus::Healthy) {
                return d
                    .vm
                    .config
                    .network
                    .loopback_ip
                    .clone()
                    .or_else(|| d.vm.config.network.host_ip.clone())
                    .context("VM healthy but has no host-side published-port address");
            }
        }
    }
    anyhow::bail!("VM did not become healthy within 180s")
}

/// Run a command in the VM (not the container) and return stdout.
fn in_vm(fcvm_path: &std::path::Path, pid: u32, script: &str) -> Result<String> {
    let out = Command::new(fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            script,
        ])
        .output()
        .with_context(|| format!("fcvm exec --vm: {script}"))?;
    anyhow::ensure!(
        out.status.success(),
        "fcvm exec --vm failed for {script:?}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// One VM per network backend covers the whole contract: host-side publication
/// reaches guest loopback, the egress proxy's port is excluded, and
/// `route_localnet` is contained with the DROP first in INPUT.
fn run_publish_loopback_case(network: &str) -> Result<()> {
    println!("\npublish loopback case: {network}");
    let fcvm_path = common::find_fcvm_binary()?;
    let vm_name = format!("publoop-{network}-{}", std::process::id());
    let host_port = common::find_available_high_port().context("finding a host port")?;
    let mut proxy_host_port =
        common::find_available_high_port().context("finding a proxy host port")?;
    while proxy_host_port == host_port {
        proxy_host_port =
            common::find_available_high_port().context("finding a distinct proxy host port")?;
    }

    // nginx, whose `listen` directive gives exact control over the bind address —
    // unlike busybox `nc`, which has no bind-address option in this image.
    let guest_cmd = format!(
        "set -e\n\
         printf 'server {{ listen 127.0.0.1:{LOOPBACK_PORT}; location / {{ return 200 \"LOOPBACK_OK\"; }} }}\\n' \
         > /etc/nginx/conf.d/default.conf\n\
         exec nginx -g 'daemon off;'\n"
    );

    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        network,
        "--publish",
        &format!("{host_port}:{LOOPBACK_PORT}"),
        // Deliberately ask for the forbidden one too; fc-agent must refuse it.
        "--publish",
        &format!("{proxy_host_port}:{EGRESS_PROXY_PORT}"),
        common::TEST_IMAGE,
        "sh",
        "-c",
        &guest_cmd,
    ]);
    common::set_test_pdeathsig_std(&mut cmd);
    let mut fcvm = cmd.spawn().context("spawning fcvm")?;
    let pid = fcvm.id();

    let result = (|| -> Result<()> {
        let ip = wait_healthy_publish_ip(&fcvm_path, pid)?;

        // Wait for the fixture's listener rather than assuming it is up: the
        // container starts after the VM reports healthy.
        let want = format!("0100007F:{LOOPBACK_PORT:04X}");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut tcp = String::new();
        while std::time::Instant::now() < deadline {
            tcp = in_vm(&fcvm_path, pid, "cat /proc/net/tcp")?.to_uppercase();
            if tcp.contains(&want) {
                break;
            }
            std::thread::sleep(common::POLL_INTERVAL);
        }
        // Prove the fixture: loopback listener present, wildcard absent. Without
        // the second check a wildcard bind — reachable with or without the
        // feature — would make this pass for the wrong reason.
        anyhow::ensure!(
            tcp.contains(&want),
            "no guest listener on 127.0.0.1:{LOOPBACK_PORT} ({want} absent):\n{tcp}"
        );
        anyhow::ensure!(
            !tcp.contains(&format!("00000000:{LOOPBACK_PORT:04X}")),
            "fixture bound 0.0.0.0, which is reachable WITHOUT the feature, so this test would \
             pass with it reverted:\n{tcp}"
        );

        // 1. THE FEATURE.
        let curl = Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                "15",
                &format!("http://{ip}:{host_port}/"),
            ])
            .output()
            .context("curl the published port")?;
        let body = String::from_utf8_lossy(&curl.stdout);
        anyhow::ensure!(
            body.contains("LOOPBACK_OK"),
            "published port did not reach the guest's 127.0.0.1:{LOOPBACK_PORT} listener. \
             fc-agent must DNAT published ports to loopback and set route_localnet \
             (fc-agent/src/network.rs::publish_to_loopback). Got: {body:?}"
        );

        let nat = in_vm(&fcvm_path, pid, "iptables -w -t nat -S PREROUTING")?;

        // 2. THE EGRESS PROXY'S PORT IS EXCLUDED. It listens on guest
        //    127.0.0.1:12345 and relays through a host-side connect() with no
        //    destination validation and no timeout; a DNAT would hand that relay
        //    to external clients.
        anyhow::ensure!(
            !nat.contains(&format!("--dport {EGRESS_PROXY_PORT} ")),
            "port {EGRESS_PROXY_PORT} was DNAT'd to loopback. That is the transparent egress \
             proxy's listener (fc-agent/src/proxy.rs::PROXY_LISTEN_PORT) — publishing it lets an \
             external client drive host-side connections. It must be refused:\n{nat}"
        );
        anyhow::ensure!(
            nat.contains(&format!("--dport {LOOPBACK_PORT} ")),
            "the ordinary port lost its rule too, so the exclusion above is vacuous:\n{nat}"
        );

        // 3. route_localnet IS CONTAINED. It is per-interface, not per-port: on
        //    its own it makes EVERY guest loopback listener reachable from the
        //    guest's segment, which is far wider than --publish opened.
        let input = in_vm(&fcvm_path, pid, "iptables -w -S INPUT")?;
        let first_input_rule = input
            .lines()
            .find(|line| line.starts_with("-A INPUT "))
            .unwrap_or_default();
        anyhow::ensure!(
            first_input_rule.contains("-i eth0")
                && first_input_rule.contains("127.0.0.0/8")
                && first_input_rule.contains("--ctstate DNAT")
                && first_input_rule.ends_with("-j DROP"),
            "the containment DROP is not rule 1 in INPUT. An earlier ACCEPT would bypass this \
             security boundary; production must install it with `-I INPUT 1`:\n{input}"
        );
        anyhow::ensure!(
            input.contains("127.0.0.0/8") && input.contains("DROP"),
            "no containment rule for 127.0.0.0/8 on INPUT. route_localnet is on, so every \
             loopback listener in the guest — not just published ports — is reachable from the \
             network segment (fc-agent/src/network.rs::install_loopback_containment):\n{input}"
        );
        anyhow::ensure!(
            input.contains("--ctstate DNAT"),
            "the containment rule does not exempt DNAT'd connections, so it drops the published \
             traffic too and the feature above only passed by luck:\n{input}"
        );
        anyhow::ensure!(
            input.contains("-i eth0"),
            "the containment rule is not scoped to eth0, so it also drops guest-internal \
             loopback traffic:\n{input}"
        );
        Ok(())
    })();

    fcvm::utils::graceful_kill(pid, 2000);
    let _ = fcvm.wait();
    result
}

/// Rootless uses pasta's published-port path and is available without host root.
#[test]
fn test_publish_reaches_loopback_and_stays_contained_rootless() -> Result<()> {
    run_publish_loopback_case("rootless")
}

/// Routed uses the built-in TCP proxy for the host-side published port.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_publish_reaches_loopback_and_stays_contained_routed() -> Result<()> {
    run_publish_loopback_case("routed")
}

/// Bridged uses the host-veth DNAT path for the host-side published port.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_publish_reaches_loopback_and_stays_contained_bridged() -> Result<()> {
    run_publish_loopback_case("bridged")
}

fn output_with_deadline(mut cmd: Command, timeout: Duration, failure: &str) -> Result<Output> {
    // Regular temporary files keep draining independently of this thread and,
    // unlike pipes, cannot fill and deadlock a verbose child while we poll its
    // deadline. They also remain readable if a killed descendant briefly keeps
    // an inherited descriptor open.
    let mut stdout_file = tempfile::tempfile().context("creating stdout capture")?;
    let mut stderr_file = tempfile::tempfile().context("creating stderr capture")?;
    cmd.stdout(Stdio::from(
        stdout_file.try_clone().context("cloning stdout capture")?,
    ));
    cmd.stderr(Stdio::from(
        stderr_file.try_clone().context("cloning stderr capture")?,
    ));
    common::set_test_pdeathsig_std(&mut cmd);
    let mut child = cmd.spawn().context("spawning fcvm")?;
    let child_pid = child.id();
    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for fcvm")? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().context("reaping timed-out fcvm")?;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    stdout_file
        .seek(SeekFrom::Start(0))
        .and_then(|_| stdout_file.read_to_end(&mut stdout))
        .context("reading fcvm stdout")?;
    stderr_file
        .seek(SeekFrom::Start(0))
        .and_then(|_| stderr_file.read_to_end(&mut stderr))
        .context("reading fcvm stderr")?;
    let out = Output {
        status,
        stdout,
        stderr,
    };
    if timed_out {
        anyhow::bail!(
            "{failure} within {:.1}s; killed and reaped pid {child_pid}. stdout={} stderr={}",
            timeout.as_secs_f64(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    Ok(out)
}

/// `--publish P` + `--forward-localhost P` must be REJECTED, not discovered at
/// runtime.
///
/// `--forward-localhost` binds a relay on the guest's `127.0.0.1:<port>` pointing
/// at the host, and `--publish` now DNATs that same address. The published port
/// therefore answers with the **host's** service and never touches the guest —
/// a successful response from the wrong machine, which no amount of staring at
/// the guest will explain. Before this feature the combination was inert.
///
/// No VM: this must fail during argument validation, long before boot.
#[test]
fn test_publish_colliding_with_forward_localhost_is_rejected() -> Result<()> {
    println!("\ntest_publish_colliding_with_forward_localhost_is_rejected");
    let fcvm_path = common::find_fcvm_binary()?;
    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &format!("collide-{}", std::process::id()),
        "--network",
        "rootless",
        "--publish",
        "18080:1421",
        "--forward-localhost",
        "1421",
        common::TEST_IMAGE,
        "true",
    ]);
    let out = output_with_deadline(
        cmd,
        Duration::from_secs(30),
        "fcvm did not reject the --publish/--forward-localhost collision",
    )?;

    anyhow::ensure!(
        !out.status.success(),
        "fcvm accepted --publish 18080:1421 together with --forward-localhost 1421. The \
         published port would answer with the HOST's service on 1421 instead of the guest's, \
         silently and successfully. It must be rejected at parse time."
    );
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    anyhow::ensure!(
        msg.contains("1421") && msg.to_lowercase().contains("forward-localhost"),
        "rejected, but the message does not name the conflicting port and flag, so the user \
         cannot act on it. Got:\n{msg}"
    );
    Ok(())
}

/// The collision guard must not over-reach. `published_guest_ports` carries TCP
/// mappings alone and the `--forward-localhost` relay is a TCP listener, so
/// `--publish H:G/udp` next to `--forward-localhost G` shares a port NUMBER
/// without sharing a socket — there is no reflector, and rejecting it would break
/// a combination that works today.
#[test]
fn test_udp_publish_does_not_collide_with_forward_localhost() -> Result<()> {
    println!("\ntest_udp_publish_does_not_collide_with_forward_localhost");
    let fcvm_path = common::find_fcvm_binary()?;
    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &format!("udpok-{}", std::process::id()),
        "--network",
        "rootless",
        "--publish",
        "18081:1422/udp",
        "--forward-localhost",
        "1422",
        common::TEST_IMAGE,
        "true",
    ]);
    let out = output_with_deadline(
        cmd,
        Duration::from_secs(60),
        "fcvm did not complete the non-conflicting UDP publication case",
    )?;
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    anyhow::ensure!(
        out.status.success(),
        "fcvm rejected or failed the valid UDP publish plus TCP forward-localhost case:\n{msg}"
    );
    anyhow::ensure!(
        !msg.contains("both claim guest port"),
        "a UDP publish was rejected as colliding with --forward-localhost, but only TCP \
         mappings are DNAT'd and the relay is TCP — there is no shared socket. Got:\n{msg}"
    );
    Ok(())
}
