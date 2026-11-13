//! The host ports a GPU host publishes its places' TLS on, for viewers with no IPv6: one public
//! IPv4 address per host, a port per place. `fleet/published-ports.json` declares them once, by
//! mode; the agent allocates only from it, the viewer-path smoke checks every published URL
//! against it, and ejc3/aws derives the security group's range from it. A port outside the
//! declaration would be one the security group drops, so the agent refuses to start instead.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::sync::LazyLock;

/// The declaration, as the repository holds it.
pub const SOURCE: &str = include_str!("../../../fleet/published-ports.json");

#[derive(Debug, Deserialize)]
pub struct Declared {
    pub ipv6_port: u16,
    pub ec2_tile_ipv4_port: u16,
    pub gpu_host_ipv4: HostIpv4,
}

#[derive(Debug, Deserialize)]
pub struct HostIpv4 {
    /// The production `--tls-publish-base`. Offsets are relative to the base the agent is given.
    pub base: u16,
    pub ranges: Vec<Range>,
}

#[derive(Debug, Deserialize)]
pub struct Range {
    pub mode: Mode,
    pub offset: u16,
    pub count: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// A GPU slot: a wall renderer or a phone, by its GPU's inventory index.
    Slot,
    Windows,
    Minecraft,
    Aggregate,
}

static DECLARED: LazyLock<Declared> = LazyLock::new(|| {
    serde_json::from_str(SOURCE).expect("fleet/published-ports.json parses (ports::tests)")
});

pub fn declared() -> &'static Declared {
    &DECLARED
}

impl Declared {
    pub fn range(&self, mode: Mode) -> &Range {
        self.gpu_host_ipv4
            .ranges
            .iter()
            .find(|r| r.mode == mode)
            .expect("every mode has a range (ports::tests)")
    }
}

/// The host port of place `index` of `mode`, with `--tls-publish-base` `base`.
pub fn port(base: u16, mode: Mode, index: usize) -> Result<u16> {
    let r = declared().range(mode);
    if index >= r.count as usize {
        bail!(
            "fleet/published-ports.json gives {:?} {} published ports; place {index} has none \
             (a port outside the declaration is one the security group drops)",
            mode,
            r.count
        );
    }
    base.checked_add(r.offset)
        .and_then(|p| p.checked_add(index as u16))
        .with_context(|| format!("--tls-publish-base {base} leaves no port for {mode:?} {index}"))
}

/// Every place's TLS host port on one host, by index; all `None` without a base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPorts {
    pub slots: Vec<Option<u16>>,
    pub windows: Vec<Option<u16>>,
    pub minecraft: Vec<Option<u16>>,
}

/// The TLS host ports of a host's `slots` GPU slots (the wall's GPUs, then the phones'),
/// `windows` sandboxes and `minecraft` servers.
pub fn allocate(
    base: Option<u16>,
    slots: usize,
    windows: usize,
    minecraft: usize,
) -> Result<HostPorts> {
    let Some(b) = base else {
        return Ok(HostPorts {
            slots: vec![None; slots],
            windows: vec![None; windows],
            minecraft: vec![None; minecraft],
        });
    };
    let each = |mode, n: usize| {
        (0..n)
            .map(|i| port(b, mode, i).map(Some))
            .collect::<Result<Vec<_>>>()
    };
    Ok(HostPorts {
        slots: each(Mode::Slot, slots)?,
        windows: each(Mode::Windows, windows)?,
        minecraft: each(Mode::Minecraft, minecraft)?,
    })
}

/// `--aggregate-tls-port` must be the declared aggregator port of `--tls-publish-base`.
pub fn check_aggregate(base: Option<u16>, aggregate: Option<u16>) -> Result<()> {
    if let (Some(b), Some(a)) = (base, aggregate) {
        let want = port(b, Mode::Aggregate, 0)?;
        if a != want {
            bail!(
                "--aggregate-tls-port {a} is not the aggregator's declared port {want} \
                 (--tls-publish-base {b}, fleet/published-ports.json)"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(r: &Range) -> std::ops::Range<u32> {
        r.offset as u32..r.offset as u32 + r.count as u32
    }

    #[test]
    fn the_declaration_parses_and_its_ranges_tile_one_span() {
        let d = declared();
        assert_eq!(d.ipv6_port, 443);
        let mut ranges: Vec<_> = d.gpu_host_ipv4.ranges.iter().collect();
        ranges.sort_by_key(|r| r.offset);
        let mut next = 0u32;
        for r in &ranges {
            assert_eq!(
                r.offset as u32, next,
                "{:?} leaves a gap or overlaps",
                r.mode
            );
            assert!(r.count > 0, "{:?} declares no port", r.mode);
            next += r.count as u32;
        }
        assert!(d.gpu_host_ipv4.base as u32 + next <= u16::MAX as u32 + 1);
        for m in [Mode::Slot, Mode::Windows, Mode::Minecraft, Mode::Aggregate] {
            assert_eq!(ranges.iter().filter(|r| r.mode == m).count(), 1, "{m:?}");
        }
    }

    /// Every place a host can run gets a port inside its own mode's declared range, whatever
    /// else the host runs: a host with fewer GPUs does not move its sandboxes into the slots'
    /// range, and one place more than the declaration allows is refused, not published on a
    /// port the security group drops.
    #[test]
    fn every_mode_allocates_inside_its_declared_range() {
        let d = declared();
        let base = d.gpu_host_ipv4.base;
        let inside = |mode: Mode, p: Option<u16>| {
            let p = p.expect("a port with a base") as u32 - base as u32;
            assert!(span(d.range(mode)).contains(&p), "{mode:?} got offset {p}");
        };
        let full = |m: Mode| d.range(m).count as usize;
        for slots in [1, 6, full(Mode::Slot)] {
            for windows in [0, 1, full(Mode::Windows)] {
                for minecraft in [0, 2, full(Mode::Minecraft)] {
                    let got = allocate(Some(base), slots, windows, minecraft).unwrap();
                    got.slots.iter().for_each(|p| inside(Mode::Slot, *p));
                    got.windows.iter().for_each(|p| inside(Mode::Windows, *p));
                    got.minecraft
                        .iter()
                        .for_each(|p| inside(Mode::Minecraft, *p));
                }
            }
        }
        assert!(allocate(Some(base), full(Mode::Slot) + 1, 0, 0).is_err());
        assert!(allocate(Some(base), 1, full(Mode::Windows) + 1, 0).is_err());
        assert!(allocate(Some(base), 1, 0, full(Mode::Minecraft) + 1).is_err());
        let none = allocate(None, 9, 5, 5).unwrap();
        assert!(none
            .slots
            .iter()
            .chain(&none.windows)
            .chain(&none.minecraft)
            .all(Option::is_none));
    }

    #[test]
    fn the_aggregator_port_must_be_the_declared_one() {
        let base = declared().gpu_host_ipv4.base;
        let agg = port(base, Mode::Aggregate, 0).unwrap();
        assert!(check_aggregate(Some(base), Some(agg)).is_ok());
        assert!(check_aggregate(Some(base), Some(base + 8)).is_err());
        assert!(check_aggregate(None, Some(1234)).is_ok());
        assert!(check_aggregate(Some(base), None).is_ok());
    }
}
