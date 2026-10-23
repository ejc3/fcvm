//! The slots' IPv6 addresses. Each GPU host has a public IPv6 prefix (a /80 that AWS
//! delegates to its network interface, or one given on the command line); a slot's VM runs
//! in fcvm's routed mode at `<prefix>::<GPU index + 2>`, so it keeps its address across
//! relaunches and a browser reaches it directly. fcvm refuses the prefix's addresses 0 and 1.

use anyhow::{bail, Context, Result};
use std::net::Ipv6Addr;

#[derive(Debug, Clone, PartialEq)]
pub struct Prefix {
    base: u128,
    len: u8,
}

impl Prefix {
    /// `2001:db8:1:2:3::/80`.
    pub fn parse(s: &str) -> Result<Prefix> {
        let (addr, len) = s
            .trim()
            .split_once('/')
            .with_context(|| format!("{s:?} is not an IPv6 prefix (<address>/<length>)"))?;
        let addr: Ipv6Addr = addr
            .parse()
            .with_context(|| format!("{s:?}: the address"))?;
        let len: u8 = len.parse().with_context(|| format!("{s:?}: the length"))?;
        if !(1..=124).contains(&len) {
            bail!("{s:?}: a prefix length must be 1 to 124");
        }
        let mask = u128::MAX << (128 - len);
        Ok(Prefix {
            base: u128::from(addr) & mask,
            len,
        })
    }

    /// The address of the slot on the GPU at `index` in the host's inventory.
    pub fn slot_address(&self, index: usize) -> Result<Ipv6Addr> {
        let host_bits = 128 - u32::from(self.len);
        let n = index as u128 + 2;
        if host_bits < 128 && n >> host_bits != 0 {
            bail!("{self} has no address for GPU {index}");
        }
        Ok(Ipv6Addr::from(self.base | n))
    }
}

impl std::fmt::Display for Prefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv6Addr::from(self.base), self.len)
    }
}

/// The IPv6 prefix delegated to the instance's primary network interface, from IMDS.
pub async fn from_imds() -> Result<Prefix> {
    let mac = crate::fleet::coord::aws::imds("mac").await?;
    let list = crate::fleet::coord::aws::imds(&format!(
        "network/interfaces/macs/{}/ipv6-prefix",
        mac.trim()
    ))
    .await
    .context("the primary interface has no delegated IPv6 prefix (Ipv6PrefixCount)")?;
    let first = list.lines().next().context("an empty IPv6 prefix list")?;
    Prefix::parse(first)
}

/// fcvm's routed mode needs root, so an agent given a prefix must run as root; refusing at
/// start beats every VM failing (and being retried) with the same configuration error.
pub fn check_routed_user(routed: bool, root: bool) -> Result<()> {
    if routed && !root {
        bail!(
            "--ipv6-prefix runs every VM in fcvm's routed mode, which needs root: run the agent as root"
        );
    }
    Ok(())
}

/// The `fcvm podman run` arguments that put a VM at `addr` in routed mode.
pub fn vm_args(prefix: &Prefix, addr: Ipv6Addr) -> Vec<String> {
    vec![
        "--network".into(),
        "routed".into(),
        "--ipv6-prefix".into(),
        prefix.to_string(),
        "--ipv6-address".into(),
        addr.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_routed_agent_must_be_root() {
        assert!(check_routed_user(true, false).is_err());
        assert!(check_routed_user(true, true).is_ok());
        assert!(check_routed_user(false, false).is_ok());
    }

    #[test]
    fn a_slot_address_is_the_gpu_index_plus_two_in_the_prefix() {
        let p = Prefix::parse("2001:db8:1:2:3:4:5:6/80").unwrap();
        assert_eq!(p.to_string(), "2001:db8:1:2:3::/80");
        assert_eq!(p.slot_address(0).unwrap().to_string(), "2001:db8:1:2:3::2");
        assert_eq!(p.slot_address(7).unwrap().to_string(), "2001:db8:1:2:3::9");
        let small = Prefix::parse("2001:db8::/124").unwrap();
        assert_eq!(small.slot_address(13).unwrap().to_string(), "2001:db8::f");
        assert!(small.slot_address(14).is_err(), "past the prefix");
        assert!(Prefix::parse("2001:db8::").is_err());
        assert!(Prefix::parse("2001:db8::/0").is_err());
    }
}
