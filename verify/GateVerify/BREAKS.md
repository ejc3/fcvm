# What the search found in the network boundary

Every run here was found by `lake exe gate-breaks traces`, is recorded in `Breaks.lean` under
the name given, and is checked by the kernel on every build, under the rule book it breaks and
under the design. Steps are counted from a running guest that no boundary holds.

Rule books: `main` is 4adc1dee6, `flags` is #2113 at a1e5fc687, `cookieA` is #2124 at its first
commit a80e1ad34, `cookieB` is #2124 at its head 27add63dd.

"Where" says in which of fcvm's configurations the run can happen: rootless (pasta, the egress
proxy on, so the guest tracks connections all the time), routed (no proxy; the guest tracks
only while a rule asks, unless a port is published), a port published with or without
`/loopback`.

Severity: **high** is a client reaching a clone it must not, or a source's live connection
reset, in a configuration people run, with nothing unusual needed. **medium** needs a failure,
a kill or unusual timing, or a narrower configuration. **low** changes a setting or a
connection inside the guest and no client's.

## The table

`holds` is the search finding no reachable state that breaks the property, in the bounded
configurations of `Configs.lean`. A name is the first recorded run of that kind.

| | `main` | `flags` | `cookieA` | `cookieB` | design |
|---|---|---|---|---|---|
| P1 `sourceDisturbed` | `liveReset`, `barrierKill`, `reopenFails` | `barrierKill`, `reopenFails` | the same | the same | proved |
| P2 `generationBroken` | early snapshot, restart | the same | `twoDumps`, `proxyConnectA`, and those | `proxyConnect`, `restartRetires`, early snapshot | holds |
| P3 `staleAtPublication` | `staleTuple`, `staleTupleGrown`, `loopbackClient`, `earlySnapshot` | `cookieAck`, `loopbackClient`, `earlySnapshot` | `loopbackClientA`, `earlySnapshot` | `earlySnapshot`, `earlySnapshotFails` | holds |
| P3 `smuggledLate` | `staleTupleLate`, `halfOpenRetransmit` | `cookieAckClone` | `cookiesBackFirst` | holds | proved |
| P4 `retiredBehindEgress`, `hookMisplaced` | holds | holds | holds | holds | proved |
| P4 `workloadBehindEgress` | `watchdogThaw`, `restartEgress`, early snapshot | the same | the same | the same | proved |
| P5 `lastingChange` | holds | holds | `writeBackFails`, `restartCookies` | the same | proved |
| P5 `capturedChange` | after `barrierKill` | the same | and `failedCloneSnapshot`, `earlySnapshotCaptures` | the same | proved |
| P6 `stuck` | `hostDies`, after `reopenFails` | the same | the same | the same | proved |
| U1 `clientDatagram` | holds with the host's gate | the same | the same | the same | proved with it |

## Known breaks, reproduced

### `liveReset`: the gate resets a live connection of the source (#2106, second fault)

`main`. P1. Run `routed`, 5 steps. High. Fixed by #2113.

```
client A connects to the source (SYN, ACK)
snapshot create: the container is frozen; close, IPv4 chains and hooks
client A sends a segment on its connection
=> the gate's --ctstate NEW rule rejects it with a reset
```

Where: a guest that tracks nothing until the gate's own rule attaches the tracker (routed, no
published port). The first segment the tracker sees of a connection that was already open is
picked up as NEW (`tcp_loose`), and the rule answers it with a reset. In a guest that always
tracks, the entry has seen a reply and the segment passes. Under `flags` and later the rule
reads the packet's flags and the run ends with the property intact.

### `staleTuple`, `staleTupleLate`, `staleTupleGrown`: a SYN on a tracked tuple passes (#2106)

`main`. P3. Run `routed-stale`, 22 and 18 steps; run `routed`, 37 steps. High. Fixed by #2113.

```
(the tracker holds an ESTABLISHED entry for client A's tuple, and no socket)
snapshot create: freeze, close, barrier, dump
client A sends a SYN                  => not NEW: it passes; a request socket after the dump
save, pause; a clone is restored; its restore runs to publication
=> the clone is published with that request socket, which no manifest names
```

`staleTupleLate` is the same SYN arriving at the clone after its retirement. `staleTupleGrown`
starts with an empty tracker and shows where the entry comes from: during one snapshot the
gate's rule attaches the tracker, a client's handshake is tracked, the gate is removed and the
tracker detached before the client closes, so the entry stays ESTABLISHED; the next snapshot's
clone then lets that client in. This is the issue's account ("a connection that is open across
publication"), found without being told.

Where: routed, no published port. In a guest that always tracks, the entry of a closed
connection is CLOSE and a SYN recycles it as NEW.

### `halfOpenRetransmit`: the same in a guest that always tracks (new, on #2106)

`main`. P3 `smuggledLate`. Run `nat`, 19 steps. Medium. Fixed by #2113
(`flags_keeps_halfOpenRetransmit`). Reported as a comment on #2106.

```
client A sends a SYN; the listener answers and holds a request socket
snapshot create ... pause; a clone is restored
restore: verify, UDP rule, barrier, open_egress, load, SOCK_DESTROY
client A sends its SYN again          => the entry (SYN_RECV) ignores it: not NEW, it passes
=> a new request socket behind the clone's closed gate, after the retirement
```

A retired request socket sends no reset (`tcp_abort`, net/ipv4/tcp.c:5062-5069), so the
tracker's entry outlives it for its 60 s. Where: rootless, or any published port. It needs a
client whose SYN-ACK was lost across the snapshot.

### `cookieAck`, `cookieAckClone`: a SYN cookie's ACK passes the flag rule (#2123)

`flags`. P3. Run `cookie`, 23 and 19 steps. Medium. Fixed by #2124.

```
client A sends a SYN that finds the SYN queue full: a cookie, nothing kept
snapshot create: freeze, close, barrier, dump
client A sends the ACK                => the flag rule passes it; an established socket
save, pause; a clone is restored; its restore runs to publication
=> the clone is published with that socket, which no manifest names
```

`cookieAckClone` is the ACK arriving at the clone after its retirement. Where: every mode; it
needs a listener in SYN-queue overflow (or `tcp_syncookies=2`) at the snapshot. Under `main`
the same ACK is reset in a guest that tracks nothing and accepted where the tracker holds the
tuple as ESTABLISHED, as #2124's table measures.

### `loopbackClient`, `loopbackClientA`: a `/loopback` port's client looks like the guest itself

`main`, `flags`, `cookieA`. P3. Run `loopback-port`, 22 and 26 steps. High for such ports.
Fixed at 27add63dd (the review of a80e1ad34, P2).

```
client A connects to the port published with /loopback
snapshot create ... the manifest leaves the socket out: its peer is 127.0.0.1
a clone is restored and published
=> the clone holds the source's connection from client A
```

The nat INPUT rule that rewrites the client's source runs after the filter hook and before the
socket lookup, so the gate sees the client and the socket sees loopback. Under `cookieA` the
restore's own list skips a loopback peer the same way, so a socket made behind the gate on such
a port is published too. Where: `--publish HOST:GUEST/loopback`, rootless and routed.

### `twoDumps`, `oneDump`: the list is two dumps

`cookieA`, and `cookieB` without its second dump. P2. Run `pairs`, 21 steps. Low.

```
a clone is restored; its restore: cookies off, verify, UDP rule, barrier, load
its own list: the IPv4 dump
a process connects to a loopback listener from the guest's own address
its own list: the IPv6 dump           => the server's end is listed, the client's is not
open_egress; SOCK_DESTROY             => the server's end of a live connection is retired
```

The review of a80e1ad34 said a connection made inside the guest between the two dumps could
lose an end. The model agrees, with a limit the review did not have: at a80e1ad34 the end that
is listed must not record a loopback peer (those are kept by address), and a connect to the
guest's own address that is not loopback is refused by the closed egress half, so what is left
is a client that binds to the guest's address and connects to a loopback listener of the other
family. Real, and close to unreachable. `oneDump` is why 27add63dd, where the mirror image
alone decides, needs the second dump for every loopback connection: without it the same 21
steps retire an end of an ordinary one.

### `cookiesBackFirst`: `tcp_syncookies` written back before the gate is removed

`cookieA`. P3 `smuggledLate`. Run `cookie`, 26 steps. Medium. Fixed at 27add63dd.

```
client A holds a cookie from before the snapshot
... the clone's restore: cookies off, ..., retirement, thaw, hook
restore_syn_cookies: tcp_syncookies written back      (open(), first half)
client A sends the ACK                => accepted: the ingress half is still in
```

The review found this as "a failed removal leaves the clone unpublished and taking cookies".
The run needs no failure: the window between the write and the removal is enough.

### `failedCloneSnapshot`: can a snapshot capture `tcp_syncookies` at 0?

`cookieA`, `cookieB`. P5 `capturedChange`. Run `early-fails`, 21 steps (written by hand from
the question, checked like the others). Low. Part of #2138.

```
a clone is restored; its restore writes 0 to tcp_syncookies
the next step fails: the restore fails closed, on its way to shutdown_vm
snapshot create of that clone, before the power-off: prepare, pause
=> the image holds tcp_syncookies at 0
```

Yes. `failedCloneSnapshotEnds` then follows a clone of that image: its only step is the
power-off, as the PR says, because the agent in the image had already left its restore.
`earlySnapshotCaptures` (20 steps) is the other way to the same record, with no failure: a
restore that begins between a prepare's exit and the pause.

### `proxyConnectA`, `proxyConnect`: a connect behind the closed egress half

`cookieA`, `cookieB`. P2. Run `proxy`, 21 and 22 steps. Low. Documented at 27add63dd.

```
a clone is restored
a process outside the container connects out: nat OUTPUT redirects it to the proxy
the restore: cookies off, ..., its own list     => both sockets listed, neither a mirror image
open_egress; SOCK_DESTROY                       => both retired
```

Where: rootless only (the proxy), and only for a process the boundary does not freeze: a system
daemon, an `fcvm exec --vm` command. Under `main` and `flags` there is no list, so nothing is
retired; the connect behind the closed half is there all the same.

### `hostDies`: a dead `snapshot create` (#1936)

All four. P6. Run `host`, 9 steps. Known, open; #1941 is its fix.

## New breaks, in #2124 at its head

All five are in code #2124 does not change and so are in `main` and #2113 as well, except where
`tcp_syncookies` is named.

### `earlySnapshot`, `earlySnapshotFails`: a snapshot of a clone that has not published (#2138)

All four. P3, and P2, P4, P5 after it. Runs `early` (22 steps) and `early-fails` (13 steps).
Medium: an isolation break, and it needs a caller that snapshots a clone before `snapshot run`
has reported it ready.

```
client A connects to the source; snapshot create ... pause
a clone is restored and resumed
snapshot create of the clone, before its restore's first step:
  prepare: freeze, close (already closed), barrier, dump, save (the manifest is replaced)
  pause, resume
  the resume one-shot: reopen_with       => both halves of the clone's gate are removed
=> ingress open, the source's socket for client A in the clone, no restore
```

With a prepare that fails, `rollback_preparation` opens the gate at once. The one-shots open
whatever gate they find; nothing says whose it is.

### `restartCookies`, `restartEgress`, `restartRetires`: fc-agent restarted during a restore (#2139)

All four (`restartCookies` and `restartRetires` only where there are cookies and a list). P5,
P4, P2. Run `kills`, 38, 25 and 34 steps, one kill each. Medium.

```
a clone's restore: cookies off, ..., open_egress, [SOCK_DESTROY]
fc-agent is killed; systemd restarts it; the restore runs again from its first step
  refuse_syn_cookies reads 0 and records nothing
  the gate does not verify (a half is gone): close    => egress closed again (restartEgress)
  ..., its own list, with the clone's new connections in it
  SOCK_DESTROY                                        => they are retired (restartRetires)
  ..., open, restore_syn_cookies: nothing to write
=> published with tcp_syncookies at 0 (restartCookies)
```

On Firecracker a kill while egress is still closed does not get this far: the new process
cannot read its boot plan over MMDS through the closed half (`Cfg.mmds`), and the clone waits
for the host's timeout. Run `kills-vsock` has the boot plan over vsock, where every kill does.
With two kills a published clone is left gated with nothing to end it (P6, run `kills`, 31
steps): the first restart runs the restore on a serving clone and closes its gate, the second
kill leaves an agent that cannot start, and no host is waiting for an acknowledgement.

### `watchdogThaw`: the freeze watchdog thaws behind the closed gate (#2140)

All four. P4. Run `slow`, 11 steps. Medium: it needs a boundary held over 60 s of guest run
time, which large guests reach.

```
snapshot create ... pause; a clone is restored (frozen, both halves closed)
the payload-freeze watchdog thaws the container
=> the container runs with egress closed
```

### `barrierKill`: `rcu_expedited` left on (#2141)

All four. P1, then P5 `capturedChange`. Run `kills`, 7 steps. Low.

```
snapshot create: freeze, close, close; receive barrier: rcu_expedited written 1
the prepare one-shot is killed
the host runs the resume one-shot: gate opened, manifest removed, thaw
=> no boundary in progress, rcu_expedited still 1; the next snapshot's image carries it
```

### `reopenFails`: a failed reopen leaves the source gated (#2142)

All four. P1, and P6. Run `fails`, 11 steps. Medium.

```
snapshot create ... pause, resume
the resume one-shot fails to open the gate; it removes the manifest, thaws, exits non-zero
=> snapshot create has returned; both halves of the source's gate are in; nothing will retry
```

### `writeBackFails`: published without SYN cookies after a failed write

`cookieA`, `cookieB`. P5. Run `fails`, 26 steps. Low. Documented in #2124, which keeps the
clone published with a warning. The design keeps the value in the restore's record and writes
it until it lands (`REPAIRS.md`, 1).

## UDP (#2112)

The clone's UDP rule is `-m conntrack --ctdir ORIGINAL -j REJECT`, in from the restore's
second step (third, with cookies) until publication.

- **What a stale entry opens** (`replyDatagram`, run `udp`, 15 steps, every rule book and the
  design): a datagram from the address and port the guest itself sent to before the snapshot,
  to the port it sent from. The clone inherits the entry, the datagram is in its reply
  direction, and the rule passes it. That is the rule as written (`udp_ingress_rule_args`: "the
  rule cannot tell a late reply from a peer speaking again"). It opens that one reversed tuple,
  for the rest of the entry's timeout in guest run time (30 s, or 120 s once both sides have
  sent), and only in a guest that tracked the flow when it was made. Whether such a datagram
  can arrive at all is the host side's matter and is not in the model: a rootless clone's own
  pasta holds no flow for it, and a routed clone has taken an address the entry does not name.
- **What it does not open**: a datagram on a flow a client began is in the original direction
  of its entry, stale or new, and is rejected once the rule is in. A datagram from the guest's
  peer when the guest tracked nothing at the time is the origin of a new flow, and is rejected
  too.
- **Before the rule is in** nothing in the guest stops a datagram, a client's or a reply
  (`clientDatagram`, run `udp-reach`, 11 steps; #1114). The host does: its filter on the clone's TAP for published
  ports, and a routed clone's address. `design_noClientDatagram` is proved under that
  hypothesis and not without it.
- **Not in the model**: the entries each restore leaves in a guest that otherwise tracks
  nothing, which is the rest of #2112. Under the flag rule they no longer open anything for TCP
  (`design_keeps_staleTuple`).
