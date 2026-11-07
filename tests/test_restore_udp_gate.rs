//! A restored clone's published UDP port is closed by the host from before the VM
//! resumes until the guest acknowledges its restore (#1114, `src/network/udp_gate.rs`).
//!
//! The guest adds its own UDP rejection only after it runs, so without the host's gate
//! a datagram that pasta forwarded between the resume and that rule reached the
//! restored workload before its restore hook had run.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

/// Wait, up to two minutes, until the log at `path` contains `marker`.
async fn wait_for_log(path: &std::path::Path, marker: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if std::fs::read_to_string(path)
            .unwrap_or_default()
            .contains(marker)
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "{path:?} never logged {marker:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Where the held clone's published port listens, and the command prefix that runs a
/// program in its network namespace.
///
/// Rootless: the VM's loopback address from its state, and nsenter into the user and
/// network namespaces of a child of its fcvm process (the namespace holder). Bridged:
/// the veth address and the named namespace, from the clone's log, which records both
/// before the hold.
async fn held_clone_network(
    name: &str,
    clone_pid: u32,
    clone_log: &std::path::Path,
    network: &str,
) -> Result<(Vec<String>, String)> {
    let fcvm = common::find_fcvm_binary()?;
    let own_netns = std::fs::read_link("/proc/self/ns/net")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if network == "bridged" {
            let log = std::fs::read_to_string(clone_log).unwrap_or_default();
            let field = |line: &str, key: &str| {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix(key))
                    .map(str::to_string)
            };
            if let Some(line) = log
                .lines()
                .find(|line| line.contains("network namespace configured successfully"))
            {
                if let (Some(namespace), Some(host_ip)) =
                    (field(line, "namespace="), field(line, "host_ip="))
                {
                    let prefix = ["ip", "netns", "exec", namespace.as_str()];
                    return Ok((prefix.iter().map(|s| s.to_string()).collect(), host_ip));
                }
            }
        } else {
            let output = tokio::process::Command::new(&fcvm)
                .args(["ls", "--json"])
                .output()
                .await
                .context("running fcvm ls --json")?;
            let vms: Vec<serde_json::Value> =
                serde_json::from_slice(&output.stdout).context("parsing fcvm ls --json")?;
            let loopback = vms
                .iter()
                .find(|vm| vm["name"] == name)
                .and_then(|vm| vm["config"]["network"]["loopback_ip"].as_str())
                .map(str::to_string);
            let children =
                std::fs::read_to_string(format!("/proc/{clone_pid}/task/{clone_pid}/children"))
                    .unwrap_or_default();
            let holder = children
                .split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok())
                .find(|pid| {
                    std::fs::read_link(format!("/proc/{pid}/ns/net"))
                        .is_ok_and(|netns| netns != own_netns)
                });
            if let (Some(holder), Some(loopback)) = (holder, loopback) {
                let prefix = [
                    "nsenter",
                    "-t",
                    &holder.to_string(),
                    "-U",
                    "-n",
                    "--preserve-credentials",
                    "--",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                return Ok((prefix, loopback));
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the held {network} clone {name} never recorded its published address and namespace"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Run `args` behind `prefix`, in the clone's network namespace.
async fn in_namespace(prefix: &[String], args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new(&prefix[0])
        .args(&prefix[1..])
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {args:?} in the clone's namespace"))?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// The egress filters of every link in the clone's network namespace.
async fn egress_filters(prefix: &[String]) -> Result<String> {
    let links = in_namespace(prefix, &["ip", "-o", "link", "show"]).await?;
    let mut filters = String::new();
    for line in links.lines() {
        let Some(name) = line.split(':').nth(1).map(str::trim) else {
            continue;
        };
        let name = name.split('@').next().unwrap_or(name);
        let shown = in_namespace(
            prefix,
            &["/usr/sbin/tc", "filter", "show", "dev", name, "egress"],
        )
        .await?;
        filters.push_str(&format!("{name}:\n{shown}"));
    }
    Ok(filters)
}

/// From before the clone's VMM resumes until its restore ACK, the host drops datagrams
/// to its published UDP guest port; after the ACK they arrive.
///
/// The clone is held before resume (`restore.post_network_pre_resume`), where pasta's
/// UDP listener is already live, and the TAP's egress filters are read there. Datagrams
/// are sent from that moment until the ACK. The guest's listener appends each to a log
/// that the restore hook also writes, so a datagram delivered before the hook ran shows
/// up ahead of the hook's line.
#[tokio::test]
async fn test_restored_clone_udp_port_is_closed_until_the_restore_ack() -> Result<()> {
    closed_until_the_restore_ack("rootless").await
}

/// The same in bridged mode, where the host's UDP DNAT rules are live before the resume.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_restored_bridged_clone_udp_port_is_closed_until_the_restore_ack() -> Result<()> {
    closed_until_the_restore_ack("bridged").await
}

async fn closed_until_the_restore_ack(network: &str) -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) =
        common::unique_names(&format!("udp-gate-{network}"));
    let host_port = common::find_available_high_port()?;
    let publish = format!("{host_port}:5000/udp");
    let go_file = std::path::PathBuf::from(format!("/tmp/fcvm-{clone_name}.go"));
    let _ = std::fs::remove_file(&go_file);

    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            network,
            "--no-snapshot",
            "--publish",
            &publish,
            "--health-check",
            "http://localhost:80",
            common::TEST_IMAGE,
            "sh",
            "-c",
            "nc -lu -p 5000 >> /tmp/udp.log & exec nginx -g 'daemon off;'",
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;
    common::poll_health_by_pid(baseline_pid, 180).await?;
    common::create_snapshot_by_pid(baseline_pid, &snapshot_name).await?;
    common::kill_process(baseline_pid).await;

    let failpoint = format!(
        "restore.post_network_pre_resume:block_until_file:{}",
        go_file.display()
    );
    let (_clone_child, clone_pid, clone_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot_name,
            "--name",
            &clone_name,
            "--on-restore",
            "echo HOOK >> /tmp/udp.log",
        ],
        &[("FCVM_FAILPOINT", &failpoint)],
    )
    .await
    .context("spawning clone held before VMM resume")?;
    let result: Result<()> = async {
        wait_for_log(
            &clone_log,
            "FAILPOINT restore.post_network_pre_resume reached",
        )
        .await?;
        let (namespace, address) =
            held_clone_network(&clone_name, clone_pid, &clone_log, network).await?;
        let held = egress_filters(&namespace).await?;
        anyhow::ensure!(
            held.contains("ip_proto udp")
                && held.contains("dst_port 5000")
                && held.contains("eth_type ipv4")
                && held.contains("eth_type ipv6")
                && held.contains("action drop"),
            "before the VMM resumes, the clone's TAP must drop UDP to guest port 5000 in \
             both families; the namespace's egress filters are:\n{held}"
        );

        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        let target = format!("{address}:{host_port}");
        std::fs::write(&go_file, b"go").context("releasing the held clone")?;
        let mut sent = 0u32;
        while !std::fs::read_to_string(&clone_log)
            .unwrap_or_default()
            .contains("acknowledged exact restore generation")
        {
            socket
                .send_to(format!("pre-{sent}\n").as_bytes(), &target)
                .await?;
            sent += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let open = egress_filters(&namespace).await?;
        anyhow::ensure!(
            !open.contains("dst_port 5000"),
            "after the restore ACK the UDP gate must be gone; the namespace's egress \
             filters are:\n{open}"
        );

        // Datagrams after the ACK arrive.
        let deadline = Instant::now() + Duration::from_secs(20);
        let log = loop {
            socket.send_to(b"post\n", &target).await?;
            let log = common::exec_in_container(clone_pid, &["cat", "/tmp/udp.log"]).await?;
            if log.contains("post") {
                break log;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "no datagram reached the guest after the restore ACK; its log:\n{log}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let before_hook: Vec<&str> = log
            .lines()
            .take_while(|line| *line != "HOOK")
            .filter(|line| line.starts_with("pre-"))
            .collect();
        println!("sent {sent} datagrams between the release and the ACK");
        anyhow::ensure!(
            log.lines().any(|line| line == "HOOK"),
            "the restore hook did not run; the guest's log:\n{log}"
        );
        anyhow::ensure!(
            before_hook.is_empty(),
            "{} datagram(s) reached the restored workload before its restore hook ran: \
             {before_hook:?}",
            before_hook.len()
        );
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&go_file);
    common::kill_process(clone_pid).await;
    result
}

/// A routed clone's own IPv6 address takes no datagram before the clone's UDP rule is in.
///
/// The network reaches a routed clone at its own address, which the guest takes during
/// the restore. Taken before the UDP rule went in, it let a datagram reach the restored
/// workload ahead of its restore hook (#1114). A guest process listening on `[::]:5000`
/// stamps each datagram with the guest's boot clock, the restore hook stamps its own
/// run, and datagrams are sent to the clone's address every millisecond from before it
/// resumes until it acknowledges.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_restored_routed_clone_takes_no_datagram_before_its_udp_rule() -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) = common::unique_names("udp-gate-routed");
    let go_file = std::path::PathBuf::from(format!("/tmp/fcvm-{clone_name}.go"));
    let _ = std::fs::remove_file(&go_file);

    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "routed",
            "--no-snapshot",
            "--health-check",
            "http://localhost:80",
            "--on-restore",
            "cat /proc/uptime > /tmp/hook-uptime",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;
    common::poll_health_by_pid(baseline_pid, 180).await?;
    let listener = "nohup setsid python3 -c \"import socket, time\n\
         s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)\n\
         s.bind(('::', 5000))\n\
         f = open('/tmp/udp-arrivals', 'a')\n\
         while True:\n    d, _ = s.recvfrom(100)\n    f.write('%.3f %s\\n' % (time.clock_gettime(time.CLOCK_BOOTTIME), d.decode().strip()))\n    f.flush()\n\" \
         >/dev/null 2>&1 &";
    common::exec_in_vm(baseline_pid, &[listener])
        .await
        .context("starting the guest's UDP listener")?;
    // The listener is up before the snapshot.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let bound = common::exec_in_vm(baseline_pid, &["ss -Hlun 'sport = :5000'"]).await?;
        if bound.contains(":5000") {
            break;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the guest's UDP listener never bound"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    common::create_snapshot_by_pid(baseline_pid, &snapshot_name).await?;
    common::kill_process(baseline_pid).await;

    let failpoint = format!(
        "restore.post_network_pre_resume:block_until_file:{}",
        go_file.display()
    );
    let (_clone_child, clone_pid, clone_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot_name,
            "--name",
            &clone_name,
        ],
        &[("FCVM_FAILPOINT", &failpoint)],
    )
    .await
    .context("spawning clone held before VMM resume")?;
    let result: Result<()> = async {
        wait_for_log(
            &clone_log,
            "FAILPOINT restore.post_network_pre_resume reached",
        )
        .await?;
        let log = std::fs::read_to_string(&clone_log).context("reading the clone log")?;
        let address: std::net::Ipv6Addr = log
            .split("vm_ipv6=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .context("the clone did not log its vm_ipv6")?
            .parse()
            .context("vm_ipv6 is not an IPv6 address")?;

        let socket = tokio::net::UdpSocket::bind("[::]:0").await?;
        let target = std::net::SocketAddr::new(address.into(), 5000);
        std::fs::write(&go_file, b"go").context("releasing the held clone")?;
        let mut sent = 0u32;
        while !std::fs::read_to_string(&clone_log)
            .unwrap_or_default()
            .contains("acknowledged exact restore generation")
        {
            socket
                .send_to(format!("pre-{sent}\n").as_bytes(), target)
                .await?;
            sent += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // The listener works: a datagram after the ACK arrives.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            socket.send_to(b"post\n", target).await?;
            let arrivals = common::exec_in_vm(clone_pid, &["cat /tmp/udp-arrivals 2>/dev/null"])
                .await
                .unwrap_or_default();
            if arrivals.contains(" post") {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "no datagram reached the routed clone after its ACK; arrivals:\n{arrivals}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let hook: f64 = common::exec_in_container(clone_pid, &["cat", "/tmp/hook-uptime"])
            .await?
            .split_whitespace()
            .next()
            .context("the restore hook left no uptime")?
            .parse()
            .context("the hook's uptime is not a number")?;
        let arrivals = common::exec_in_vm(clone_pid, &["cat /tmp/udp-arrivals 2>/dev/null"])
            .await
            .unwrap_or_default();
        let before_hook: Vec<&str> = arrivals
            .lines()
            .filter(|line| {
                line.split_whitespace()
                    .next()
                    .and_then(|t| t.parse::<f64>().ok())
                    .is_some_and(|t| t < hook)
            })
            .collect();
        println!(
            "sent {sent} datagrams between the release and the ACK; {} arrived in all, {} \
             before the hook",
            arrivals.lines().count(),
            before_hook.len()
        );
        anyhow::ensure!(
            before_hook.is_empty(),
            "{} datagram(s) reached the routed clone's workload before its restore hook ran \
             (hook at {hook}): {before_hook:?}",
            before_hook.len()
        );
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&go_file);
    common::kill_process(clone_pid).await;
    result
}
