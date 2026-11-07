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
//! on `lo`. A restore removes a retired address no listener uses; the reconcile after
//! the hook and the watch after publication change only the translations.
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

/// The retired-address state of this guest, kept in fc-agent's memory and so carried by
/// a snapshot with the netfilter rules it describes.
#[derive(Debug, Default)]
struct RestoreState {
    /// The restore whose plan the rules follow, counted from 1. Every change to the
    /// rules and to the addresses on `lo` is made holding this state, and a change made
    /// for an earlier restore is refused (`reconcile`). A snapshot of a clone holds that
    /// clone's watch (`watch`), which resumes in the next clone with the previous clone's
    /// address: once the next restore has counted itself, nothing the inherited watch
    /// does reaches the rules.
    generation: u64,
    /// Whether the chain holds translations. A restore of a clone whose snapshot had
    /// them, and that has none of its own, empties the chain rather than leaving rules
    /// that send its own address's ports to addresses it has just removed.
    rules: bool,
}

static RESTORE: std::sync::Mutex<RestoreState> = std::sync::Mutex::new(RestoreState {
    generation: 0,
    rules: false,
});

fn lock_restore() -> std::sync::MutexGuard<'static, RestoreState> {
    RESTORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What one restore carried: the translations it set up, for the reconcile after its
/// hook and its watch.
#[derive(Debug, Clone)]
pub struct Carried {
    pub generation: u64,
    pub current: Ipv6Addr,
    pub plan: Plan,
    /// The retired addresses left on `lo`. A listener on one can close and bind again
    /// (a service restart), so the watch keeps looking while any remain.
    pub retained: Vec<Ipv6Addr>,
}

impl Carried {
    /// Whether the watch has anything left to follow.
    pub fn keeps_watching(&self) -> bool {
        !self.plan.translate.is_empty() || !self.retained.is_empty()
    }
}

/// The addresses on `lo` once `plan` is applied over `on_lo`.
fn retained_after(plan: &Plan, on_lo: &[Ipv6Addr]) -> Vec<Ipv6Addr> {
    let mut retained: Vec<Ipv6Addr> = on_lo
        .iter()
        .chain(&plan.keep)
        .copied()
        .filter(|address| !plan.remove.contains(address))
        .collect();
    retained.sort_unstable();
    retained.dedup();
    retained
}

/// The listener sockets and the retired addresses on `lo`, as the kernel lists them now.
fn read_tables() -> Result<(Vec<Ipv6Addr>, Vec<Listener>)> {
    let read =
        |path: &str| std::fs::read_to_string(path).with_context(|| format!("reading {path}"));
    let on_lo = parse_retired_on_lo(&read("/proc/net/if_inet6")?);
    let mut listeners = parse_sockets(&read("/proc/net/tcp6")?, Proto::Tcp);
    listeners.extend(parse_sockets(&read("/proc/net/udp6")?, Proto::Udp));
    Ok((on_lo, listeners))
}

/// The signature of [`apply`]: plan, the clone's address, the retired addresses on
/// `lo`, and whether the chain holds translations now.
type ApplyFn<'a> = dyn FnMut(&Plan, Ipv6Addr, &[Ipv6Addr], bool) -> Result<()> + 'a;

/// [`carry_listeners`] over tables already read, with `apply` in place of [`apply`].
fn carry_in(
    state: &mut RestoreState,
    retired_now: Option<Ipv6Addr>,
    current: Ipv6Addr,
    on_lo: &[Ipv6Addr],
    listeners: &[Listener],
    apply: &mut ApplyFn<'_>,
) -> Result<Carried> {
    state.generation += 1;
    let plan = plan(retired_now, on_lo, listeners, current);
    apply(&plan, current, on_lo, state.rules)?;
    state.rules = !plan.translate.is_empty();
    Ok(Carried {
        generation: state.generation,
        current,
        retained: retained_after(&plan, on_lo),
        plan,
    })
}

/// After a routed restore moved the guest to `current` (and removed `retired_now`
/// from eth0), keep every listener on a retired address reachable at `current`.
/// Counts this restore, so an inherited watch can no longer change the rules.
pub fn carry_listeners(retired_now: Option<Ipv6Addr>, current: Ipv6Addr) -> Result<Carried> {
    let mut state = lock_restore();
    let (on_lo, listeners) = read_tables()?;
    carry_in(
        &mut state,
        retired_now,
        current,
        &on_lo,
        &listeners,
        &mut apply,
    )
}

/// The plan a reconcile applies: the translations the listeners call for now, and no
/// address removed. A service that closed its listener on a retired address can still
/// be draining connections it accepted there, and deleting the address would take the
/// route those connections use; the next restore, whose socket cleanup retires every
/// snapshot-time connection, removes it.
fn replan(on_lo: &[Ipv6Addr], listeners: &[Listener], current: Ipv6Addr) -> Plan {
    Plan {
        remove: Vec::new(),
        ..plan(None, on_lo, listeners, current)
    }
}

/// Plan again from the listeners as they are now and apply what changed since
/// `carried.plan`: a retired listener that closed, or a service now listening on the
/// clone's own address, loses its translation, and one that listens again on a retained
/// address gets it back. Returns false, changing nothing, when a later restore than
/// `carried`'s has counted itself. Reads three `/proc` files and starts no process when
/// nothing changed.
pub fn reconcile(carried: &mut Carried) -> Result<bool> {
    let mut state = lock_restore();
    if state.generation != carried.generation {
        return Ok(false);
    }
    let (on_lo, listeners) = read_tables()?;
    let next = replan(&on_lo, &listeners, carried.current);
    if next.translate != carried.plan.translate {
        apply(&next, carried.current, &on_lo, state.rules)?;
        state.rules = !next.translate.is_empty();
    }
    carried.retained = retained_after(&next, &on_lo);
    carried.plan = next;
    Ok(true)
}

/// How often a clone with translations looks again at its listeners.
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// Keep `carried`'s translations in step with the listeners after the restore
/// published: every second while a retired address remains on `lo`, reconcile. A
/// service that closes its listener on the retired address and binds the clone's own
/// loses the translation within a second, and one that binds the retired address again
/// gets it back. Ends when no retired address remains or a later restore has counted
/// itself.
pub fn watch(carried: Carried) {
    if !carried.keeps_watching() {
        return;
    }
    tokio::spawn(async move {
        let mut carried = carried;
        while carried.keeps_watching() {
            tokio::time::sleep(WATCH_EVERY).await;
            let previous = carried.clone();
            carried = match tokio::task::spawn_blocking(move || {
                let mut next = previous;
                reconcile(&mut next).map(|current| (current, next))
            })
            .await
            {
                Ok(Ok((true, next))) => next,
                Ok(Ok((false, _))) => return,
                Ok(Err(error)) => {
                    eprintln!(
                        "[fc-agent] WARNING: keeping retired IPv6 listeners reachable: {error:#}"
                    );
                    carried
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

    /// A reconcile removes no address: a service that closed its listener on a retired
    /// address can still be draining the connections it accepted there.
    #[test]
    fn a_reconcile_keeps_a_retired_address_its_listener_left() {
        let plan = replan(&[addr(OLD)], &[], addr(NEW));
        assert!(plan.remove.is_empty(), "{plan:?}");
        assert!(plan.translate.is_empty(), "{plan:?}");
    }

    /// The watch a snapshot of a clone carried into the next clone names the previous
    /// clone's address. Once the next restore has counted itself, its reconcile changes
    /// nothing and the watch ends.
    #[test]
    fn a_watch_from_an_earlier_restore_changes_nothing() {
        let generation = {
            let mut state = lock_restore();
            state.generation += 1;
            state.generation
        };
        let mut inherited = Carried {
            generation,
            current: addr(OLD),
            // No translation, so a reconcile that went ahead would find nothing to
            // apply on the machine running the test.
            plan: Plan::default(),
            retained: Vec::new(),
        };
        lock_restore().generation += 1;
        let before = inherited.plan.clone();
        assert!(
            !reconcile(&mut inherited).unwrap(),
            "a stale watch reconciled"
        );
        assert_eq!(inherited.plan, before);
    }

    /// A restore with nothing to translate empties a chain its snapshot carried
    /// translations in, rather than leaving rules that send its own address's ports to
    /// addresses it has just removed.
    #[test]
    fn a_restore_with_nothing_to_translate_empties_inherited_rules() {
        let mut state = RestoreState {
            generation: 1,
            rules: true,
        };
        let mut had_rules = None;
        let mut record = |plan: &Plan, _: Ipv6Addr, _: &[Ipv6Addr], rules: bool| {
            assert!(plan.translate.is_empty());
            had_rules = Some(rules);
            Ok(())
        };
        let carried = carry_in(
            &mut state,
            Some(addr(OLD)),
            addr(NEW),
            &[addr(OLDER)],
            &[],
            &mut record,
        )
        .unwrap();
        assert_eq!(
            had_rules,
            Some(true),
            "the apply was not told the chain holds rules"
        );
        assert!(!state.rules);
        assert!(carried.retained.is_empty(), "{carried:?}");
    }

    /// The watch keeps looking while a retired address remains on `lo`, so a service
    /// that closes its listener there and binds it again (a restart) gets its
    /// translation back.
    #[test]
    fn the_watch_follows_a_listener_through_a_restart() {
        let carried = Carried {
            generation: 1,
            current: addr(NEW),
            plan: Plan::default(),
            retained: vec![addr(OLD)],
        };
        assert!(carried.keeps_watching());
        assert!(!Carried {
            retained: Vec::new(),
            ..carried
        }
        .keeps_watching());
    }
}
