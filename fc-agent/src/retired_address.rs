//! A routed clone's listeners on the IPv6 address it was restored from (#1083).
//!
//! A routed restore moves the guest from the snapshot's global IPv6 address to the
//! clone's own (`network::reconfigure_ipv6`). A socket that listens on the snapshot's
//! address specifically, rather than on `::`, stays bound to an address the guest no
//! longer has, and the restore's socket cleanup does not touch listeners. Every clone
//! of a snapshot taken while such a service ran would leave it unreachable.
//!
//! So a retired address that a listener still uses is kept, on `lo` and deprecated
//! (preferred lifetime 0, so the kernel never picks it as the source of a new
//! connection), and the clone's own address is translated to it for that listener's
//! port and protocol: a DNAT in the `nat` table's PREROUTING (clients on the network)
//! and OUTPUT (clients in the guest) chains, through one chain this module owns and
//! rebuilds on every restore. A clone of a clone carries listeners on any earlier
//! generation's address the same way: those addresses are the deprecated global ones
//! on `lo`. A retired address no listener uses is removed.
//!
//! The common case, no listener on a retired address, reads three `/proc` files and
//! spawns nothing.

use std::net::Ipv6Addr;

use anyhow::{bail, Context, Result};

/// The `nat` chain the translations live in, jumped to from PREROUTING and OUTPUT.
pub const CHAIN: &str = "FCVM_RETIRED_ADDRS";

/// `IFA_F_DEPRECATED` from linux/if_addr.h.
const IFA_F_DEPRECATED: u32 = 0x20;
/// `/proc/net/if_inet6` scope of a global address.
const SCOPE_GLOBAL: u32 = 0;
/// `TCP_LISTEN` and `TCP_CLOSE` as `/proc/net/{tcp6,udp6}` print them.
const TCP_LISTEN: u32 = 0x0a;
const UDP_UNCONNECTED: u32 = 0x07;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    fn name(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// A socket that accepts traffic on one local address and port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Listener {
    pub proto: Proto,
    pub address: Ipv6Addr,
    pub port: u16,
}

/// What a restore does with the retired addresses.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Retired addresses a listener uses, to be held on `lo`, deprecated.
    pub keep: Vec<Ipv6Addr>,
    /// Retired addresses on `lo` no listener uses any more.
    pub remove: Vec<Ipv6Addr>,
    /// (proto, port, retired address): the clone's address and this port reach that
    /// address. One per (proto, port); the first listener in address order wins.
    pub translate: Vec<(Proto, u16, Ipv6Addr)>,
}

/// Parse the address field of `/proc/net/{if_inet6,tcp6,udp6}`: 32 hex digits. In
/// `if_inet6` they are the address in network order; in the socket tables each group
/// of eight is one 32-bit word printed in host byte order.
fn parse_address(hex: &str, words_in_host_order: bool) -> Option<Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    let mut octets = [0u8; 16];
    for (i, octet) in octets.iter_mut().enumerate() {
        *octet = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    if words_in_host_order {
        for word in octets.chunks_mut(4) {
            // The kernel printed the word's native value, so its bytes in memory are
            // that value's native encoding.
            let value = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            word.copy_from_slice(&value.to_ne_bytes());
        }
    }
    Some(Ipv6Addr::from(octets))
}

/// The listeners in a `/proc/net/tcp6` or `/proc/net/udp6` table: TCP sockets in
/// LISTEN, UDP sockets with no peer.
pub fn parse_sockets(table: &str, proto: Proto) -> Vec<Listener> {
    let wanted = match proto {
        Proto::Tcp => TCP_LISTEN,
        Proto::Udp => UDP_UNCONNECTED,
    };
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace().skip(1);
            let local = fields.next()?;
            let remote = fields.next()?;
            let state = u32::from_str_radix(fields.next()?, 16).ok()?;
            if state != wanted {
                return None;
            }
            if proto == Proto::Udp && !remote.starts_with("00000000000000000000000000000000:") {
                return None;
            }
            let (address, port) = local.split_once(':')?;
            Some(Listener {
                proto,
                address: parse_address(address, true)?,
                port: u16::from_str_radix(port, 16).ok()?,
            })
        })
        .collect()
}

/// The deprecated global addresses on `lo` in a `/proc/net/if_inet6` table: the
/// addresses earlier restores retired.
pub fn parse_retired_on_lo(table: &str) -> Vec<Ipv6Addr> {
    table
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [address, _index, _prefix, scope, flags, device] = fields[..] else {
                return None;
            };
            let scope = u32::from_str_radix(scope, 16).ok()?;
            let flags = u32::from_str_radix(flags, 16).ok()?;
            (device == "lo" && scope == SCOPE_GLOBAL && flags & IFA_F_DEPRECATED != 0)
                .then(|| parse_address(address, false))
                .flatten()
        })
        .collect()
}

/// Decide what to keep, remove and translate. `retired_now` is the address this
/// restore removed from eth0, `on_lo` the ones earlier restores retired, and `current`
/// the clone's own address.
pub fn plan(
    retired_now: Option<Ipv6Addr>,
    on_lo: &[Ipv6Addr],
    listeners: &[Listener],
    current: Ipv6Addr,
) -> Plan {
    let mut retired: Vec<Ipv6Addr> = on_lo.iter().copied().chain(retired_now).collect();
    retired.retain(|address| *address != current);
    retired.sort_unstable();
    retired.dedup();

    // A port something listens on at the clone's own address, or on `::`, is that
    // listener's: a service the restore hook or a later restart bound again there is
    // reached directly, and its old listener on the retired address is not.
    let served_here = |l: &Listener| {
        listeners.iter().any(|other| {
            other.proto == l.proto
                && other.port == l.port
                && (other.address == current || other.address.is_unspecified())
        })
    };
    let mut using: Vec<Listener> = listeners
        .iter()
        .copied()
        .filter(|l| retired.contains(&l.address) && !served_here(l))
        .collect();
    using.sort_unstable();

    let mut plan = Plan::default();
    for address in &retired {
        if using.iter().any(|l| l.address == *address) {
            // An address already on lo stays; one this restore removed is added there.
            plan.keep.push(*address);
        } else if on_lo.contains(address) {
            plan.remove.push(*address);
        }
    }
    for listener in using {
        if !plan
            .translate
            .iter()
            .any(|(proto, port, _)| *proto == listener.proto && *port == listener.port)
        {
            plan.translate
                .push((listener.proto, listener.port, listener.address));
        }
    }
    plan
}

/// The `ip6tables-restore --noflush` input that replaces the chain's rules.
pub fn chain_rules(plan: &Plan, current: Ipv6Addr) -> String {
    let mut rules = format!("*nat\n:{CHAIN} - [0:0]\n-F {CHAIN}\n");
    for (proto, port, address) in &plan.translate {
        let proto = proto.name();
        rules.push_str(&format!(
            "-A {CHAIN} -d {current}/128 -p {proto} -m {proto} --dport {port} \
             -j DNAT --to-destination {address}\n"
        ));
    }
    rules.push_str("COMMIT\n");
    rules
}

fn run(program: &str, args: &[&str], input: Option<&str>) -> Result<std::process::Output> {
    use std::io::Write;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if input.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("running {program}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin
            .write_all(input.as_bytes())
            .with_context(|| format!("writing {program}'s input"))?;
    }
    child
        .wait_with_output()
        .with_context(|| format!("waiting for {program}"))
}

fn run_checked(program: &str, args: &[&str], input: Option<&str>) -> Result<()> {
    let output = run(program, args, input)?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Apply `plan`: hold the kept addresses on `lo`, translate the clone's address to
/// them, remove what no listener uses.
fn apply(plan: &Plan, current: Ipv6Addr, on_lo: &[Ipv6Addr], had_rules: bool) -> Result<()> {
    for address in &plan.keep {
        if !on_lo.contains(address) {
            let address = format!("{address}/128");
            run_checked(
                "ip",
                &[
                    "-6",
                    "addr",
                    "add",
                    &address,
                    "dev",
                    "lo",
                    "nodad",
                    "preferred_lft",
                    "0",
                ],
                None,
            )?;
        }
    }
    if plan.translate.is_empty() && had_rules {
        // The chain stays hooked and empty.
        run_checked(
            "ip6tables-restore",
            &["-w", "--noflush"],
            Some(&chain_rules(plan, current)),
        )?;
    }
    if !plan.translate.is_empty() {
        run_checked(
            "ip6tables-restore",
            &["-w", "--noflush"],
            Some(&chain_rules(plan, current)),
        )?;
        for hook in ["PREROUTING", "OUTPUT"] {
            let present = run(
                "ip6tables",
                &["-w", "-t", "nat", "-C", hook, "-j", CHAIN],
                None,
            )?
            .status
            .success();
            if !present {
                run_checked(
                    "ip6tables",
                    &["-w", "-t", "nat", "-I", hook, "1", "-j", CHAIN],
                    None,
                )?;
            }
        }
    }
    for address in &plan.remove {
        let address = format!("{address}/128");
        run_checked("ip", &["-6", "addr", "del", &address, "dev", "lo"], None)?;
    }
    Ok(())
}

/// After a routed restore moved the guest to `current` (and removed `retired_now`
/// from eth0), keep every listener on a retired address reachable at `current`.
pub fn carry_listeners(retired_now: Option<Ipv6Addr>, current: Ipv6Addr) -> Result<Plan> {
    let read =
        |path: &str| std::fs::read_to_string(path).with_context(|| format!("reading {path}"));
    let on_lo = parse_retired_on_lo(&read("/proc/net/if_inet6")?);
    let mut listeners = parse_sockets(&read("/proc/net/tcp6")?, Proto::Tcp);
    listeners.extend(parse_sockets(&read("/proc/net/udp6")?, Proto::Udp));
    let plan = plan(retired_now, &on_lo, &listeners, current);
    apply(&plan, current, &on_lo, false)?;
    Ok(plan)
}

/// Plan again from the listeners as they are now and apply what changed, after
/// `previous`: a retired listener that closed, or a service now listening on the
/// clone's own address, loses its translation, and an address no listener uses any
/// more leaves `lo`. Reads three `/proc` files and starts no process when nothing
/// changed.
pub fn reconcile(current: Ipv6Addr, previous: &Plan) -> Result<Plan> {
    let read =
        |path: &str| std::fs::read_to_string(path).with_context(|| format!("reading {path}"));
    let on_lo = parse_retired_on_lo(&read("/proc/net/if_inet6")?);
    let mut listeners = parse_sockets(&read("/proc/net/tcp6")?, Proto::Tcp);
    listeners.extend(parse_sockets(&read("/proc/net/udp6")?, Proto::Udp));
    let next = plan(None, &on_lo, &listeners, current);
    if next.translate != previous.translate || !next.remove.is_empty() {
        apply(&next, current, &on_lo, !previous.translate.is_empty())?;
    }
    Ok(next)
}

/// How often a clone with translations looks again at its listeners.
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// The restore whose translations the running watch keeps; a later restore ends it.
static WATCH_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Keep `plan`'s translations in step with the listeners after the restore published:
/// every second while any remain, reconcile. A service that closes its listener on the
/// retired address and binds the clone's own loses the translation within a second.
/// Ends when no translation remains or a later restore starts its own.
pub fn watch(current: Ipv6Addr, plan: Plan) {
    use std::sync::atomic::Ordering;
    let generation = WATCH_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    if plan.translate.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut plan = plan;
        while !plan.translate.is_empty() {
            tokio::time::sleep(WATCH_EVERY).await;
            if WATCH_GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }
            let previous = std::mem::take(&mut plan);
            plan = match tokio::task::spawn_blocking(move || {
                reconcile(current, &previous).map_err(|e| (e, previous))
            })
            .await
            {
                Ok(Ok(next)) => next,
                Ok(Err((error, previous))) => {
                    eprintln!(
                        "[fc-agent] WARNING: keeping retired IPv6 listeners reachable: {error:#}"
                    );
                    previous
                }
                Err(_) => return,
            };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "2001:db8:0:1::a";
    const OLDER: &str = "2001:db8:0:1::b";
    const NEW: &str = "2001:db8:0:1::c";

    fn addr(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    /// The text the kernel prints for `address` in a socket table on this machine.
    fn socket_hex(address: Ipv6Addr) -> String {
        address
            .octets()
            .chunks(4)
            .map(|w| format!("{:08X}", u32::from_ne_bytes([w[0], w[1], w[2], w[3]])))
            .collect()
    }

    #[test]
    fn socket_tables_name_listeners_by_address_and_port() {
        let tcp = format!(
            "  sl  local_address                         remote_address                        st\n\
             0: {}:1F91 00000000000000000000000000000000:0000 0A 0\n\
             1: {}:0050 00000000000000000000000000000000:0000 0A 0\n\
             2: {}:C350 {}:0050 01 0\n",
            socket_hex(addr(OLD)),
            socket_hex(Ipv6Addr::UNSPECIFIED),
            socket_hex(addr(NEW)),
            socket_hex(addr(OLDER)),
        );
        assert_eq!(
            parse_sockets(&tcp, Proto::Tcp),
            vec![
                Listener {
                    proto: Proto::Tcp,
                    address: addr(OLD),
                    port: 8081
                },
                Listener {
                    proto: Proto::Tcp,
                    address: Ipv6Addr::UNSPECIFIED,
                    port: 80
                },
            ]
        );
        let udp = format!(
            "  sl  local_address remote_address st\n\
             0: {}:1388 00000000000000000000000000000000:0000 07 0\n\
             1: {}:1389 {}:0035 07 0\n",
            socket_hex(addr(OLD)),
            socket_hex(addr(OLD)),
            socket_hex(addr(OLDER)),
        );
        assert_eq!(
            parse_sockets(&udp, Proto::Udp),
            vec![Listener {
                proto: Proto::Udp,
                address: addr(OLD),
                port: 5000
            }]
        );
    }

    #[test]
    fn retired_addresses_are_the_deprecated_global_ones_on_lo() {
        // address, index, prefix length, scope, flags, device. 0xa0 is deprecated and
        // permanent, 0x80 permanent only, scope 0x20 link-local.
        let table = "\
20010db800000001000000000000000a 01 80 00 a0 lo
20010db800000001000000000000000b 01 80 00 80 lo
20010db800000001000000000000000c 02 80 00 a0 eth0
fe800000000000000000000000000001 01 40 20 a0 lo
";
        assert_eq!(parse_retired_on_lo(table), vec![addr(OLD)]);
    }

    fn listener(proto: Proto, address: &str, port: u16) -> Listener {
        Listener {
            proto,
            address: addr(address),
            port,
        }
    }

    #[test]
    fn a_listener_on_the_retired_address_is_translated_from_the_new_one() {
        let listeners = [
            listener(Proto::Tcp, OLD, 8081),
            listener(Proto::Tcp, "::", 80),
            listener(Proto::Udp, OLD, 5000),
            listener(Proto::Tcp, NEW, 9000),
        ];
        let plan = plan(Some(addr(OLD)), &[], &listeners, addr(NEW));
        assert_eq!(plan.keep, vec![addr(OLD)]);
        assert!(plan.remove.is_empty());
        assert_eq!(
            plan.translate,
            vec![(Proto::Tcp, 8081, addr(OLD)), (Proto::Udp, 5000, addr(OLD))]
        );
        let rules = chain_rules(&plan, addr(NEW));
        assert!(rules.contains(&format!(
            "-A {CHAIN} -d {NEW}/128 -p tcp -m tcp --dport 8081 -j DNAT --to-destination {OLD}\n"
        )));
        assert!(rules.contains(&format!(
            "-A {CHAIN} -d {NEW}/128 -p udp -m udp --dport 5000 -j DNAT --to-destination {OLD}\n"
        )));
        assert!(rules.starts_with(&format!("*nat\n:{CHAIN} - [0:0]\n-F {CHAIN}\n")));
        assert!(rules.ends_with("COMMIT\n"));
    }

    #[test]
    fn without_a_listener_on_a_retired_address_nothing_is_kept() {
        let listeners = [listener(Proto::Tcp, "::", 80)];
        assert_eq!(
            plan(Some(addr(OLD)), &[], &listeners, addr(NEW)),
            Plan::default()
        );
        assert_eq!(plan(None, &[], &listeners, addr(NEW)), Plan::default());
    }

    /// A port a listener serves at the clone's own address or on `::` is not
    /// translated: a service the restore hook or a restart bound there again is reached
    /// directly, not sent to its old listener on the retired address.
    #[test]
    fn a_port_served_at_the_new_address_is_not_translated() {
        for here in [NEW, "::"] {
            let listeners = [
                listener(Proto::Tcp, OLD, 8081),
                listener(Proto::Tcp, here, 8081),
            ];
            let plan = plan(Some(addr(OLD)), &[], &listeners, addr(NEW));
            assert!(plan.translate.is_empty(), "{here}: {plan:?}");
            assert!(plan.keep.is_empty(), "{here}: {plan:?}");
        }
        // Another port of the same service is still carried.
        let listeners = [
            listener(Proto::Tcp, OLD, 8081),
            listener(Proto::Tcp, NEW, 9000),
        ];
        let plan = plan(Some(addr(OLD)), &[], &listeners, addr(NEW));
        assert_eq!(plan.translate, vec![(Proto::Tcp, 8081, addr(OLD))]);
    }

    /// A clone of a clone: the first snapshot's address is already on lo, and the
    /// second's was just removed from eth0.
    #[test]
    fn earlier_generations_are_carried_and_unused_ones_removed() {
        let listeners = [listener(Proto::Tcp, OLDER, 8081)];
        let plan = plan(Some(addr(OLD)), &[addr(OLDER)], &listeners, addr(NEW));
        assert_eq!(plan.keep, vec![addr(OLDER)]);
        assert!(plan.remove.is_empty());
        assert_eq!(plan.translate, vec![(Proto::Tcp, 8081, addr(OLDER))]);

        let plan = super::plan(Some(addr(OLD)), &[addr(OLDER)], &[], addr(NEW));
        assert_eq!(plan.remove, vec![addr(OLDER)]);
        assert!(plan.keep.is_empty() && plan.translate.is_empty());
    }
}
