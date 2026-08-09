use tokio::{
    process::Command,
    time::{sleep, Duration},
};

pub async fn flush_arp_cache() {
    let output = Command::new("ip")
        .args(["neigh", "flush", "all"])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] ARP cache flushed");
        }
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: ARP flush failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: ARP flush error: {}", e);
        }
    }
}

/// Send ARP to the gateway and verify layer 2 reachability.
///
/// After snapshot restore, the bridge/network has stale MAC→port mappings from
/// the baseline VM. Sending an ARP request to the gateway accomplishes:
/// 1. The ARP REQUEST broadcast teaches the network (bridge/pasta) our MAC
/// 2. Waiting for the ARP REPLY ensures the L2 path is operational
///
/// Uses `arping` instead of `ping` because ping requires ICMP raw sockets which
/// fail in rootless mode (pasta doesn't respond to ICMP), causing a 3s timeout.
/// `arping` operates at layer 2 — pasta responds to ARP even in rootless mode.
pub async fn send_gratuitous_arp() {
    let route_output = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await;

    let gateway = match route_output {
        Ok(o) if o.status.success() => {
            let output = String::from_utf8_lossy(&o.stdout);
            output
                .split_whitespace()
                .skip_while(|&s| s != "via")
                .nth(1)
                .map(|s| s.to_string())
        }
        _ => None,
    };

    let Some(gateway) = gateway else {
        eprintln!("[fc-agent] WARNING: could not determine gateway for ARP");
        return;
    };

    // arping -c 1 -w 1 -I eth0 <gateway>
    // Sends an ARP request and waits for a reply. This verifies L2 connectivity
    // and teaches the bridge/pasta our MAC→port mapping via the request's source.
    match Command::new("arping")
        .args(["-c", "1", "-w", "1", "-I", "eth0", &gateway])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            eprintln!("[fc-agent] gateway {} ARP resolved", gateway);
        }
        Ok(output) => {
            eprintln!(
                "[fc-agent] WARNING: gateway {} arping failed (exit {})",
                gateway,
                output.status.code().unwrap_or(-1)
            );
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: arping not available ({}), skipping ARP",
                e
            );
        }
    }
}

/// Kill stale EXTERNAL TCP connections after snapshot restore.
///
/// Only kills connections to non-local addresses. Local connections
/// (127.0.0.1, ::1) between services in the same VM (e.g., HHVM → mcrouter)
/// are preserved — they're still valid after restore since both endpoints
/// resumed from the same snapshot.
///
/// Killing local connections causes a reconnection storm: mcrouter drops
/// HHVM's connections, HHVM retries, TAO/ucache lookups fail, status.php
/// blocks for minutes waiting for services to re-establish.
pub async fn kill_stale_tcp_connections() {
    let list_output = Command::new("ss")
        .args(["-tn", "state", "established"])
        .output()
        .await;

    let Ok(o) = list_output else {
        eprintln!("[fc-agent] WARNING: failed to list TCP connections");
        return;
    };

    let connections = String::from_utf8_lossy(&o.stdout);
    let mut external_count = 0;
    let mut local_count = 0;

    for line in connections.lines().skip(1) {
        // ss output: Recv-Q Send-Q Local_Address:Port Peer_Address:Port
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let peer = fields[3];
        if peer.starts_with("127.")
            || peer.starts_with("[::1]:")
            || peer.starts_with("::1:")
            || peer.starts_with("10.0.2.")
            || peer.starts_with("[fd00:")
        {
            local_count += 1;
        } else {
            external_count += 1;
        }
    }

    eprintln!(
        "[fc-agent] TCP connections: {} external (will kill), {} local (preserving)",
        external_count, local_count
    );

    if external_count == 0 {
        eprintln!("[fc-agent] no external TCP connections to kill");
        return;
    }

    // Kill only external connections using ss filter.
    // Preserve: loopback (127.0.0.0/8, [::1]), VM gateway (10.0.2.0/24).
    // Note: ss doesn't support IPv6 CIDR in brackets — [fd00::]/64 fails.
    // fd00:: traffic goes through the gateway anyway (preserved by 10.0.2.0/24 rule).
    let kill_output = Command::new("ss")
        .args([
            "-K",
            "state",
            "established",
            "(",
            "!",
            "dst",
            "127.0.0.0/8",
            "and",
            "!",
            "dst",
            "[::1]",
            "and",
            "!",
            "dst",
            "10.0.2.0/24",
            ")",
        ])
        .output()
        .await;

    match kill_output {
        Ok(o) if o.status.success() => {
            eprintln!(
                "[fc-agent] killed {} external TCP connections",
                external_count
            );
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.contains("INET_DIAG_DESTROY") || stderr.contains("Operation not supported") {
                eprintln!("[fc-agent] ss -K not supported, trying conntrack");
                kill_connections_via_conntrack().await;
            } else {
                eprintln!("[fc-agent] WARNING: ss -K failed: {}", stderr);
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: ss -K error: {}", e);
        }
    }

    sleep(Duration::from_millis(10)).await;
}

async fn kill_connections_via_conntrack() {
    let output = Command::new("conntrack").args(["-F"]).output().await;

    match output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] flushed conntrack table");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.contains("No such file") {
                eprintln!("[fc-agent] conntrack flush: {}", stderr.trim());
            }
        }
        Err(_) => {} // conntrack not available
    }
}

/// Configure DNS from kernel ip= boot parameter.
pub fn configure_dns_from_cmdline() {
    eprintln!("[fc-agent] configuring DNS from kernel cmdline");

    let cmdline = match std::fs::read_to_string("/proc/cmdline") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to read /proc/cmdline: {}", e);
            return;
        }
    };
    eprintln!("[fc-agent] cmdline: {}", cmdline.trim());

    let ip_param = cmdline
        .split_whitespace()
        .find(|s| s.starts_with("ip="))
        .map(|s| s.trim_start_matches("ip="));

    let ip_param = match ip_param {
        Some(p) => p,
        None => {
            eprintln!("[fc-agent] WARNING: no ip= parameter in cmdline, skipping DNS config");
            return;
        }
    };
    eprintln!("[fc-agent] ip param: {}", ip_param);

    let fields: Vec<&str> = ip_param.split(':').collect();
    eprintln!("[fc-agent] ip fields: {:?}", fields);

    let gateway = fields.get(2).copied().unwrap_or("");
    let dns = fields.get(7).copied().unwrap_or("");

    eprintln!("[fc-agent] gateway={}, dns={}", gateway, dns);

    let nameserver = if !dns.is_empty() {
        dns
    } else if !gateway.is_empty() {
        gateway
    } else {
        eprintln!("[fc-agent] WARNING: no DNS or gateway found, skipping DNS config");
        return;
    };

    let nameservers: Vec<String> = cmdline
        .split_whitespace()
        .find(|s| s.starts_with("fcvm_dns="))
        .map(|s| {
            s.trim_start_matches("fcvm_dns=")
                .split('|')
                .map(|ns| ns.to_string())
                .collect()
        })
        .unwrap_or_else(|| vec![nameserver.to_string()]);

    let search_domains: Option<String> = cmdline
        .split_whitespace()
        .find(|s| s.starts_with("fcvm_dns_search="))
        .map(|s| s.trim_start_matches("fcvm_dns_search=").replace('|', " "));

    let mut resolv_conf = String::new();
    if let Some(ref search) = search_domains {
        resolv_conf.push_str(&format!("search {}\n", search));
    }
    for ns in &nameservers {
        resolv_conf.push_str(&format!("nameserver {}\n", ns));
    }

    match std::fs::write("/etc/resolv.conf", &resolv_conf) {
        Ok(_) => {
            eprintln!("[fc-agent] configured DNS: {}", resolv_conf.trim());
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to write /etc/resolv.conf: {}",
                e
            );
        }
    }
}

/// Configure IPv6 from kernel ipv6= boot parameter.
pub fn configure_ipv6_from_cmdline() {
    eprintln!("[fc-agent] checking for IPv6 configuration");

    let cmdline = match std::fs::read_to_string("/proc/cmdline") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to read /proc/cmdline: {}", e);
            return;
        }
    };

    let ipv6_param = cmdline
        .split_whitespace()
        .find(|s| s.starts_with("ipv6="))
        .map(|s| s.trim_start_matches("ipv6="));

    let ipv6_param = match ipv6_param {
        Some(p) => p,
        None => {
            eprintln!("[fc-agent] no ipv6= parameter, IPv6 not configured");
            return;
        }
    };
    eprintln!("[fc-agent] ipv6 param: {}", ipv6_param);

    let parts: Vec<&str> = ipv6_param.split('|').collect();
    if parts.len() != 2 {
        eprintln!("[fc-agent] WARNING: invalid ipv6= format, expected <client>|<gateway>");
        return;
    }
    let client_raw = parts[0];
    let gateway = parts[1];

    // Parse optional prefix length from client address (e.g., "addr/128" or just "addr").
    // Routed mode passes /128 (VM's only neighbor is the bridge, no on-link /64 needed).
    // Pasta mode omits the prefix, defaulting to /64 (needed for on-link NDP to fd00::2).
    let (client, prefix_len) = if let Some((addr, pfx)) = client_raw.split_once('/') {
        (addr, pfx)
    } else {
        (client_raw, "64")
    };

    eprintln!(
        "[fc-agent] IPv6: client={}, prefix=/{}, gateway={}",
        client, prefix_len, gateway
    );

    // nodad: DAD is pointless on a Firecracker TAP (single endpoint) and adds
    // 1-2s of "tentative" state that blocks IPv6 traffic.
    let addr_cidr = format!("{}/{}", client, prefix_len);
    let addr_output = std::process::Command::new("ip")
        .args(["-6", "addr", "add", &addr_cidr, "dev", "eth0", "nodad"])
        .output();

    match addr_output {
        Ok(output) if output.status.success() => {
            eprintln!("[fc-agent] added IPv6 address {} to eth0", addr_cidr);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("File exists") {
                eprintln!("[fc-agent] IPv6 address already exists on eth0");
            } else {
                eprintln!("[fc-agent] WARNING: failed to add IPv6 address: {}", stderr);
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to run ip -6 addr add: {}", e);
        }
    }

    // Use "replace" not "add" — RA may have already installed a default route.
    // "onlink" skips NDP reachability check — needed when the bridge gateway's
    // DAD hasn't completed yet at the time fc-agent runs.
    let route_output = std::process::Command::new("ip")
        .args([
            "-6", "route", "replace", "default", "via", gateway, "dev", "eth0", "onlink",
        ])
        .output();

    match route_output {
        Ok(output) if output.status.success() => {
            eprintln!("[fc-agent] added IPv6 default route via {}", gateway);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("File exists") {
                eprintln!("[fc-agent] IPv6 default route already exists");
            } else {
                eprintln!("[fc-agent] WARNING: failed to add IPv6 route: {}", stderr);
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to run ip -6 route add: {}", e);
        }
    }
}

/// Reconfigure the guest's IPv6 address on eth0 after snapshot restore.
///
/// Replaces the snapshot's shared guest IPv6 with the unique per-clone address.
/// This is called during handle_clone_restore(), BEFORE any network traffic
/// can use the old address. Uses `ip addr replace` semantics: find the current
/// IPv6, remove it, add the new one.
pub async fn reconfigure_ipv6(new_ipv6: &str) {
    eprintln!("[fc-agent] reconfiguring IPv6: new address = {}", new_ipv6);

    // Find current global IPv6 on eth0
    let output = tokio::process::Command::new("ip")
        .args(["-6", "addr", "show", "dev", "eth0", "scope", "global"])
        .output()
        .await;

    let old_addr = match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Parse "inet6 <addr>/<prefix> scope global" line
            stdout.lines().find_map(|line| {
                let line = line.trim();
                line.strip_prefix("inet6 ")
                    .and_then(|rest| rest.split_whitespace().next().map(|s| s.to_string()))
            })
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to list IPv6 addrs: {}", e);
            None
        }
    };

    // Remove old address if found
    if let Some(ref old) = old_addr {
        let del = tokio::process::Command::new("ip")
            .args(["-6", "addr", "del", old, "dev", "eth0"])
            .output()
            .await;
        match del {
            Ok(out) if out.status.success() => {
                eprintln!("[fc-agent] removed old IPv6 {} from eth0", old);
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!("[fc-agent] WARNING: failed to remove old IPv6: {}", stderr);
            }
            Err(e) => {
                eprintln!("[fc-agent] WARNING: ip addr del failed: {}", e);
            }
        }
    }

    // Add new address with nodad — DAD is pointless on a Firecracker TAP (only
    // one endpoint) and the 1-2s tentative period prevents the VM from using
    // the address, causing IPv6 connectivity failures in tests.
    let add = tokio::process::Command::new("ip")
        .args([
            "-6",
            "addr",
            "add",
            &format!("{}/128", new_ipv6),
            "dev",
            "eth0",
            "nodad",
        ])
        .output()
        .await;
    match add {
        Ok(out) if out.status.success() => {
            eprintln!("[fc-agent] added new IPv6 {}/128 to eth0", new_ipv6);
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("[fc-agent] WARNING: failed to add new IPv6: {}", stderr);
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: ip addr add failed: {}", e);
        }
    }
}

/// Forward specific localhost ports to host gateway via TCP proxy.
///
/// iptables DNAT doesn't work with pasta networking: DNAT'd packets retain
/// their 127.0.0.1 source address, which pasta's L4 translation can't handle
/// (loopback source going through an external TAP device). Instead, we spawn
/// a TCP proxy for each port: listen on 127.0.0.1:port inside the VM and
/// forward connections to 10.0.2.2:port (the gateway). Pasta's default
/// --map-host-loopback maps gateway traffic to the host's 127.0.0.1.
pub fn setup_localhost_forwarding(ports: &[String]) {
    for port_str in ports {
        let port: u16 = match port_str.parse() {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARNING: invalid forward-localhost port '{}': {}",
                    port_str, e
                );
                continue;
            }
        };

        let listener = match std::net::TcpListener::bind(format!("127.0.0.1:{}", port)) {
            Ok(l) => {
                eprintln!("[fc-agent] localhost proxy listening on 127.0.0.1:{}", port);
                l
            }
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARNING: failed to bind 127.0.0.1:{}: {}",
                    port, e
                );
                continue;
            }
        };

        listener.set_nonblocking(true).ok();
        let tokio_listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARNING: failed to create async listener for port {}: {}",
                    port, e
                );
                continue;
            }
        };

        tokio::spawn(async move {
            loop {
                match tokio_listener.accept().await {
                    Ok((client, _)) => {
                        tokio::spawn(proxy_connection(client, port));
                    }
                    Err(e) => {
                        eprintln!(
                            "[fc-agent] WARNING: accept failed on localhost:{}: {}",
                            port, e
                        );
                    }
                }
            }
        });
    }
    if !ports.is_empty() {
        eprintln!(
            "[fc-agent] forwarding localhost ports to host gateway: {:?}",
            ports
        );
    }
}

/// Proxy a single TCP connection from localhost to the gateway (10.0.2.2).
async fn proxy_connection(mut client: tokio::net::TcpStream, port: u16) {
    let mut upstream = match tokio::net::TcpStream::connect(format!("10.0.2.2:{}", port)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to connect to gateway 10.0.2.2:{}: {}",
                port, e
            );
            return;
        }
    };
    if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        if e.kind() != std::io::ErrorKind::ConnectionReset
            && e.kind() != std::io::ErrorKind::BrokenPipe
        {
            eprintln!("[fc-agent] localhost proxy port {}: {}", port, e);
        }
    }
}

/// Guest-side port of the transparent egress proxy (`proxy.rs::PROXY_LISTEN_PORT`).
///
/// Kept in sync deliberately rather than imported, so the compiler points here if
/// that constant moves: this list is a SECURITY exclusion, not a convenience.
const EGRESS_PROXY_PORT: u16 = 12345;

/// Make every published guest port reachable even when the service binds
/// 127.0.0.1 only.
///
/// # Why this exists
///
/// `--publish` delivers a packet to the guest's external interface. A service
/// listening on `0.0.0.0` receives it; one listening on `127.0.0.1` does not,
/// and there was no way to reach it — `--forward-localhost` runs the other
/// direction (guest -> host), so aiming it at such a port HIJACKS the guest's own
/// listener rather than exposing it.
///
/// The motivating case is Chromium's DevTools endpoint. Measured on chromium
/// 151.0.7922.71 (Debian bookworm arm64): with `--remote-debugging-address=0.0.0.0`
/// confirmed present in `/proc/<pid>/cmdline`, `/proc/net/tcp` shows
/// `0100007F:2406` (127.0.0.1:9222) and nothing else. The browser ignores the
/// flag, so the only previous route in was a userspace relay running inside the
/// guest — an extra process per clone, in the byte path, on the one arm that
/// dropped connections.
///
/// # Why it is unconditional
///
/// Rewriting the destination breaks one case: a service bound to the guest's OWN
/// address (`10.0.2.100:P`), whose socket stops matching. Netfilter can express
/// the exception — `-m socket --nowildcard -j RETURN` — but the shipped guest
/// kernel cannot run it. Measured in the guest (Kata 6.12.47, iptables 1.8.10 nft):
///
/// ```text
/// # iptables -t nat -A PREROUTING -p tcp --dport N -m socket -j RETURN
/// Warning: Extension socket revision 0 not supported, missing kernel module?
/// RULE_APPEND failed (No such file or directory): rule in chain PREROUTING
/// # modprobe xt_socket
/// FATAL: Module xt_socket not found in directory /lib/modules/6.12.47
/// ```
///
/// `/lib/modules` in the guest holds the ROOTFS's module tree, not the running
/// kernel's, so no netfilter match can be loaded at runtime — the Kata kernel is
/// built-in-only from here. Inferring the exception in userspace instead (poll
/// `/proc/net/tcp`, add/remove the rule as listeners change) was written and
/// rejected: there is no "safe" moment to decide, because the address a service
/// will bind does not exist until it binds, so every such design has a window.
///
/// The exception is also close to unreachable HERE. The guest netns has exactly
/// one external interface with exactly one IPv4 address (measured: `eth0`
/// `10.0.2.100/24` plus `lo`), so binding it and binding the wildcard are
/// equivalent — a specific bind exists to EXCLUDE other interfaces and there are
/// none. The hostname resolves to nothing (`/etc/hosts` has only `127.0.0.1
/// localhost`), so `InetAddress.getLocalHost()` and friends throw rather than
/// returning the guest address. No listener in this repo binds a specific
/// non-loopback address.
///
/// # Known limits
///
/// - **TCP only.** `--publish X:P/udp` to a loopback-bound UDP service stays
///   unreachable.
/// - **`127.0.0.1` exactly.** A service on another `127.0.0.0/8` address
///   (`127.0.1.1`, which Debian's hostname convention and Akka/Pekko produce)
///   is not matched by this rewrite. It is unreachable, exactly as before.
/// - **A service bound to `10.0.2.100:P` loses `--publish`.** See above for why
///   that is not reachable in practice.
///
/// # Why DNAT works HERE and not in `setup_localhost_forwarding`
///
/// That function documents that DNAT is unusable for guest -> host, because the
/// DNAT'd packet keeps its `127.0.0.1` SOURCE and pasta's L4 translation cannot
/// carry a loopback source out through the TAP. This is the opposite direction:
/// the source stays external and only the DESTINATION becomes loopback, and
/// conntrack rewrites the reply's source back to the original destination before
/// it leaves. Nothing downstream ever sees a loopback source address.
///
/// # Security
///
/// `route_localnet=1` is what makes a loopback destination routable for a packet
/// that arrived on `eth0` — and on its own it does that for the guest's ENTIRE
/// loopback listener set, not just the published ports. That is strictly wider
/// than `--publish` and is NOT acceptable on the strength of "the microVM guards
/// ingress", so `install_loopback_containment` closes it: anything arriving on
/// `eth0` addressed to `127.0.0.0/8` is dropped unless conntrack says WE
/// translated it. Only deliberately published ports get through.
///
/// The egress proxy's own port is excluded from publishing entirely. It listens
/// on `127.0.0.1:12345` inside the guest and relays through the host, and a
/// published DNAT would hand external clients a host-side `connect()` with no
/// destination validation and no timeout.
pub fn publish_to_loopback(ports: &[String]) {
    let mut wanted: Vec<u16> = Vec::new();
    for spec in ports {
        match spec.parse::<u16>() {
            Ok(EGRESS_PROXY_PORT) => eprintln!(
                "[fc-agent] WARNING: refusing to publish port {EGRESS_PROXY_PORT} to guest \
                 loopback — it is the transparent egress proxy's listener, and exposing it \
                 would let an external client drive host-side connections. Publish a \
                 different guest port."
            ),
            Ok(p) => wanted.push(p),
            Err(e) => eprintln!("[fc-agent] WARNING: invalid published port '{spec}': {e}"),
        }
    }
    if wanted.is_empty() {
        return;
    }

    enable_route_localnet();

    // BEFORE the DNATs, so there is no window in which route_localnet is on and
    // the containment is not.
    install_loopback_containment();

    for port in wanted {
        match run_iptables(&loopback_dnat_args("-A", port)) {
            Ok(()) => eprintln!("[fc-agent] published port {port} also reaches 127.0.0.1:{port}"),
            Err(e) => eprintln!(
                "[fc-agent] WARNING: DNAT for published port {port} failed: {e}. A service \
                 bound to 127.0.0.1:{port} will not be reachable."
            ),
        }
    }
}

/// `iptables -w -t nat <op> PREROUTING -p tcp --dport P -j DNAT --to 127.0.0.1:P`.
///
/// PREROUTING so the rewrite happens before the routing decision, which is what
/// lets 127.0.0.1 be a valid destination for a packet that arrived on eth0.
/// `-w` takes the xtables lock rather than racing another caller.
fn loopback_dnat_args(op: &str, port: u16) -> Vec<String> {
    vec![
        "-w".into(),
        "-t".into(),
        "nat".into(),
        op.into(),
        "PREROUTING".into(),
        "-p".into(),
        "tcp".into(),
        "--dport".into(),
        port.to_string(),
        "-j".into(),
        "DNAT".into(),
        "--to-destination".into(),
        format!("127.0.0.1:{port}"),
    ]
}

/// Drop anything arriving on `eth0` for `127.0.0.0/8` that we did not translate.
///
/// `route_localnet` is per-interface, not per-port: switching it on makes every
/// loopback listener in the guest reachable from the L2 segment, which is much
/// more than `--publish` opened. `-m conntrack ! --ctstate DNAT` distinguishes the
/// packets our PREROUTING rules rewrote (conntrack records the translation) from
/// ones an attacker addressed to `127.0.0.1` directly. `xt_conntrack` IS built
/// into the Kata kernel — verified in the guest, unlike `xt_socket`.
///
/// Scoped to `-i eth0`: locally-generated traffic re-enters on `lo` and is never
/// matched, so guest-internal loopback use and the egress proxy's `nat OUTPUT`
/// REDIRECT are untouched.
fn install_loopback_containment() {
    let args = containment_args("-A");
    match run_iptables(&args) {
        Ok(()) => eprintln!(
            "[fc-agent] loopback containment installed: only DNAT'd connections reach 127.0.0.0/8 \
             from eth0"
        ),
        Err(e) => eprintln!(
            "[fc-agent] WARNING: could not install loopback containment ({e}). route_localnet is \
             on, so every guest loopback listener — not just published ports — is reachable from \
             the guest's network segment."
        ),
    }
}

fn containment_args(op: &str) -> Vec<String> {
    vec![
        "-w".into(),
        op.into(),
        "INPUT".into(),
        "-i".into(),
        "eth0".into(),
        "-d".into(),
        "127.0.0.0/8".into(),
        "-m".into(),
        "conntrack".into(),
        "!".into(),
        "--ctstate".into(),
        "DNAT".into(),
        "-j".into(),
        "DROP".into(),
    ]
}

fn run_iptables(args: &[String]) -> Result<(), String> {
    match std::process::Command::new("iptables").args(args).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Without this the kernel treats a packet routed to 127.0.0.0/8 that arrived on
/// a non-loopback interface as a martian source and drops it.
///
/// `eth0` only, deliberately. `all` would also cover interfaces created LATER —
/// a bridge a nested workload brings up — which is broader than anything
/// `--publish` implies, and the containment rule above is scoped to `eth0`.
fn enable_route_localnet() {
    let key = "net.ipv4.conf.eth0.route_localnet";
    match std::process::Command::new("sysctl")
        .arg("-w")
        .arg(format!("{key}=1"))
        .output()
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!(
            "[fc-agent] WARNING: {key}=1 failed: {}. Published ports will not reach a \
             loopback-bound service.",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("[fc-agent] WARNING: could not run sysctl for {key}: {e}"),
    }
}

#[cfg(test)]
mod loopback_publish_tests {
    use super::*;

    /// The command that gets run IS the feature; a typo here is invisible until a
    /// VM silently fails to publish.
    #[test]
    fn dnat_rewrites_only_the_destination_and_keeps_the_port() {
        assert_eq!(
            loopback_dnat_args("-A", 9222).join(" "),
            "-w -t nat -A PREROUTING -p tcp --dport 9222 -j DNAT --to-destination 127.0.0.1:9222"
        );
        // The source is never touched: conntrack un-NATs the reply, so nothing
        // downstream (pasta's L4 translation, the bridged veth) sees a loopback
        // source. See setup_localhost_forwarding for why the reverse direction
        // cannot do this.
        let args = loopback_dnat_args("-A", 9222).join(" ");
        assert!(
            !args.contains("--to-source") && !args.contains("SNAT"),
            "{args}"
        );
    }

    /// Containment is the difference between "published ports reach loopback" and
    /// "the guest's whole loopback surface is on the wire". Without `! --ctstate
    /// DNAT` the rule would drop the published traffic too; without `-i eth0` it
    /// would break guest-internal loopback.
    #[test]
    fn containment_drops_only_untranslated_wire_traffic() {
        let args = containment_args("-A").join(" ");
        assert_eq!(
            args,
            "-w -A INPUT -i eth0 -d 127.0.0.0/8 -m conntrack ! --ctstate DNAT -j DROP"
        );
        assert!(
            args.contains("! --ctstate DNAT"),
            "must let OUR translations through: {args}"
        );
        assert!(
            args.contains("-i eth0"),
            "must not touch locally-generated loopback: {args}"
        );
    }

    /// The egress proxy listens on 127.0.0.1:12345 in the guest and relays via the
    /// host. Publishing it hands an external client a host-side connect() with no
    /// destination validation and no timeout, so it must never get a DNAT.
    #[test]
    fn the_egress_proxy_port_is_never_published() {
        assert_eq!(
            EGRESS_PROXY_PORT, 12345,
            "must track proxy.rs::PROXY_LISTEN_PORT; if that moved, this exclusion is dead"
        );
    }
}
