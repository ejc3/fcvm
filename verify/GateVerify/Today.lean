import GateVerify.Spec

/-!
# The code as it is: four rule books

Each is one commit of ejc3/fcvm, and each field cites the lines that make it so. `sn.rs` is
fc-agent/src/snapshot_network.rs.

| rule book | commit | what it is |
|---|---|---|
| `Today.main` | 4adc1dee6 | `origin/main` |
| `Today.flags` | a1e5fc687 | PR #2113, `gate-syn-flags` |
| `Today.cookieA` | a80e1ad34 | PR #2124, its first commit, the one that was reviewed |
| `Today.cookieB` | 27add63dd | PR #2124, its head |

What all four share, cited at 27add63dd (the same code is in the other three, at other lines):

- `snapshot create` on the host: the prepare one-shot, pause, resume, the resume one-shot, and
  the resume one-shot alone after a prepare that failed (src/commands/common.rs:6548-6600,
  6021). No check of the guest's restore state before the prepare (`snapUnpublished`).
- The prepare one-shot: freeze, then `prepare_with` under the boundary lock: close, barrier,
  dump, save (fc-agent/src/main.rs:108-118; sn.rs:2331-2393), and `rollback_preparation` on a
  failure (sn.rs:2322-2329).
- The resume one-shot: `reopen_with`, then `store.remove()`, whatever the reopen returned, then
  thaw (sn.rs:2779-2806; main.rs:119-126). A reopen that fails is reported in the exit status
  and nothing tries it again (`reopenRetry := false`). Nothing undoes what a dead
  `snapshot create` left (`guard := false`; #1936, open).
- The gate's chains (`gate_chain_rules`): in INPUT, TCP from a loopback source returns, anything
  that entered on `lo` returns, the TCP rule, return; in OUTPUT, TCP to a loopback destination
  returns, NTP is rejected, the TCP rule, return. A clone inserts its UDP rule third in INPUT
  (`udp_ingress_rule_args`, `-m conntrack --ctdir ORIGINAL`).
- The egress proxy's redirect is in nat OUTPUT and nothing of the gate is (`natGate := false`;
  fc-agent/src/proxy.rs:763-822).
- Both one-shots are exec'd through the long-running fc-agent (src/commands/common.rs:6021).
  When that agent has died, systemd has killed the unit's cgroup with it (the container, and a
  one-shot in progress) and starts a new one (`Restart=on-failure`, `Delegate=yes`, no
  `KillMode`, src/setup/rootfs.rs:1610-1626). The new process loops on its boot plan before it
  starts the exec server or the restore watcher (fc-agent/src/agent.rs:74-86, 215). A guest
  restored from a memory image is served no plan, so its agent never comes back and the
  restore is never run a second time; a cold-booted guest's comes back unless the fetch is
  over MMDS behind the closed egress half (`Spec.restart`). Read from the source; not run.
- A write of `tcp_syncookies` back that fails is tried three times and then the restore goes on
  with a warning (`restore_syn_cookies`; `backFails := false`).
- The receive barrier turns `/sys/kernel/rcu_expedited` on and off around its grace period
  (sn.rs:486-529), and nothing else ever writes it back (`rcuUndo := false`).
- The payload-freeze watchdog thaws any freeze it has seen 60 times, a second apart, whatever
  the restore is doing (fc-agent/src/payload_freeze.rs:87, 419; `wdMidBoundary := true`).
-/

namespace Gate

namespace Today

/-- `origin/main`, 4adc1dee6.

- `tcpByCt`: the TCP rule is `-p tcp -m conntrack --ctstate NEW -j REJECT --reject-with
  tcp-reset` (sn.rs:943-954, in `gate_chain_rules`, sn.rs:912-957).
- `manAddr`: the manifest is the dump less every socket `is_preserved` (sn.rs:2075-2086), which
  is a loopback peer, IPv4-mapped included (sn.rs:117-124).
- `list := none`: the restore retires the manifest's sockets and takes no list of its own
  (sn.rs:2248-2290).
- `restore` (`restore_boundary_with`, sn.rs:2142-2313): `is_closed` and `close` (2188, 2202),
  `gate_udp_ingress` (2208), the barrier (2218), `open_egress` (2243), `store.load()` (2248),
  `destroy` (2264), `store.remove()` (2291); then, in `handle_clone_restore`,
  `payload_freeze::thaw` (fc-agent/src/restore.rs:792) and `publish` (restore.rs:814), which is
  the hook (sn.rs:2369) and `reopen_with` (sn.rs:2374). -/
def main : Rules where
  name := "main"
  tcpByCt := true
  manAddr := true
  list := .none
  look := false
  restore := [.verify, .udp, .barIn, .barOut, .egress, .load, .retire, .disarm, .thaw, .hook,
              .ingress]
  natGate := false
  wdMidBoundary := true
  snapUnpublished := true
  reopenRetry := false
  backFails := false
  startWaits := false
  guard := false
  rcuUndo := false

/-- PR #2113, a1e5fc687. One change against `main`:

- `tcpByCt := false`: the TCP rule is `TCP_NEW_FLOW_REJECT`, `-p tcp -m tcp --tcp-flags SYN,ACK
  SYN -j REJECT --reject-with tcp-reset` (sn.rs:990-1002, used at sn.rs:942).

The manifest (sn.rs:2142-2153, `is_preserved` at sn.rs:117-124) and the restore's order
(sn.rs:2209-2380: 2255, 2269, 2275, 2285, 2310, 2315, 2331, 2358; restore.rs:795, 818;
sn.rs:2436, 2441) are `main`'s. -/
def flags : Rules where
  name := "flags"
  tcpByCt := false
  manAddr := true
  list := .none
  look := false
  restore := main.restore
  natGate := false
  wdMidBoundary := true
  snapUnpublished := true
  reopenRetry := false
  backFails := false
  startWaits := false
  guard := false
  rcuUndo := false

/-- PR #2124 at its first commit, a80e1ad34.

- `restore` (`restore_boundary_with`, sn.rs:2376-2575): `refuse_syn_cookies` first (2423), then
  `is_closed` and `close` (2426, 2440), `gate_udp_ingress` (2446), the barrier (2456),
  `store.load()` (2474), the dump and `unlisted_sockets` (2475), `open_egress` (2493),
  `destroy` (2515), `store.remove()` (2547); thaw (restore.rs:795); the hook (sn.rs:2626); and
  `open` (sn.rs:1686-1692), which writes `tcp_syncookies` back BEFORE it removes the gate.
- `list := addr`: `unlisted_sockets` (sn.rs:1856-1880) skips a socket the manifest names or
  whose peer is loopback (1871), and one whose mirror image is in the same list (1874-1876).
- `look := false`: the list is one `dump_tcp_sockets`, which is the IPv4 dump and then the IPv6
  dump.
- `manAddr`: the manifest still leaves out by address (sn.rs:2314). -/
def cookieA : Rules where
  name := "cookieA"
  tcpByCt := false
  manAddr := true
  list := .addr
  look := false
  restore := [.cookiesOff, .verify, .udp, .barIn, .barOut, .load, .dump4, .dump6, .egress,
              .retire, .disarm, .thaw, .hook, .cookiesBack, .ingress]
  natGate := false
  wdMidBoundary := true
  snapUnpublished := true
  reopenRetry := false
  backFails := false
  startWaits := false
  guard := false
  rcuUndo := false

/-- PR #2124 at its head, 27add63dd.

- `restore` (`restore_boundary_with`, sn.rs:2432-2621): `refuse_syn_cookies` (2479), `is_closed`
  and `close` (2482, 2496), `gate_udp_ingress` (2502), the barrier (2512), `store.load()` (2530),
  `unlisted_sockets` (2531), `open_egress` (2545), `destroy` (2567), `store.remove()` (2599);
  thaw (restore.rs:795); the hook (sn.rs:2678); and `open` (sn.rs:1684-1694), which removes the
  gate and then writes `tcp_syncookies` back.
- `manAddr := false`: `manifest_sockets` (sn.rs:1902-1907) leaves a socket out only when its
  peer is loopback and its mirror image is in the list (`sockets_leaving_the_guest`,
  sn.rs:1854-1883).
- `list := mirror`: `unlisted_sockets` (sn.rs:1934-1947) keeps a socket the manifest does not
  name only when its mirror image is in the list.
- `look`: when a socket that may stay has no mirror image, the sockets are dumped once more
  (sn.rs:1872-1881). -/
def cookieB : Rules where
  name := "cookieB"
  tcpByCt := false
  manAddr := false
  list := .mirror
  look := true
  restore := [.cookiesOff, .verify, .udp, .barIn, .barOut, .load, .dump4, .dump6, .look,
              .egress, .retire, .disarm, .thaw, .hook, .ingress, .cookiesBack]
  natGate := false
  wdMidBoundary := true
  snapUnpublished := true
  reopenRetry := false
  backFails := false
  startWaits := false
  guard := false
  rcuUndo := false

/-- 27add63dd with its second dump taken out: what its red run "one dump" removes. Not a state
of the code; it shows what the second dump is for. -/
def cookieBOneLook : Rules :=
  { cookieB with
    name := "cookieB-one-dump"
    look := false
    restore := cookieB.restore.filter (· != .look) }

end Today

end Gate
