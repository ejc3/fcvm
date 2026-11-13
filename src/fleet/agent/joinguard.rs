//! The host's limits on public Minecraft joins (docs/fleet/minecraft.md, "Public joins"): the
//! fleet security group admits TCP 25565 over IPv6 to the Minecraft VMs, which this host routes to
//! them, so the host's own filter is where a source's connections are counted. Per source /64 (an
//! IPv6 subscriber's usual prefix), new connections past [`MAX_CONNECTIONS`] open ones, or past
//! [`NEW_PER_MINUTE`] a minute after a burst of [`NEW_BURST`], are reset before they reach a VM.
//!
//! One chain, `FCVM-MC-JOIN`, flushed and filled again at every agent start, and one jump to it
//! from the hook chain (`FORWARD` on a host), added once. The player cap and the login timeout are
//! Pumpkin's own (fleet/minecraft/src/pumpkin.rs).

use anyhow::{bail, Context, Result};

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
/// chain is created if missing and refilled, and the jump is added only if it is not there.
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
    let mut check = s(&["-C"]);
    check.extend(jump(hook, port));
    if !run(&check).map(|o| o.status.success()).unwrap_or(false) {
        let mut insert = s(&["-I"]);
        let j = jump(hook, port);
        insert.push(j[0].clone());
        insert.push("1".into());
        insert.extend(j[1..].iter().cloned());
        must(&insert)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
