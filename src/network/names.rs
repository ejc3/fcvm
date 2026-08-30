//! Names for the host objects one VM owns: its network namespace, its veth
//! pair, and its TAP device.
//!
//! Every name is derived from `vm_id` so a live VM is recognisable in `ip
//! netns list` and `ip link show`. The derivation has to be short: Linux caps
//! an interface name at 15 characters and the longest prefix built on it is
//! `veth0-`, which leaves nine. Nine characters of a `vm-<32 hex>` id is `vm-`
//! plus six hex digits, so the derived name is a guess at uniqueness, not an
//! identity. 100 clones draw a duplicate about 0.03% of the time; at the five
//! hex digits this used before it was 0.5%, which is what #888 hit.
//!
//! [`reserve`] therefore settles ownership with `ip netns add`, which fails
//! when the name is taken. That is the only compare-and-swap available here,
//! and it needs no host-wide lock: a caller that loses the race is told to
//! pick another base rather than being handed a namespace another VM's
//! interfaces already live in.

use anyhow::{Context, Result};
use tracing::warn;

use super::{namespace, veth};
use crate::state::truncate_id;

/// Characters of `vm_id` carried by a derived name.
pub const NAME_BASE_LEN: usize = 9;

/// Namespace name prefix, shared by the bridged and routed modes.
pub const NAMESPACE_PREFIX: &str = "fcvm-";

/// The longest interface prefix any caller builds on a base (`veth0-`).
const LONGEST_LINK_PREFIX: usize = 6;

/// Linux's IFNAMSIZ minus the trailing NUL.
const MAX_IF_NAME_LEN: usize = 15;

const _: () = assert!(
    LONGEST_LINK_PREFIX + NAME_BASE_LEN <= MAX_IF_NAME_LEN,
    "derived interface names must fit in IFNAMSIZ"
);

const _: () = assert!(
    NAME_BASE_LEN == 9,
    "candidate_base renders `vm-` plus six hex digits"
);

/// Bases to try before giving up.
const MAX_ATTEMPTS: u32 = 100;

/// The names reserved for one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmNetworkNames {
    /// The `vm-<hex>` fragment every name for this VM is built from.
    pub base: String,
    /// The network namespace, already created by [`reserve`].
    pub namespace: String,
}

impl VmNetworkNames {
    /// A name in this VM's family, e.g. `link("veth0-")` or `link("tap-")`.
    pub fn link(&self, prefix: &str) -> String {
        format!("{}{}", prefix, self.base)
    }
}

/// The name base to try on `attempt`.
///
/// Attempt 0 is the leading characters of `vm_id`, so the usual name still
/// reads back to the VM that owns it. Later attempts rehash the id with the
/// attempt number instead of appending a counter: appending pushes the veth
/// names past IFNAMSIZ, and truncating to make room lets two attempts land on
/// one name, since a hex id can end in digits (`vm-1111111` + `1` and
/// `vm-111111` + `11` both truncate to `vm-11111111`).
fn candidate_base(vm_id: &str, attempt: u32) -> String {
    if attempt == 0 {
        return truncate_id(vm_id, NAME_BASE_LEN).to_string();
    }

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    vm_id.hash(&mut hasher);
    attempt.hash(&mut hasher);
    format!("vm-{:06x}", hasher.finish() & 0xff_ffff)
}

/// Reserve a namespace, and with it a name base, for this VM.
///
/// `link_prefixes` are the interface prefixes the caller will create in the
/// host namespace from the returned base. They are probed so a base whose
/// interfaces leaked from a dead VM is skipped instead of failing later at
/// `ip link add`; creating the namespace is what actually settles ownership.
pub async fn reserve(vm_id: &str, link_prefixes: &[&str]) -> Result<VmNetworkNames> {
    for attempt in 0..MAX_ATTEMPTS {
        let base = candidate_base(vm_id, attempt);
        let names = VmNetworkNames {
            namespace: format!("{}{}", NAMESPACE_PREFIX, base),
            base,
        };

        if let Some(taken) = link_prefixes
            .iter()
            .map(|prefix| names.link(prefix))
            .find(|name| veth::link_exists(name))
        {
            warn!(
                vm_id = %vm_id, link = %taken, attempt,
                "interface name is taken, trying another name base"
            );
            continue;
        }

        match namespace::create_namespace(&names.namespace)
            .await
            .with_context(|| format!("creating network namespace {}", names.namespace))?
        {
            namespace::NamespaceCreation::Created => {
                if attempt > 0 {
                    warn!(
                        vm_id = %vm_id, namespace = %names.namespace, attempt,
                        "name base collided, reserved a rehashed base"
                    );
                }
                return Ok(names);
            }
            namespace::NamespaceCreation::AlreadyExists => {
                warn!(
                    vm_id = %vm_id, namespace = %names.namespace, attempt,
                    "namespace name belongs to another VM, trying another name base"
                );
            }
        }
    }

    anyhow::bail!(
        "no free network name base for {vm_id} after {MAX_ATTEMPTS} attempts. \
         Check for leaked namespaces with `ip netns list` and interfaces with `ip link show`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arm64 pair from #888 shares five hex digits and differs at the
    /// sixth, so the widened base alone separates it.
    #[test]
    fn ids_differing_at_the_sixth_hex_digit_derive_different_bases() {
        let a = candidate_base("vm-ca1c071bafd64b8a9ad3211f7fe5d7d0", 0);
        let b = candidate_base("vm-ca1c0a5654134eb3b0b854f7dc9710a", 0);
        assert_eq!(a, "vm-ca1c07");
        assert_eq!(b, "vm-ca1c0a");
        assert_ne!(a, b);
    }

    /// The x64 pair from #888 shares SIX hex digits, so widening does not
    /// separate it and never could have. Their derived names are equal, and
    /// only the later attempts move off. This is why `reserve` reserves
    /// rather than trusting the derivation.
    #[test]
    fn ids_sharing_six_hex_digits_still_collide_so_later_attempts_must_differ() {
        let a = "vm-e7f5d11346f04cc280d9f9db7dc45124";
        let b = "vm-e7f5d1a1d4fd4728a1216b803888393c";
        assert_eq!(candidate_base(a, 0), "vm-e7f5d1");
        assert_eq!(candidate_base(a, 0), candidate_base(b, 0));
        for attempt in 1..=8 {
            assert_ne!(
                candidate_base(a, attempt),
                candidate_base(b, attempt),
                "attempt {attempt} must separate two ids sharing six hex digits"
            );
        }
    }

    /// Every attempt has to stay inside IFNAMSIZ once a prefix is added.
    ///
    /// The distinctness check guards the rehash: the append-and-truncate
    /// scheme it replaced rendered two attempts as one name for an id ending
    /// in digits, wasting attempts on a candidate already rejected.
    #[test]
    fn every_attempt_fits_ifnamsiz_and_is_distinct() {
        let vm_id = "vm-1111111111111111111111111111111f";
        let mut seen = std::collections::HashSet::new();
        for attempt in 0..MAX_ATTEMPTS {
            let base = candidate_base(vm_id, attempt);
            assert_eq!(base.len(), NAME_BASE_LEN, "attempt {attempt}: {base}");
            assert!(
                LONGEST_LINK_PREFIX + base.len() <= MAX_IF_NAME_LEN,
                "attempt {attempt}: veth0-{base} exceeds IFNAMSIZ"
            );
            assert!(
                seen.insert(base.clone()),
                "attempt {attempt} repeats {base}"
            );
        }
    }

    /// A short id (tests and fixtures use them) must not panic or overrun.
    #[test]
    fn a_short_vm_id_is_carried_whole() {
        assert_eq!(candidate_base("vm-abc", 0), "vm-abc");
    }

    #[test]
    fn link_names_are_built_from_the_reserved_base() {
        let names = VmNetworkNames {
            base: "vm-e7f5d1".to_string(),
            namespace: "fcvm-vm-e7f5d1".to_string(),
        };
        assert_eq!(names.link("veth0-"), "veth0-vm-e7f5d1");
        assert_eq!(names.link("tap-"), "tap-vm-e7f5d1");
    }
}
