use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::types::{PortMapping, Protocol};

/// Refuse a mapping whose HOSTIP is an IPv6 address.
///
/// Bridged port forwarding is IPv4 DNAT through `iptables`, which has no rule for an
/// IPv6 destination. Without this the mapping fails as an iptables parse error, or is
/// rescoped to the veth address and carries on as if no address had been named.
pub fn require_ipv4_host_addresses(mappings: &[PortMapping]) -> Result<()> {
    for mapping in mappings {
        let Some(host_ip) = mapping.host_ip.as_deref() else {
            continue;
        };
        if host_ip.parse::<std::net::Ipv6Addr>().is_ok() {
            anyhow::bail!(
                "bridged networking cannot publish on the IPv6 host address [{host_ip}]:{}: \
                 its port forwarding is IPv4 iptables DNAT. Use --network routed or \
                 --network rootless for an IPv6 host address.",
                mapping.host_port
            );
        }
    }
    Ok(())
}

/// The iptables rules that publish `mappings` on `guest_ip`, as `-A` rule
/// specifications, four per mapping and in the order they are added: DNAT for traffic
/// arriving from outside, DNAT for locally generated traffic, MASQUERADE so the guest's
/// reply returns, and FORWARD to let the traffic through.
pub fn port_mapping_rules(guest_ip: &str, mappings: &[PortMapping]) -> Vec<String> {
    let mut rules = Vec::with_capacity(mappings.len() * 4);
    for mapping in mappings {
        let proto = match mapping.proto {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        };
        // If host_ip is specified, only match packets destined to that IP (security:
        // prevents exposing on all interfaces).
        let destination = mapping
            .host_ip
            .as_ref()
            .map(|host_ip| format!("-d {host_ip} "))
            .unwrap_or_default();
        for chain in ["PREROUTING", "OUTPUT"] {
            rules.push(format!(
                "-t nat -A {chain} {destination}-p {proto} --dport {} -j DNAT --to-destination {guest_ip}:{}",
                mapping.host_port, mapping.guest_port
            ));
        }
        rules.push(format!(
            "-t nat -A POSTROUTING -d {guest_ip} -p {proto} --dport {} -j MASQUERADE",
            mapping.guest_port
        ));
        rules.push(format!(
            "-A FORWARD -p {proto} -d {guest_ip} --dport {} -j ACCEPT",
            mapping.guest_port
        ));
    }
    rules
}

/// One of [`port_mapping_rules`], as the record that lets a later launch remove it
/// (`crate::reclaim`).
pub fn rule_resource(rule: &str) -> crate::reclaim::Resource {
    crate::reclaim::Resource::Rule {
        program: "iptables".to_string(),
        args: rule.split_whitespace().map(str::to_string).collect(),
    }
}

/// Sets up port mapping rules for a VM
///
/// Adds the rules of [`port_mapping_rules`]. Returns them, for cleanup. A rule that
/// cannot be added removes the ones added before it.
pub async fn setup_port_mappings(guest_ip: &str, mappings: &[PortMapping]) -> Result<Vec<String>> {
    if mappings.is_empty() {
        return Ok(Vec::new());
    }

    debug!(
        guest_ip = %guest_ip,
        mappings = mappings.len(),
        "setting up port mappings"
    );

    let mut created_rules: Vec<String> = Vec::new();
    for rule in port_mapping_rules(guest_ip, mappings) {
        let added = Command::new("iptables")
            .args(rule.split_whitespace())
            .output()
            .await
            .with_context(|| format!("adding iptables rule {rule}"));
        let failure = match added {
            Ok(output) if output.status.success() => None,
            Ok(output) => Some(anyhow::anyhow!(
                "failed to add iptables rule {rule}: {}",
                String::from_utf8_lossy(&output.stderr)
            )),
            Err(error) => Some(error),
        };
        if let Some(error) = failure {
            for rule in created_rules.iter().rev() {
                let _ = delete_rule(rule).await;
            }
            return Err(error);
        }
        created_rules.push(rule);
    }
    for mapping in mappings {
        info!(
            host_port = mapping.host_port,
            guest_port = mapping.guest_port,
            proto = ?mapping.proto,
            "port mapping created"
        );
    }
    Ok(created_rules)
}

/// Enables route_localnet on a network interface
///
/// This is required for localhost port forwarding to work. By default, Linux
/// doesn't route packets with 127.0.0.0/8 source to external interfaces.
/// Enabling route_localnet allows DNAT'd packets from localhost to be routed
/// to the guest VM.
pub async fn enable_route_localnet(interface: &str) -> Result<()> {
    let sysctl_path = format!("net.ipv4.conf.{}.route_localnet", interface);

    let output = Command::new("sysctl")
        .args(["-w", &format!("{}=1", sysctl_path)])
        .output()
        .await
        .with_context(|| format!("enabling route_localnet on {}", interface))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            "failed to enable route_localnet on {}: {}",
            interface, stderr
        );
    } else {
        info!(
            interface = %interface,
            "enabled route_localnet for localhost port forwarding"
        );
    }

    Ok(())
}

/// Deletes a single iptables rule
///
/// Converts an -A (append) rule to -D (delete) and executes it.
async fn delete_rule(rule: &str) -> Result<()> {
    let delete_rule = to_delete_rule(rule);

    let output = crate::utils::helper_command("iptables")
        .args(delete_rule.split_whitespace())
        .output()
        .await
        .context("deleting iptables rule")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // "No chain/target/match", "Bad rule": the rule is gone already.
        if !stderr.contains("No chain")
            && !stderr.contains("does not exist")
            && !stderr.contains("Bad rule")
        {
            anyhow::bail!("deleting iptables rule {rule}: {}", stderr.trim());
        }
    }

    Ok(())
}

fn to_delete_rule(rule: &str) -> String {
    if let Some(rest) = rule.strip_prefix("-A ") {
        format!("-D {}", rest)
    } else {
        rule.replacen(" -A ", " -D ", 1)
    }
}

/// Cleans up port mapping rules for a VM
///
/// Takes the list of rules returned by setup_port_mappings() and removes them.
/// Rules are deleted in reverse order for proper cleanup.
pub async fn cleanup_port_mappings(rules: &[String]) -> Result<()> {
    if rules.is_empty() {
        return Ok(());
    }

    debug!(rules = rules.len(), "cleaning up port mapping rules");

    // Delete in reverse order, every one, and report the ones that stayed.
    let mut failures = Vec::new();
    for rule in rules.iter().rev() {
        if let Err(e) = delete_rule(rule).await {
            warn!(rule = %rule, error = %e, "failed to delete port mapping rule");
            failures.push(format!("{e:#}"));
        }
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

/// Comment used to tag iptables rules that fcvm added.
///
/// `cleanup_global_nat_if_unused()` only deletes rules carrying this comment, so
/// fcvm never removes NAT configuration an admin or another tool put in place.
const FCVM_RULE_COMMENT: &str = "fcvm-bridged";

/// The subnets bridged guests send from: baseline guests' 172.30.x.y, and the
/// 10.x.y.2 namespace side of a clone's veth.
pub const BRIDGED_EGRESS_SUBNETS: [&str; 2] = ["172.30.0.0/16", "10.0.0.0/8"];

/// The `iptables -t nat` rule (after the chain name) that translates bridged guests'
/// traffic from `subnet` leaving through `iface` (#1735).
///
/// The source match is a subnet the host's own address can be in: 10.0.0.0/8 holds
/// many cloud hosts' VPC address, and a MASQUERADE on it rewrote every connection the
/// host opened. So the rule never matches an address the host owns (`addrtype !
/// --src-type LOCAL`); a guest's address, forwarded through the host, is never one.
/// And it translates to `source`, the default route's source address, which the
/// host's own connections leave from: MASQUERADE takes the uplink's primary address
/// for the next hop's subnet instead, and on a host whose uplink lists another
/// address first (a secondary private address with no public mapping) the guests'
/// traffic left from an address nothing answers. Without a route source the kernel's
/// own choice is the host's choice too, so MASQUERADE is kept.
pub fn global_nat_rule(subnet: &str, iface: &str, source: Option<&str>) -> Vec<String> {
    let mut rule: Vec<String> = [
        "-s",
        subnet,
        "-o",
        iface,
        "-m",
        "addrtype",
        "!",
        "--src-type",
        "LOCAL",
        "-m",
        "comment",
        "--comment",
        FCVM_RULE_COMMENT,
        "-j",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match source {
        Some(source) => rule.extend(["SNAT".into(), "--to-source".into(), source.to_string()]),
        None => rule.push("MASQUERADE".into()),
    }
    rule
}

/// The `src` of `ip -4 route show default` output: the address the host's own
/// connections leave from.
pub fn parse_default_route_source(stdout: &str) -> Option<String> {
    let words: Vec<&str> = stdout.lines().next()?.split_whitespace().collect();
    let at = words.iter().position(|w| *w == "src")?;
    let source = words.get(at + 1)?;
    source.parse::<std::net::Ipv4Addr>().ok()?;
    Some(source.to_string())
}

/// Ensures global NAT is enabled for VM traffic
///
/// Sets up:
/// 1. Verifies IP forwarding is enabled (errors if not — admin must configure)
/// 2. Enables per-interface forwarding on the outbound interface
/// 3. MASQUERADE rule for outbound traffic from VM subnet (tagged with an
///    fcvm comment so cleanup only ever removes rules fcvm created)
///
/// The check-then-add of the MASQUERADE rule runs under the host-level
/// `bridged-nat.lock`, serializing it against `cleanup_global_nat_if_unused()`
/// in other fcvm processes. Without the lock, a stopping VM that has already
/// listed interfaces (and saw no veths) could delete the rule right after this
/// check sees it, leaving this VM without outbound NAT.
///
/// This should be called once during fcvm initialization, not per-VM.
pub async fn ensure_global_nat(vm_subnet: &str) -> Result<()> {
    debug!(subnet = %vm_subnet, "ensuring global NAT configuration");

    // Verify IP forwarding is enabled (we don't set it — that's a system-wide
    // setting the admin should configure, e.g. via sysctl.conf or cloud-init)
    let output = Command::new("sysctl")
        .args(["-n", "net.ipv4.ip_forward"])
        .output()
        .await
        .context("checking IP forwarding")?;

    if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() != "1" {
        anyhow::bail!(
            "IP forwarding is disabled. Bridged networking requires net.ipv4.ip_forward=1.\n\
             Enable it with: sudo sysctl -w net.ipv4.ip_forward=1\n\
             To persist across reboots: echo 'net.ipv4.ip_forward=1' | sudo tee /etc/sysctl.d/99-ip-forward.conf"
        );
    }

    // Serialize the check-then-add against cleanup_global_nat_if_unused() running
    // in a concurrently stopping fcvm process, and against other launches.
    let _nat_lock = super::acquire_host_network_lock("bridged-nat.lock")
        .await
        .context("acquiring bridged NAT lock")?;

    // The outbound interface and the source address the host's own connections leave
    // from, read together from one default route under the lock: a launch that read
    // them earlier could hold an interface the default route has since left, and its
    // replacement below would delete the rule a later launch made for the current one.
    let route = Command::new("ip")
        .args(["-4", "route", "show", "default"])
        .output()
        .await
        .context("reading the default route")?;
    let route = String::from_utf8_lossy(&route.stdout);
    let outbound_iface = parse_default_interface(&route)?;
    let source = parse_default_route_source(&route);
    let rule = global_nat_rule(vm_subnet, &outbound_iface, source.as_deref());

    // Enable forwarding on the outbound interface specifically
    // (per-interface forwarding may be disabled even when global ip_forward=1)
    let iface_forwarding = format!("net.ipv4.conf.{}.forwarding=1", outbound_iface);
    let output = Command::new("sysctl")
        .args(["-w", &iface_forwarding])
        .output()
        .await
        .with_context(|| format!("enabling forwarding on {}", outbound_iface))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            "failed to enable forwarding on {}: {}",
            outbound_iface, stderr
        );
    }

    place_global_nat_rule(vm_subnet, &rule, &mut |args| {
        Box::pin(async move {
            let output = Command::new("iptables")
                .args(&args)
                .output()
                .await
                .with_context(|| format!("running iptables {args:?}"))?;
            Ok(IptablesRun {
                ok: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    })
    .await?;

    debug!("global NAT configuration complete");
    Ok(())
}

/// Removes global NAT rules if no bridged VMs are running
///
/// Checks for veth0-* interfaces (indicates active bridged VMs).
/// If none exist, removes the fcvm-tagged MASQUERADE rules for both subnets
/// (rules without the fcvm comment were added by someone else and are left alone).
/// IP forwarding is intentionally left enabled (other services may depend on it).
/// Best-effort — logs warnings but doesn't fail.
///
/// The list-then-delete sequence runs under the host-level `bridged-nat.lock`,
/// serializing it against `ensure_global_nat()` in concurrently starting fcvm
/// processes so this never deletes a MASQUERADE rule another VM just confirmed.
pub async fn cleanup_global_nat_if_unused() -> Result<()> {
    // Serialize against ensure_global_nat() in other fcvm processes.
    let _nat_lock = super::acquire_host_network_lock("bridged-nat.lock")
        .await
        .context("acquiring the bridged NAT lock; global NAT rules left in place")?;

    // Check if any veth0- interfaces exist (active bridged VMs)
    let output = crate::utils::helper_command("ip")
        .args(["-o", "link", "show"])
        .output()
        .await
        .context("listing interfaces to see whether bridged VMs remain")?;
    anyhow::ensure!(
        output.status.success(),
        "`ip -o link show` failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.lines().any(|line| line.contains("veth0-")) {
        debug!("other bridged VMs still running, keeping global NAT rules");
        return Ok(());
    }

    info!("no bridged VMs running, cleaning up global NAT rules");

    // Remove every fcvm-tagged rule for the guests' subnets, whatever its interface
    // or translation: the rule carries the source address the host's default route
    // had when it was added, which can have changed since.
    let listed = crate::utils::helper_command("iptables")
        .args(["-t", "nat", "-S", "POSTROUTING"])
        .output()
        .await
        .context("listing the NAT rules to remove")?;
    anyhow::ensure!(
        listed.status.success(),
        "`iptables -t nat -S POSTROUTING` failed: {}",
        String::from_utf8_lossy(&listed.stderr).trim()
    );
    let mut failures = Vec::new();
    for rule in global_nat_rules_in(&String::from_utf8_lossy(&listed.stdout)) {
        let output = crate::utils::helper_command("iptables")
            .args(["-t", "nat", "-D", "POSTROUTING"])
            .args(&rule)
            .output()
            .await;
        match output {
            Ok(o) if o.status.success() => debug!(?rule, "removed a bridged NAT rule"),
            Ok(o) => failures.push(format!(
                "{rule:?}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => failures.push(format!("{rule:?}: {e}")),
        }
    }

    // Note: we intentionally do NOT disable ip_forward here.
    // Other services on the host (Docker, Kubernetes, VPNs, etc.) may depend on
    // IP forwarding being enabled. Since we can't know whether fcvm was the one
    // that enabled it, the safe default is to leave it on.

    anyhow::ensure!(
        failures.is_empty(),
        "removing the shared MASQUERADE rules failed: {}",
        failures.join("; ")
    );
    debug!("global NAT cleanup complete");
    Ok(())
}

/// The fcvm-tagged rules for the guests' subnets in `iptables -t nat -S POSTROUTING`
/// output, as the arguments that follow the chain name.
fn global_nat_rules_in(listing: &str) -> Vec<Vec<String>> {
    listing
        .lines()
        .filter_map(|line| line.strip_prefix("-A POSTROUTING "))
        .map(|rule| {
            rule.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|rule| {
            let has = |a: &str, b: &str| rule.windows(2).any(|w| w[0] == a && w[1] == b);
            has("--comment", FCVM_RULE_COMMENT)
                && BRIDGED_EGRESS_SUBNETS
                    .iter()
                    .any(|subnet| has("-s", subnet))
        })
        .collect()
}

/// What one `iptables` run returned.
pub(crate) struct IptablesRun {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs `iptables` with the given arguments.
type IptablesRunner<'a> = dyn FnMut(
        Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<IptablesRun>> + Send>>
    + Send
    + 'a;

/// How many times `place_global_nat_rule` lists the chain and places its rule.
const PLACE_ATTEMPTS: usize = 3;

/// Put `rule` in POSTROUTING for `subnet` and delete the fcvm rules it replaces.
///
/// The rule replaces any fcvm rule for this subnet that differs from it: one added
/// under an earlier route source would otherwise keep translating to an address the
/// host no longer has. It goes where the first of those stood, so it keeps that rule's
/// place among rules others added, and in before they are deleted, so no guest
/// connection leaves untranslated between. The NAT lock does not hold off other
/// firewall writers: a position from the listing can be past the end once one has
/// deleted rules, and a stale rule can be gone already. A refused insert lists the
/// chain again; a stale rule already gone is not a failure.
async fn place_global_nat_rule(
    subnet: &str,
    rule: &[String],
    run: &mut IptablesRunner<'_>,
) -> Result<()> {
    let strings = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    let mut last_refusal = String::new();
    for _attempt in 0..PLACE_ATTEMPTS {
        let listed = run(strings(&["-t", "nat", "-S", "POSTROUTING"])).await?;
        anyhow::ensure!(
            listed.ok,
            "`iptables -t nat -S POSTROUTING` failed: {}",
            listed.stderr.trim()
        );
        let (insert, stale) = global_nat_replacement(&listed.stdout, subnet, rule);
        let mut args = match insert {
            Insert::Present => None,
            Insert::At(position) => Some(
                strings(&["-t", "nat", "-I", "POSTROUTING"])
                    .into_iter()
                    .chain([position.to_string()])
                    .collect::<Vec<_>>(),
            ),
            Insert::Append => Some(strings(&["-t", "nat", "-A", "POSTROUTING"])),
        };
        if let Some(args) = args.as_mut() {
            args.extend(rule.iter().cloned());
            let inserted = run(args.clone()).await?;
            if !inserted.ok {
                last_refusal = inserted.stderr.trim().to_string();
                continue;
            }
        }
        for old in stale {
            let mut args = strings(&["-t", "nat", "-D", "POSTROUTING"]);
            args.extend(old.iter().cloned());
            let deleted = run(args).await?;
            let gone = deleted.stderr.contains("does a matching rule exist")
                || deleted.stderr.contains("Bad rule");
            anyhow::ensure!(
                deleted.ok || gone,
                "failed to remove the stale bridged NAT rule {old:?}: {}",
                deleted.stderr.trim()
            );
        }
        return Ok(());
    }
    anyhow::bail!("failed to add the bridged NAT rule {rule:?}: {last_refusal}")
}

/// Where `ensure_global_nat` puts its rule.
#[derive(Debug, PartialEq, Eq)]
enum Insert {
    /// The rule is in the chain already.
    Present,
    /// Before the rule at this 1-based position: the first fcvm rule for the subnet.
    At(usize),
    /// At the end: no fcvm rule for the subnet is in the chain.
    Append,
}

/// What `ensure_global_nat` does to the listed rules for `subnet`: where `wanted`
/// goes, and the fcvm rules for the subnet to delete because they differ from it.
fn global_nat_replacement(
    listing: &str,
    subnet: &str,
    wanted: &[String],
) -> (Insert, Vec<Vec<String>>) {
    let for_subnet = |rule: &[String]| rule.windows(2).any(|w| w[0] == "-s" && w[1] == subnet);
    let ours: Vec<Vec<String>> = global_nat_rules_in(listing)
        .into_iter()
        .filter(|rule| for_subnet(rule))
        .collect();
    // The 1-based position of the first fcvm rule for the subnet among the chain's
    // rules, as `iptables -I` counts them.
    let first = listing
        .lines()
        .filter_map(|line| line.strip_prefix("-A POSTROUTING "))
        .map(|rule| {
            rule.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .position(|rule| ours.contains(&rule));
    let insert = if ours.iter().any(|rule| rule == wanted) {
        Insert::Present
    } else if let Some(index) = first {
        Insert::At(index + 1)
    } else {
        Insert::Append
    };
    let stale = ours.into_iter().filter(|rule| rule != wanted).collect();
    (insert, stale)
}

/// Detects the default network interface for outbound traffic
pub async fn detect_default_interface() -> Result<String> {
    let output = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await
        .context("detecting default interface")?;

    if !output.status.success() {
        anyhow::bail!("failed to get default route");
    }

    parse_default_interface(&String::from_utf8_lossy(&output.stdout))
}

/// Pick the interface out of `ip route show default` output.
///
/// Empty output gets its own diagnosis rather than being reported as an
/// unparseable route: a host that routes only over IPv6 has no IPv4 default
/// route at all, and "could not detect default interface from: " with nothing
/// after the colon reads like a parser bug instead of an unsupported host.
fn parse_default_interface(stdout: &str) -> Result<String> {
    // Output format: "default via 192.168.1.1 dev eth0 ..."
    if let Some(prev) = stdout.split_whitespace().position(|p| p == "dev") {
        if let Some(iface) = stdout.split_whitespace().nth(prev + 1) {
            return Ok(iface.to_string());
        }
    }

    if stdout.trim().is_empty() {
        anyhow::bail!(
            "this host has no IPv4 default route, so bridged networking cannot \
             pick an interface to NAT through. Hosts that route only over IPv6 \
             hit this; use --network rootless there, or add an IPv4 default \
             route. (`ip route show default` printed nothing.)"
        );
    }
    anyhow::bail!("could not detect default interface from: {}", stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bridged NAT rule leaves the host's own traffic alone (#1735). Its source
    /// match is a subnet the host's own address can be in (10.20.30.40 is in
    /// 10.0.0.0/8), and a MASQUERADE there rewrote every connection the host opened
    /// to the uplink's primary address, which had no public mapping: the host lost
    /// IPv4 for as long as a bridged VM ran. The rule now excludes every address the
    /// host owns, and translates to the default route's source address, the one the
    /// host's own connections use.
    #[test]
    fn the_bridged_nat_rule_never_translates_the_hosts_own_traffic() {
        let has = |rule: &[String], run: &[&str]| rule.windows(run.len()).any(|w| w == run);
        for subnet in BRIDGED_EGRESS_SUBNETS {
            for source in [Some("10.20.30.40"), None] {
                let rule = global_nat_rule(subnet, "eth0", source);
                assert!(has(&rule, &["-s", subnet]), "{rule:?}");
                assert!(
                    has(&rule, &["-m", "addrtype", "!", "--src-type", "LOCAL"]),
                    "the rule's source match covers the host's own addresses: {rule:?}"
                );
                match source {
                    Some(source) => assert!(
                        has(&rule, &["-j", "SNAT", "--to-source", source]),
                        "{rule:?}"
                    ),
                    None => assert!(has(&rule, &["-j", "MASQUERADE"]), "{rule:?}"),
                }
            }
        }
    }

    #[test]
    fn the_default_routes_source_is_read_from_ip_route() {
        assert_eq!(
            parse_default_route_source(
                "default via 10.20.30.1 dev eth0 proto dhcp src 10.20.30.40 metric 100\n"
            )
            .as_deref(),
            Some("10.20.30.40")
        );
        assert_eq!(
            parse_default_route_source("default via 10.20.30.1 dev eth0\n"),
            None
        );
        assert_eq!(parse_default_route_source(""), None);
    }

    /// A rule added under an earlier route source is replaced, not shadowed: it came
    /// first in the chain and kept translating to the old address.
    #[test]
    fn a_rule_for_an_old_route_source_is_replaced() {
        let wanted = global_nat_rule("10.0.0.0/8", "eth0", Some("10.20.30.40"));
        let old = global_nat_rule("10.0.0.0/8", "eth0", Some("10.20.30.7"));
        let other = global_nat_rule("172.30.0.0/16", "eth0", Some("10.20.30.7"));
        // A rule someone else added for part of the subnet: it keeps its precedence.
        let admins: Vec<String> = "-s 10.42.0.0/16 -o eth0 -j SNAT --to-source 192.0.2.9"
            .split(' ')
            .map(str::to_string)
            .collect();
        let listing = |rules: &[&Vec<String>]| {
            rules
                .iter()
                .map(|rule| format!("-A POSTROUTING {}\n", rule.join(" ")))
                .collect::<String>()
        };
        assert_eq!(
            global_nat_replacement(&listing(&[&admins, &old, &other]), "10.0.0.0/8", &wanted),
            (Insert::At(2), vec![old.clone()])
        );
        assert_eq!(
            global_nat_replacement(&listing(&[&old, &wanted]), "10.0.0.0/8", &wanted),
            (Insert::Present, vec![old.clone()])
        );
        assert_eq!(
            global_nat_replacement(&listing(&[&admins, &other]), "10.0.0.0/8", &wanted),
            (Insert::Append, Vec::<Vec<String>>::new())
        );
    }

    /// A position from the listing can be past the end once another firewall writer
    /// has deleted rules: the refused insert lists the chain again and places the rule
    /// there, and a stale rule someone else already removed is not a failure.
    #[tokio::test]
    async fn a_refused_insert_lists_the_chain_again() {
        let wanted = global_nat_rule("10.0.0.0/8", "eth0", Some("10.20.30.40"));
        let old = global_nat_rule("10.0.0.0/8", "eth0", Some("10.20.30.7"));
        let line = |rule: &[String]| format!("-A POSTROUTING {}\n", rule.join(" "));
        let first = format!(
            "-A POSTROUTING -s 192.0.2.0/24 -j RETURN\n-A POSTROUTING -s 192.0.2.128/25 -j RETURN\n{}",
            line(&old)
        );
        let second = line(&old);
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let mut listings = vec![second, first];
        let seen = calls.clone();
        let mut run = move |args: Vec<String>| {
            seen.lock().unwrap().push(args.clone());
            let reply = match args[2].as_str() {
                "-S" => IptablesRun {
                    ok: true,
                    stdout: listings.pop().unwrap_or_default(),
                    stderr: String::new(),
                },
                // The first insert's position (3) is past the end of the shrunk chain.
                "-I" if args[4] == "3" => IptablesRun {
                    ok: false,
                    stdout: String::new(),
                    stderr: "iptables: Index of insertion too big.".into(),
                },
                "-D" => IptablesRun {
                    ok: false,
                    stdout: String::new(),
                    stderr: "iptables: Bad rule (does a matching rule exist in that chain?)."
                        .into(),
                },
                _ => IptablesRun {
                    ok: true,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            };
            Box::pin(async move { Ok(reply) })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Result<IptablesRun>> + Send>>
        };
        place_global_nat_rule("10.0.0.0/8", &wanted, &mut run)
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        let verbs: Vec<(&str, &str)> = calls
            .iter()
            .map(|args| {
                (
                    args[2].as_str(),
                    args.get(4).map(String::as_str).unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            verbs[..4],
            [("-S", ""), ("-I", "3"), ("-S", ""), ("-I", "1")],
            "{calls:?}"
        );
        assert_eq!(calls[4][2], "-D");
    }

    /// The interface and the route source are read together under the NAT lock, so a
    /// launch never replaces another's rule using an interface it read before the
    /// default route moved.
    #[test]
    fn the_nat_rule_is_built_from_one_default_route_read_under_the_lock() {
        let source = include_str!("portmap.rs");
        let body = &source[source.find("pub async fn ensure_global_nat(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(
            body.starts_with("pub async fn ensure_global_nat(vm_subnet: &str) -> Result<()>"),
            "ensure_global_nat takes an interface read before its lock"
        );
        let lock = body.find("acquire_host_network_lock(").unwrap();
        let route = body.find("\"route\", \"show\", \"default\"").unwrap();
        let iface = body.find("parse_default_interface(&route)").unwrap();
        assert!(
            lock < route && route < iface,
            "the default route is read outside the lock"
        );
    }

    /// Cleanup removes fcvm's rules for the guests' subnets in any form, and nothing
    /// else.
    #[test]
    fn cleanup_selects_only_fcvms_rules_for_the_guests_subnets() {
        let listing = "-P POSTROUTING ACCEPT\n\
            -A POSTROUTING -s 10.0.0.0/8 -o eth0 -m comment --comment fcvm-bridged -j MASQUERADE\n\
            -A POSTROUTING -s 172.30.0.0/16 -o eth0 -m addrtype ! --src-type LOCAL -m comment --comment fcvm-bridged -j SNAT --to-source 10.20.30.40\n\
            -A POSTROUTING -d 172.30.1.2/32 -p tcp -m tcp --dport 80 -m comment --comment fcvm-bridged -j MASQUERADE\n\
            -A POSTROUTING -s 10.0.0.0/8 -o eth0 -j MASQUERADE\n";
        let rules = global_nat_rules_in(listing);
        assert_eq!(rules.len(), 2, "{rules:?}");
        assert_eq!(rules[0][..2], ["-s".to_string(), "10.0.0.0/8".to_string()]);
        assert_eq!(
            rules[1][..2],
            ["-s".to_string(), "172.30.0.0/16".to_string()]
        );
    }

    #[test]
    fn an_ipv6_only_host_is_diagnosed_not_reported_as_a_parse_failure() {
        // Measured on a devserver whose only default route is IPv6: `ip route
        // show default` prints nothing, and the old message ended in a colon
        // with nothing after it.
        let error = parse_default_interface("").expect_err("no default route");
        let text = format!("{error:#}");
        assert!(text.contains("no IPv4 default route"), "{text}");
        assert!(
            text.contains("rootless"),
            "must name the way forward: {text}"
        );
    }

    #[test]
    fn an_ordinary_default_route_yields_its_interface() {
        assert_eq!(
            parse_default_interface("default via 10.0.0.1 dev eth0 proto dhcp\n").unwrap(),
            "eth0"
        );
    }

    #[test]
    fn output_without_a_device_still_reports_what_it_saw() {
        let error = parse_default_interface("default via 10.0.0.1\n").expect_err("no dev");
        assert!(format!("{error:#}").contains("10.0.0.1"));
    }

    fn mapping(host_ip: Option<&str>) -> PortMapping {
        PortMapping {
            host_ip: host_ip.map(str::to_string),
            host_port: 8080,
            guest_port: 80,
            proto: Protocol::Tcp,
            loopback_client: false,
        }
    }

    #[test]
    fn an_ipv6_host_address_is_refused_by_name() {
        let ipv4 = [
            mapping(None),
            mapping(Some("127.0.0.1")),
            mapping(Some("0.0.0.0")),
        ];
        require_ipv4_host_addresses(&ipv4).unwrap();

        for host_ip in ["::", "::1", "2001:db8::1"] {
            let error = require_ipv4_host_addresses(&[mapping(None), mapping(Some(host_ip))])
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("[{host_ip}]:8080")), "{error}");
        }
    }

    #[test]
    fn test_delete_rule_conversion() {
        let forward_rule = "-A FORWARD -p tcp -d 172.30.1.2 --dport 80 -j ACCEPT";
        let dnat_rule =
            "-t nat -A PREROUTING -p tcp --dport 8080 -j DNAT --to-destination 172.30.1.2:80";

        let forward_delete = to_delete_rule(forward_rule);
        let dnat_delete = to_delete_rule(dnat_rule);

        assert_eq!(
            forward_delete,
            "-D FORWARD -p tcp -d 172.30.1.2 --dport 80 -j ACCEPT"
        );
        assert_eq!(
            dnat_delete,
            "-t nat -D PREROUTING -p tcp --dport 8080 -j DNAT --to-destination 172.30.1.2:80"
        );
    }

    #[tokio::test]
    async fn test_detect_default_interface() {
        // This test just verifies the function doesn't panic
        // Actual interface name depends on the system
        let result = detect_default_interface().await;
        // On most systems this should succeed
        if let Ok(iface) = result {
            assert!(!iface.is_empty());
            println!("Detected interface: {}", iface);
        }
    }

    #[cfg(feature = "privileged-tests")]
    #[tokio::test]
    async fn test_port_mapping_lifecycle() {
        // Test that we can create and cleanup rules (requires root for iptables)
        // Use a scoped host_ip so rules don't conflict with parallel tests
        let veth_ip = "172.30.99.1"; // Fake veth IP for testing
        let guest_ip = "172.30.99.2";
        let mappings = vec![PortMapping {
            host_ip: Some(veth_ip.to_string()), // Scope DNAT to this IP
            host_port: 8080,
            guest_port: 80,
            proto: Protocol::Tcp,
            loopback_client: false,
        }];

        // Setup
        let rules = setup_port_mappings(guest_ip, &mappings)
            .await
            .expect("setup port mappings (requires root)");

        assert_eq!(rules.len(), 4); // DNAT (PREROUTING) + DNAT (OUTPUT) + MASQUERADE + FORWARD

        // Cleanup
        cleanup_port_mappings(&rules).await.unwrap();
    }
}
