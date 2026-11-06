//! The host's gate on a restored clone's published UDP ports.
//!
//! A restored clone refuses new TCP connections from its first instruction: the gate
//! is in the snapshot. UDP has no such rule there, because the snapshot side cannot add
//! one without disturbing the source VM, so the guest adds its own UDP rejection after
//! it resumes (`gate_udp_ingress` in fc-agent's `snapshot_network.rs`). The host's
//! forwarders (pasta `-u`, the bridged DNAT rules) are live before the resume, so a
//! datagram that arrived in between reached the restored workload before its restore
//! hook had run (#1114).
//!
//! The host closes that window itself: from before the clone's TAP can carry a frame
//! until the guest acknowledges its restore, a `clsact` egress filter on the TAP drops
//! every UDP datagram to a published guest port, in both address families. The TAP's
//! egress is the guest's ingress, so the filter sees what pasta splices into the
//! namespace, what it sends at layer 2, and what the bridged namespace routes, which
//! iptables in a rootless namespace does not (pasta's TAP and the clone's are bridged).
//! The guest acknowledges after it has published, so the guest's own gate has come
//! and gone inside the host's.
//!
//! Dropped, not rejected: the guest's own gate refuses with an ICMP error, which the
//! sender of a datagram to a not-yet-published port sees either way, but a host-side
//! refusal would come from the host's namespace and name it. A dropped datagram is what
//! UDP allows; the clone is not serving yet.
//!
//! The filter matches the destination port only, so a reply to a flow the guest opened
//! from a published port during the restore is dropped too. That port is a listener's,
//! and its replies are not expected before publication.

use super::types::{PortMapping, Protocol};

/// The guest ports, in order and once each, of the UDP mappings.
pub fn published_udp_guest_ports(mappings: &[PortMapping]) -> Vec<u16> {
    let mut ports: Vec<u16> = mappings
        .iter()
        .filter(|m| m.proto == Protocol::Udp)
        .map(|m| m.guest_port)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// The `tc -batch` lines that close the gate on `tap` for `ports`. Empty when there is
/// no UDP port to gate.
pub fn close_batch(tap: &str, ports: &[u16]) -> String {
    if ports.is_empty() {
        return String::new();
    }
    let mut batch = format!("qdisc add dev {tap} clsact\n");
    for family in ["ip", "ipv6"] {
        for port in ports {
            batch.push_str(&format!(
                "filter add dev {tap} egress protocol {family} flower ip_proto udp \
                 dst_port {port} action drop\n"
            ));
        }
    }
    batch
}

/// The shell lines that run [`close_batch`] in one `tc` process. Empty when there is
/// nothing to gate.
pub fn close_script(tap: &str, ports: &[u16]) -> String {
    let batch = close_batch(tap, ports);
    if batch.is_empty() {
        return String::new();
    }
    format!("tc -batch - <<'{DELIMITER}'\n{batch}{DELIMITER}\n")
}

/// The `tc` arguments that open the gate: the `clsact` qdisc goes with its filters.
pub fn open_args(tap: &str) -> [&str; 5] {
    ["qdisc", "del", "dev", tap, "clsact"]
}

const DELIMITER: &str = "FCVM_TC_BATCH";

#[cfg(test)]
mod tests {
    use super::*;

    fn mappings(specs: &[&str]) -> Vec<PortMapping> {
        specs
            .iter()
            .map(|s| PortMapping::parse(s).unwrap())
            .collect()
    }

    #[test]
    fn only_udp_guest_ports_are_gated_once_each() {
        assert_eq!(
            published_udp_guest_ports(&mappings(&[
                "8080:80",
                "5353:53/udp",
                "127.0.0.1:5354:53/udp",
                "9000:9000/udp"
            ])),
            vec![53, 9000]
        );
        assert!(published_udp_guest_ports(&mappings(&["8080:80"])).is_empty());
        assert!(close_script("tap0", &[]).is_empty());
    }

    #[test]
    fn the_gate_drops_each_port_in_both_families() {
        let batch = close_batch("tap-x", &[53, 9000]);
        let lines: Vec<&str> = batch.lines().collect();
        assert_eq!(lines[0], "qdisc add dev tap-x clsact");
        for family in ["ip", "ipv6"] {
            for port in [53, 9000] {
                let line = format!(
                    "filter add dev tap-x egress protocol {family} flower ip_proto udp \
                     dst_port {port} action drop"
                );
                assert!(lines.contains(&line.as_str()), "missing {line}\n{batch}");
            }
        }
        assert_eq!(lines.len(), 5);
        assert_eq!(
            open_args("tap-x"),
            ["qdisc", "del", "dev", "tap-x", "clsact"]
        );
    }
}
