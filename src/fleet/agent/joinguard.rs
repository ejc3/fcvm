//! The host's limits on public Minecraft joins (docs/fleet/minecraft.md, "Public joins"): the
//! fleet security group admits TCP 25565 over IPv6 to the Minecraft VMs, which this host routes to
//! them, so the host's own filter is where a source's connections are counted. Per source /64 (an
//! IPv6 subscriber's usual prefix), new connections past [`MAX_CONNECTIONS`] open ones, or past
//! [`NEW_PER_MINUTE`] a minute after a burst of [`NEW_BURST`], are reset before they reach a VM.
//!
//! One chain, `FCVM-MC-JOIN`, flushed and filled again at every agent start, and one jump to it
//! from the hook chain (`FORWARD` on a host). The jump is kept the hook's first rule, so no earlier
//! rule can accept or return a join before the limits see it: podman's netavark inserts its own
//! FORWARD jump at position 1 when the Windows bridge containers start, after the agent, so
//! [`keep_first`] moves the agent's jump back, every [`KEEP_EVERY`] and whenever [`poke`]d. The
//! fleet's Java guard (ejc3/aws `minecraft-java-guard.sh`) opens 25565 only while it reads the
//! jump first. The player cap and the login timeout are Pumpkin's own
//! (fleet/minecraft/src/pumpkin.rs).

use anyhow::{bail, Context, Result};
use std::time::Duration;

/// The chain the rules live in.
pub const CHAIN: &str = "FCVM-MC-JOIN";
/// The Java Edition port the security group opens.
pub const JOIN_PORT: u16 = 25565;
/// Open connections one source /64 may hold to the servers.
pub const MAX_CONNECTIONS: u32 = 4;
/// New connections a minute one source /64 may open after its burst.
pub const NEW_PER_MINUTE: u32 = 12;
/// New connections one source /64 may open at once before the rate applies.
pub const NEW_BURST: u32 = 6;
/// The prefix length a source is counted by.
pub const SOURCE_MASK: u8 = 64;

fn s(a: &[&str]) -> Vec<String> {
    a.iter().map(|x| x.to_string()).collect()
}

/// The chain's rules, each an `ip6tables` argument list appending to [`CHAIN`].
pub fn rules() -> Vec<Vec<String>> {
    let reject = ["-j", "REJECT", "--reject-with", "tcp-reset"];
    let mut connlimit = s(&["-A", CHAIN, "-p", "tcp", "--syn", "-m", "connlimit"]);
    connlimit.extend(s(&[
        "--connlimit-above",
        &MAX_CONNECTIONS.to_string(),
        "--connlimit-mask",
        &SOURCE_MASK.to_string(),
    ]));
    connlimit.extend(s(&reject));
    let mut hashlimit = s(&["-A", CHAIN, "-p", "tcp", "--syn", "-m", "hashlimit"]);
    hashlimit.extend(s(&[
        "--hashlimit-above",
        &format!("{NEW_PER_MINUTE}/minute"),
        "--hashlimit-burst",
        &NEW_BURST.to_string(),
        "--hashlimit-mode",
        "srcip",
        "--hashlimit-srcmask",
        &SOURCE_MASK.to_string(),
        "--hashlimit-name",
        "fcvm-mc-join",
    ]));
    hashlimit.extend(s(&reject));
    vec![connlimit, hashlimit]
}

/// The jump from `hook` to [`CHAIN`] for the join port, without the `-I`/`-C`.
pub fn jump(hook: &str, port: u16) -> Vec<String> {
    s(&[hook, "-p", "tcp", "--dport", &port.to_string(), "-j", CHAIN])
}

/// What it takes to make the jump `hook`'s first rule, from `ip6tables -S <hook>`.
#[derive(Debug, PartialEq, Eq)]
pub enum First {
    /// The jump is the first rule and the only copy.
    Keep,
    /// No jump: insert one at position 1.
    Insert,
    /// Delete the jump's `copies` and insert one at position 1, in one transaction.
    Move { copies: usize },
}

/// How often [`keep_first`] checks the jump is still first.
pub const KEEP_EVERY: Duration = Duration::from_secs(2);

/// Plan [`First`] from a listing of `hook` (`ip6tables -S <hook>`).
pub fn plan_first(listing: &str, hook: &str, port: u16) -> First {
    let jump = format!("-A {hook} -p tcp -m tcp --dport {port} -j {CHAIN}");
    let rules: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with(&format!("-A {hook} ")))
        .collect();
    match rules.iter().filter(|l| **l == jump).count() {
        0 => First::Insert,
        1 if rules[0] == jump => First::Keep,
        copies => First::Move { copies },
    }
}

/// The `ip6tables-restore --noflush` input that deletes the jump's `copies` and inserts it at
/// position 1. With the nf_tables backend it is one transaction: there is no moment without the
/// jump and none with two, and if the listing it was planned from is stale (a copy is gone) the
/// whole transaction is refused and nothing changes.
pub fn move_transaction(hook: &str, port: u16, copies: usize) -> String {
    let spec = jump(hook, port)[1..].join(" ");
    let mut t = String::from("*filter\n");
    for _ in 0..copies {
        t.push_str(&format!("-D {hook} {spec}\n"));
    }
    t.push_str(&format!("-I {hook} 1 {spec}\nCOMMIT\n"));
    t
}

/// Make the jump `hook`'s first and only copy. Returns what it did.
pub fn ensure_first(hook: &str, port: u16) -> Result<First> {
    let listing = must_output(&s(&["-S", hook]))?;
    let plan = plan_first(&listing, hook, port);
    match plan {
        First::Keep => {}
        First::Insert => {
            let mut insert = s(&["-I", hook, "1"]);
            insert.extend(jump(hook, port)[1..].iter().cloned());
            must(&insert)?;
        }
        First::Move { copies } => restore(&move_transaction(hook, port, copies))?,
    }
    Ok(plan)
}

static POKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Ask [`keep_first`] to check now: after starting a container, whose network setup can insert
/// rules ahead of the jump.
pub fn poke() {
    POKE.notify_one();
}

/// Keep the jump first for as long as the agent runs: every [`KEEP_EVERY`] and on each [`poke`].
pub async fn keep_first(hook: &'static str, port: u16) {
    let mut tick = tokio::time::interval(KEEP_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = POKE.notified() => {}
        }
        match tokio::task::spawn_blocking(move || ensure_first(hook, port)).await {
            Ok(Ok(First::Keep)) => {}
            Ok(Ok(done)) => tracing::warn!(hook, ?done, "put the join limits' jump back first"),
            Ok(Err(e)) => {
                tracing::warn!(hook, error = %format!("{e:#}"), "could not keep the join limits' jump first")
            }
            Err(e) => tracing::warn!(hook, error = %e, "the join limits' keeper task failed"),
        }
    }
}

/// ip6tables by its path where the system puts it: a caller's PATH need not hold /usr/sbin.
fn ip6tables() -> &'static str {
    ["/usr/sbin/ip6tables", "/sbin/ip6tables"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("ip6tables")
}

fn run(args: &[String]) -> std::io::Result<std::process::Output> {
    std::process::Command::new(ip6tables()).args(args).output()
}

fn must_output(args: &[String]) -> Result<String> {
    let out = run(args).with_context(|| format!("running ip6tables {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "ip6tables {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `ip6tables-restore --noflush` with `input`.
fn restore(input: &str) -> Result<()> {
    use std::io::Write;
    let bin = format!("{}-restore", ip6tables());
    let mut child = std::process::Command::new(&bin)
        .arg("--noflush")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running {bin}"))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .with_context(|| format!("writing to {bin}"))?;
    let out = child
        .wait_with_output()
        .with_context(|| format!("waiting for {bin}"))?;
    if !out.status.success() {
        bail!(
            "{bin} --noflush failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn must(args: &[String]) -> Result<()> {
    let out = run(args).with_context(|| format!("running ip6tables {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "ip6tables {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Install the chain and its rules, and jump to it from `hook` for TCP `port`. Idempotent: the
/// chain is created if missing and refilled, and the jump is made `hook`'s first and only copy
/// ([`ensure_first`]).
pub fn install(hook: &str, port: u16) -> Result<()> {
    let new = run(&s(&["-N", CHAIN])).context("running ip6tables -N")?;
    if !new.status.success() && !String::from_utf8_lossy(&new.stderr).contains("exists") {
        bail!(
            "ip6tables -N {CHAIN} failed: {}",
            String::from_utf8_lossy(&new.stderr).trim()
        );
    }
    must(&s(&["-F", CHAIN]))?;
    for r in rules() {
        must(&r)?;
    }
    ensure_first(hook, port)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const JUMP: &str = "-A FORWARD -p tcp -m tcp --dport 25565 -j FCVM-MC-JOIN";
    const NETAVARK: &str =
        "-A FORWARD -m comment --comment \"netavark firewall plugin rules\" -j NETAVARK_FORWARD";

    fn listing(rules: &[&str]) -> String {
        std::iter::once("-P FORWARD ACCEPT")
            .chain(rules.iter().copied())
            .map(|l| format!("{l}\n"))
            .collect()
    }

    #[test]
    fn netavark_inserting_its_jump_later_moves_the_join_jump_back_first() {
        // A staging host's FORWARD (2026-11-15): the agent inserted its jump at position 1, then
        // netavark inserted its own there when the Windows bridge containers started.
        assert_eq!(
            plan_first(&listing(&[NETAVARK, JUMP]), "FORWARD", JOIN_PORT),
            First::Move { copies: 1 }
        );
        assert_eq!(
            plan_first(&listing(&[JUMP, NETAVARK]), "FORWARD", JOIN_PORT),
            First::Keep
        );
        assert_eq!(
            plan_first(&listing(&[NETAVARK]), "FORWARD", JOIN_PORT),
            First::Insert
        );
        // A second copy makes every SYN pass the limits twice (two hashlimit tokens each).
        assert_eq!(
            plan_first(&listing(&[JUMP, NETAVARK, JUMP]), "FORWARD", JOIN_PORT),
            First::Move { copies: 2 }
        );
        // Only the exact jump counts: another port or chain is not it.
        let other = "-A FORWARD -p tcp -m tcp --dport 25566 -j FCVM-MC-JOIN";
        assert_eq!(
            plan_first(&listing(&[other]), "FORWARD", JOIN_PORT),
            First::Insert
        );
        assert_eq!(
            move_transaction("FORWARD", JOIN_PORT, 2),
            "*filter\n\
             -D FORWARD -p tcp --dport 25565 -j FCVM-MC-JOIN\n\
             -D FORWARD -p tcp --dport 25565 -j FCVM-MC-JOIN\n\
             -I FORWARD 1 -p tcp --dport 25565 -j FCVM-MC-JOIN\n\
             COMMIT\n"
        );
    }

    #[test]
    fn a_source_is_capped_by_open_connections_and_by_new_ones_a_minute() {
        let r: Vec<String> = rules().iter().map(|r| r.join(" ")).collect();
        assert_eq!(
            r,
            vec![
                "-A FCVM-MC-JOIN -p tcp --syn -m connlimit --connlimit-above 4 --connlimit-mask 64 -j REJECT --reject-with tcp-reset",
                "-A FCVM-MC-JOIN -p tcp --syn -m hashlimit --hashlimit-above 12/minute --hashlimit-burst 6 --hashlimit-mode srcip --hashlimit-srcmask 64 --hashlimit-name fcvm-mc-join -j REJECT --reject-with tcp-reset",
            ]
        );
        assert_eq!(
            jump("FORWARD", JOIN_PORT).join(" "),
            "FORWARD -p tcp --dport 25565 -j FCVM-MC-JOIN"
        );
    }
}
