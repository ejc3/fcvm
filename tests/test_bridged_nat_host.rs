//! The bridged NAT rule on a host whose uplink holds two addresses (#1735).
//!
//! Three network namespaces stand in for the host, the next hop and a bridged guest.
//! The host's uplink holds 10.20.30.7/24 first and 10.20.30.40/24 second, and its
//! default route leaves from 10.20.30.40, as on the dev box where a MASQUERADE for
//! 10.0.0.0/8 rewrote the host's own connections to 10.20.30.7, which had no public
//! mapping, and took the host's IPv4 down while a bridged VM ran. The test installs
//! the rule fcvm installs (`global_nat_rule`) and checks the address the next hop sees
//! for a connection the host opens and for one a guest opens through the host.

#![cfg(feature = "privileged-tests")]

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::time::Duration;

const HOST_PRIMARY: &str = "10.20.30.7";
const HOST_ROUTE_SOURCE: &str = "10.20.30.40";
const NEXT_HOP: &str = "10.20.30.1";
/// A server beyond the next hop, reached by the default route as the internet is.
const SERVER: &str = "198.51.100.10";

/// Network namespaces deleted when dropped, so a failed assertion leaves none behind.
struct Namespaces(Vec<String>);

impl Drop for Namespaces {
    fn drop(&mut self) {
        for name in &self.0 {
            let _ = Command::new("ip").args(["netns", "del", name]).status();
        }
    }
}

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

fn in_ns(ns: &str, args: &[&str]) -> Result<String> {
    let mut all = vec!["ip", "netns", "exec", ns];
    all.extend_from_slice(args);
    run(&all)
}

/// Run `f` on a thread whose network namespace is `ns`.
fn on_thread_in<T: Send + 'static>(
    ns: &str,
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::thread::JoinHandle<Result<T>> {
    let path = format!("/run/netns/{ns}");
    std::thread::spawn(move || {
        let file = std::fs::File::open(&path).with_context(|| format!("opening {path}"))?;
        nix::sched::setns(&file, nix::sched::CloneFlags::CLONE_NEWNET)
            .with_context(|| format!("entering {path}"))?;
        f()
    })
}

/// Connect from `ns` to the server beyond the next hop and say `who`.
fn connect_from(ns: &str, who: &'static str) -> Result<()> {
    on_thread_in(ns, move || {
        let target: SocketAddr = format!("{SERVER}:8080").parse()?;
        let mut stream = TcpStream::connect_timeout(&target, Duration::from_secs(5))
            .with_context(|| format!("{who} connecting to {target}"))?;
        stream.write_all(who.as_bytes())?;
        Ok(())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("{who}'s client thread panicked"))?
}

#[test]
fn bridged_nat_leaves_the_hosts_own_connections_on_their_route_source() -> Result<()> {
    let id = std::process::id();
    let (host, peer, guest) = (
        format!("fcvm-nat-h-{id}"),
        format!("fcvm-nat-p-{id}"),
        format!("fcvm-nat-g-{id}"),
    );
    let _namespaces = Namespaces(vec![host.clone(), peer.clone(), guest.clone()]);
    for ns in [&host, &peer, &guest] {
        run(&["ip", "netns", "add", ns])?;
        in_ns(ns, &["ip", "link", "set", "lo", "up"])?;
    }
    let (uplink, hop, guest_side, guest_if) = (
        format!("nu{id}"),
        format!("np{id}"),
        format!("ng{id}"),
        format!("nv{id}"),
    );

    // The host's uplink: the address with no public mapping first (the primary), the
    // one the default route leaves from second.
    run(&[
        "ip", "link", "add", &uplink, "netns", &host, "type", "veth", "peer", "name", &hop,
        "netns", &peer,
    ])?;
    in_ns(
        &host,
        &[
            "ip",
            "addr",
            "add",
            &format!("{HOST_PRIMARY}/24"),
            "dev",
            &uplink,
        ],
    )?;
    in_ns(
        &host,
        &[
            "ip",
            "addr",
            "add",
            &format!("{HOST_ROUTE_SOURCE}/24"),
            "dev",
            &uplink,
        ],
    )?;
    in_ns(&host, &["ip", "link", "set", &uplink, "up"])?;
    in_ns(
        &host,
        &[
            "ip",
            "route",
            "add",
            "default",
            "via",
            NEXT_HOP,
            "dev",
            &uplink,
            "src",
            HOST_ROUTE_SOURCE,
        ],
    )?;
    in_ns(
        &peer,
        &["ip", "addr", "add", &format!("{NEXT_HOP}/24"), "dev", &hop],
    )?;
    in_ns(&peer, &["ip", "link", "set", &hop, "up"])?;
    in_ns(
        &peer,
        &["ip", "addr", "add", &format!("{SERVER}/32"), "dev", "lo"],
    )?;

    // A bridged clone's namespace side of its veth, 10.x.y.2, routed through the host.
    run(&[
        "ip",
        "link",
        "add",
        &guest_side,
        "netns",
        &host,
        "type",
        "veth",
        "peer",
        "name",
        &guest_if,
        "netns",
        &guest,
    ])?;
    in_ns(
        &host,
        &["ip", "addr", "add", "10.200.0.1/30", "dev", &guest_side],
    )?;
    in_ns(&host, &["ip", "link", "set", &guest_side, "up"])?;
    in_ns(&host, &["sysctl", "-qw", "net.ipv4.ip_forward=1"])?;
    in_ns(
        &guest,
        &["ip", "addr", "add", "10.200.0.2/30", "dev", &guest_if],
    )?;
    in_ns(&guest, &["ip", "link", "set", &guest_if, "up"])?;
    in_ns(
        &guest,
        &["ip", "route", "add", "default", "via", "10.200.0.1"],
    )?;

    // The rule fcvm installs, from the host's own default route.
    let source = fcvm::network::portmap::parse_default_route_source(&in_ns(
        &host,
        &["ip", "-4", "route", "show", "default"],
    )?);
    let rule = fcvm::network::portmap::global_nat_rule("10.0.0.0/8", &uplink, source.as_deref());
    let mut add = vec!["iptables", "-t", "nat", "-A", "POSTROUTING"];
    add.extend(rule.iter().map(String::as_str));
    in_ns(&host, &add)?;
    // iptables lists the rule exactly as fcvm builds it, which is how a later launch
    // tells its own rule from a stale one (`global_nat_replacement`).
    let listed = in_ns(&host, &["iptables", "-t", "nat", "-S", "POSTROUTING"])?;
    let line = format!("-A POSTROUTING {}", rule.join(" "));
    assert!(
        listed.lines().any(|l| l == line),
        "iptables lists the rule differently from {line:?}:\n{listed}"
    );

    let server = on_thread_in(&peer, || {
        let listener = TcpListener::bind(format!("{SERVER}:8080"))?;
        let mut seen = Vec::new();
        for _ in 0..2 {
            let (mut stream, from) = listener.accept()?;
            let mut who = String::new();
            stream.read_to_string(&mut who)?;
            seen.push((who, from.ip().to_string()));
        }
        Ok(seen)
    });
    // The listener is bound once a connection succeeds; retry briefly until then.
    let mut connected = false;
    for _ in 0..50 {
        if connect_from(&host, "host").is_ok() {
            connected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::ensure!(connected, "the host never reached the server");
    connect_from(&guest, "guest")?;
    let seen = server
        .join()
        .map_err(|_| anyhow::anyhow!("the server thread panicked"))??;

    for (who, from) in &seen {
        assert_eq!(
            from, HOST_ROUTE_SOURCE,
            "the server saw the {who}'s connection from {from}, not from the host's \
             route source {HOST_ROUTE_SOURCE} (rule {rule:?})"
        );
    }
    Ok(())
}
