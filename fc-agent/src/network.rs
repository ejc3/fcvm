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

/// Send gratuitous ARP via ping to teach new slirp4netns our MAC address.
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
        eprintln!("[fc-agent] WARNING: could not determine gateway for gratuitous ARP");
        return;
    };

    eprintln!("[fc-agent] sending gratuitous ARP to gateway {}", gateway);

    let ping_output = Command::new("ping")
        .args(["-c", "1", "-W", "1", &gateway])
        .output()
        .await;

    match ping_output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] gratuitous ARP sent (pinged gateway)");
        }
        Ok(o) => {
            eprintln!(
                "[fc-agent] gratuitous ARP sent (ping returned: {})",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to send gratuitous ARP: {}", e);
        }
    }
}

/// Kill all established TCP connections — dead after snapshot restore.
pub async fn kill_stale_tcp_connections() {
    let list_output = Command::new("ss")
        .args(["-tn", "state", "established"])
        .output()
        .await;

    if let Ok(o) = &list_output {
        let connections = String::from_utf8_lossy(&o.stdout);
        let count = connections.lines().count().saturating_sub(1);
        if count > 0 {
            eprintln!(
                "[fc-agent] found {} established TCP connection(s) to kill",
                count
            );
            for line in connections.lines().skip(1) {
                eprintln!("[fc-agent]   {}", line);
            }
        } else {
            eprintln!("[fc-agent] no established TCP connections to kill");
            return;
        }
    }

    let kill_output = Command::new("ss")
        .args(["-K", "state", "established"])
        .output()
        .await;

    match kill_output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] killed all established TCP connections");
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
    let client = parts[0];
    let gateway = parts[1];

    eprintln!("[fc-agent] IPv6: client={}, gateway={}", client, gateway);

    let addr_output = std::process::Command::new("ip")
        .args([
            "-6",
            "addr",
            "add",
            &format!("{}/64", client),
            "dev",
            "eth0",
        ])
        .output();

    match addr_output {
        Ok(output) if output.status.success() => {
            eprintln!("[fc-agent] added IPv6 address {}/64 to eth0", client);
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

    let route_output = std::process::Command::new("ip")
        .args([
            "-6", "route", "add", "default", "via", gateway, "dev", "eth0",
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

/// Set up iptables DNAT to forward specific localhost ports to host gateway.
pub fn setup_localhost_forwarding(ports: &[String]) {
    let _ = std::process::Command::new("sysctl")
        .args(["-w", "net.ipv4.conf.all.route_localnet=1"])
        .output();
    let _ = std::process::Command::new("sysctl")
        .args(["-w", "net.ipv6.conf.lo.disable_ipv6=1"])
        .output();
    for port in ports {
        let _ = std::process::Command::new("iptables")
            .args([
                "-t",
                "nat",
                "-A",
                "OUTPUT",
                "-d",
                "127.0.0.0/8",
                "-p",
                "tcp",
                "--dport",
                port,
                "-j",
                "DNAT",
                "--to-destination",
                "10.0.2.2",
            ])
            .output();
    }
    eprintln!("[fc-agent] forwarding localhost ports to host: {:?}", ports);
}
