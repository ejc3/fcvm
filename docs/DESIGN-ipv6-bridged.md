# Design: IPv6 Bridged Networking Without NAT

## Problem Statement

Bridged networking mode currently requires iptables NAT (MASQUERADE) for egress traffic. On IPv6-only hosts where the `nf_nat` kernel module is blocked by policy, bridged mode fails:

```
iptables v1.8.10: can't initialize iptables table `nat': Table does not exist
```

This affects environments where NAT modules are intentionally blacklisted:
```
/etc/modprobe.d/modprobe.conf:blacklist nf_nat
/etc/modprobe.d/modprobe.conf:install nf_nat /bin/true
```

## Current Architecture (NAT-based)

```
┌─────────────────────────────────────────────────────────────┐
│ Host                                                        │
│                                                             │
│   eth0 (2401:db00:...:7dd1)                                │
│     │                                                       │
│     │ iptables MASQUERADE  ◄── BLOCKED (no nf_nat)         │
│     │                                                       │
│   veth-host ◄────────────► veth-vm                         │
│                              │                              │
│                         [namespace]                         │
│                              │                              │
│                           bridge                            │
│                              │                              │
│                            tap0                             │
│                              │                              │
│                      ┌───────┴───────┐                      │
│                      │  Firecracker  │                      │
│                      │  172.30.x.x   │ (private IPv4)       │
│                      └───────────────┘                      │
└─────────────────────────────────────────────────────────────┘

Traffic flow:
  VM (172.30.x.x) → MASQUERADE → Host IP → Internet
                    ^^^^^^^^^^^
                    requires nf_nat
```

## Proposed Architecture (Direct IPv6)

Native IPv6 routing without NAT. VMs get real routable IPv6 addresses.

```
                          Internet
                              │
                         [Router]
                              │
                      2401:db00::/48
                              │
┌─────────────────────────────┴─────────────────────────────────┐
│ Host                                                          │
│                                                               │
│   eth0: 2401:db00:...:7dd1/128                               │
│     │                                                         │
│     │  IPv6 forwarding (no NAT)                              │
│     │  sysctl net.ipv6.conf.all.forwarding=1                 │
│     │                                                         │
│   veth-host ◄────────────► veth-vm                           │
│   fe80::1                  fe80::2                            │
│                              │                                │
│                         [namespace]                           │
│                              │                                │
│                           bridge                              │
│                              │                                │
│                            tap0                               │
│                              │                                │
│                      ┌───────┴───────┐                        │
│                      │  Firecracker  │                        │
│                      │  VM           │                        │
│                      │  2401:db00:...:7dd2  (real IPv6)      │
│                      └───────────────┘                        │
└───────────────────────────────────────────────────────────────┘

Traffic flow (no NAT):
  1. VM sends packet (src: ::7dd2, dst: internet)
  2. Packet traverses: tap0 → bridge → veth-vm → veth-host → eth0
  3. Host forwards unchanged (src still ::7dd2)
  4. Router routes reply back to host
  5. Host forwards to VM
```

## Requirements

### Option A: Delegated Prefix

Infrastructure delegates a /64 (or smaller) prefix to each host for VM use.

| Requirement | Description |
|-------------|-------------|
| Prefix delegation | Host receives /64 via DHCPv6-PD or static config |
| Routing | Router knows prefix is reachable via host |
| Address allocation | fcvm allocates VM addresses from prefix |
| Forwarding | `net.ipv6.conf.all.forwarding=1` |

**Pros:**
- Clean, standard IPv6 architecture
- No hacks or proxies
- Scales to many VMs per host

**Cons:**
- Requires infrastructure changes (prefix delegation)
- Each host needs unique prefix

### Option B: NDP Proxy with Static Routes

Router has static routes for individual VM addresses.

| Requirement | Description |
|-------------|-------------|
| Static routes | Router configured with /128 routes per VM via host |
| NDP proxy | Host answers NDP for VM addresses |
| Forwarding | `net.ipv6.conf.all.forwarding=1` |

**Pros:**
- Works with existing /128 host addressing
- No prefix delegation needed

**Cons:**
- Requires per-VM router configuration
- Doesn't scale well
- Operational complexity

### Option C: IPv6 NPTv6 (Prefix Translation)

Use NPTv6 (RFC 6296) for stateless prefix translation. Similar to NAT but 1:1 mapping.

| Requirement | Description |
|-------------|-------------|
| NPTv6 support | Kernel module or userspace implementation |
| ULA prefix | VMs use fd00::/64 internally |
| Translation | fd00::x ↔ 2401:db00::x at host boundary |

**Pros:**
- VMs get predictable internal addresses
- Stateless (no connection tracking)

**Cons:**
- Still requires kernel support (may be blocked)
- Breaks end-to-end principle

## Comparison with Rootless Mode

| Aspect | Bridged (current) | Bridged (direct IPv6) | Rootless |
|--------|-------------------|----------------------|----------|
| NAT required | Yes (iptables) | No | Yes (userspace/slirp) |
| Kernel modules | nf_nat (blocked) | None | None |
| Performance | Good | Best | Good |
| IPv6 egress | Via NAT66 | Native | Via slirp |
| Infra changes | None | Prefix delegation | None |
| Works today | No (on this host) | No (needs infra) | **Yes** |

## Recommendation

### Short-term: Use Rootless Mode

Rootless mode already works on IPv6-only hosts without kernel NAT:
- Uses slirp4netns for userspace NAT
- IPv6 DNS works via slirp's fd00::3 + NDP Neighbor Advertisement
- Guest sends gratuitous NDP NA at boot to teach slirp its MAC
- All tests pass

### Long-term: Direct IPv6 with Delegated Prefix

If bridged mode performance is needed:
1. Work with infrastructure to enable IPv6 prefix delegation
2. Implement Option A (cleanest architecture)
3. Add `--network bridged-ipv6` mode that:
   - Detects/requests delegated prefix
   - Allocates VM addresses from prefix
   - Sets up pure IPv6 forwarding (no NAT)

## Implementation Sketch (Option A)

```rust
// New network mode
pub enum NetworkMode {
    Bridged,       // Current: IPv4 + iptables NAT
    BridgedIPv6,   // New: IPv6 + direct routing
    Rootless,      // Current: slirp4netns
}

impl BridgedIPv6Network {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        // 1. Get delegated prefix from DHCPv6-PD or config
        let prefix = self.get_delegated_prefix()?;

        // 2. Allocate VM address from prefix
        let vm_addr = self.allocate_address(&prefix)?;

        // 3. Create veth pair
        self.create_veth_pair()?;

        // 4. Enable IPv6 forwarding
        self.enable_ipv6_forwarding()?;

        // 5. Add route for VM via veth
        self.add_vm_route(&vm_addr)?;

        // No iptables/NAT needed!

        Ok(NetworkConfig {
            guest_ip: Some(vm_addr),
            // ...
        })
    }
}
```

## Open Questions

1. **Prefix delegation mechanism**: DHCPv6-PD? Static config? API call to infra?
2. **Address allocation**: How to coordinate across hosts to avoid conflicts?
3. **DNS**: Use host's DNS directly? Run local resolver?
4. **Firewall**: Any filtering needed without NAT? Security implications?
5. **Fallback**: Auto-detect and fall back to rootless if no prefix available?

## References

- [RFC 6296 - NPTv6](https://tools.ietf.org/html/rfc6296)
- [RFC 8415 - DHCPv6](https://tools.ietf.org/html/rfc8415) (Prefix Delegation)
- [Linux IPv6 Routing](https://www.kernel.org/doc/html/latest/networking/ipv6.html)
- [NDP Proxy](https://man7.org/linux/man-pages/man8/ip-neighbour.8.html)
