# What the search found in the network boundary

Every run here is recorded in `Breaks.lean` under the name given and checked by the kernel on
every build, under the rule book it breaks. Most were found by `lake exe gate-breaks traces`;
three are written by hand (`handRuns` in `GateSearch.lean`), where the search reports a break
only through a shorter run of another kind. Steps are counted from a running guest that no
boundary holds and that booted without the snapshot cache.

Rule books are fixed commits, named for what the boundary does at each and not for a branch:
`ctstate` is 4adc1dee6, where the gate's TCP rule is `--ctstate NEW` (what `main` was before
#2113); `flags` is #2113 at a1e5fc687; `cookieA` is #2124 at its first commit a80e1ad34;
`cookieB` is #2124 at its head. `README.md`, "The rule books", says which one `main` matches
now.

Nothing here was run in a VM. Each run is a run of the model, whose rules are read from the
source at those commits and from Linux v6.18.60; "by source" below means exactly that.

"Where" says in which of fcvm's configurations the run can happen: rootless (pasta, the egress
proxy on, so the guest tracks both families all the time), routed (no proxy; the guest tracks a
family only while a rule of that family asks), a port published with `/loopback` (IPv4 nat
rules, so IPv4 is tracked all the time and IPv6 is not).

Two things decide which runs a real guest can take, and neither is a step of the model:

- **Whether the guest was restored.** A guest restored from a memory image is served no boot
  plan, so an fc-agent that dies in it never comes back. Most guests are restored guests: a
  `podman run` that hits the snapshot cache goes through the restore path. "Booted without the
  cache" below means a guest whose agent can be served its plan again.
- **Firecracker or Cloud Hypervisor.** On Firecracker the boot plan is fetched over MMDS, a TCP
  connection the gate's egress half refuses. On Cloud Hypervisor it is over vsock, which the
  gate (iptables rules) does not see. Every configuration here is Firecracker's but the three
  named `vsock`.

Severity: **high** is a client reaching a clone it must not, or a source's live connection
reset, in a configuration people run, with nothing unusual needed. **medium** needs a failure,
a kill or unusual timing, or a narrower configuration. **low** changes a setting or a
connection inside the guest and no client's.

## The table

`holds` is the search finding no reachable state that breaks the property in the 20
configurations of `Configs.lean` that run by default (17 with the boot plan over MMDS, 3 over
vsock). CI runs that search and compares its result with `search.expected`. A name is a
recorded run of that kind. The two `all-` configurations, which turn everything on at once,
are in the last section.

| | `ctstate` | `flags` | `cookieA` | `cookieB` | design |
|---|---|---|---|---|---|
| P1 `sourceDisturbed` | `liveReset`, `agentDiesSource`, `barrierKill`, `reopenFails` | `agentDiesSource`, `barrierKill`, `reopenFails` | the same | the same | proved |
| P2 `generationBroken` | after an early snapshot | the same | `twoDumps`, `proxyConnectA`, and that | `proxyConnect`, and that | proved, for the connections the model makes (`Limits.lean`) |
| P3 `staleAtPublication` | `staleTuple`, `staleTupleGrown`, `loopbackClient`, `proxyStale`, `earlySnapshot` | `cookieAck`, `loopbackClient`, `proxyStale`, `earlySnapshot` | `loopbackClientA`, `earlySnapshot` | `earlySnapshot`, `earlySnapshotFails` | proved |
| P3 `smuggledLate` | `staleTupleLate`, `halfOpenRetransmit`, `reusedTuple` | `cookieAckClone` | `cookiesBackFirst` | holds | proved |
| P4 `retiredBehindEgress`, `hookMisplaced` | holds | holds | holds | holds | proved |
| P4 `workloadBehindEgress` | `watchdogThaw`, after an early snapshot, and over vsock `containerMidBoundary` | the same | the same | the same | proved |
| P5 `lastingChange` | holds | holds | `writeBackFails` | the same | proved |
| P5 `capturedChange` | `barrierKillCaptured`, after an early snapshot | the same | and `failedCloneSnapshot`, `earlySnapshotCaptures`, after `writeBackFails` | the same | proved |
| P6 `stuck` | `agentDies`, `agentDiesVsock`, `hostDies`, `reopenFailsStuck` | the same | the same | the same | proved |
| U1 `clientDatagram` | `clientDatagram` without the host's gate | the same | the same | the same | the same: assumed, not proved |

What the design does with each recorded run is in `Breaks.lean` beside it: `design_refuses_`
(a step of the run is not possible under the design) or `design_keeps_` (the run is possible
and the property holds at its end), with the run aligned to the design's restore order. Those
are about one run each. The column above is the general theorems of `Proofs.lean`.

## Known breaks, reproduced

### `liveReset`: the gate resets a live connection of the source (#2106, second fault)

`ctstate`. P1. Run `routed`, 5 steps. High. Fixed by #2113.

```
client A connects to the source (SYN, ACK)
snapshot create: the container is frozen; close, IPv4 chains and hooks
client A sends a segment on its connection
=> the gate's --ctstate NEW rule rejects it with a reset
```

Where: a guest that does not track the client's family until the gate's own rule attaches the
tracker (routed; IPv6 in a guest whose nat rules are IPv4 only). The first segment the tracker
sees of a connection that was already open is picked up as NEW (`tcp_loose`), and the rule
answers it with a reset. Where the family is always tracked, the entry has seen a reply and the
segment passes. Under `flags` and later the rule reads the packet's flags.

### `staleTuple`, `staleTupleLate`, `staleTupleGrown`: a SYN on a tracked tuple passes (#2106)

`ctstate`. P3. Run `routed-stale`, 22 and 18 steps; run `routed`, 37 steps. High. Fixed by #2113.

```
(the tracker holds an ESTABLISHED entry for client A's tuple, and no socket)
snapshot create: freeze, close, barrier, dump
client A sends a SYN                  => not NEW: it passes; a request socket after the dump
save, pause; a clone is restored; its restore runs to publication
=> the clone is published with that request socket, which no manifest names
```

`staleTupleLate` is the same SYN arriving at the clone after its retirement.

`staleTupleGrown` starts with an empty tracker and shows where the entry comes from, by the
issue's own route. Tracking is per address family (`nf_ct_netns_get` takes the rule's family,
xt_conntrack.c:266-280; `users4` and `users6` are counted apart, nf_conntrack_proto.c:441-470),
and the hooks stay attached a little after the last rule that used them is gone:

```
snapshot create ... the resume one-shot opens the gate (IPv4 tracking still attached)
client A connects (SYN, ACK): the handshake is tracked, ESTABLISHED
the tracker's hooks are detached
client A closes: nobody tracks the FIN, the entry stays ESTABLISHED
snapshot create: freeze, close, barrier, dump
client A sends a SYN                  => not NEW: it passes
save, pause; a clone is restored and published with that request socket
```

An earlier version of this model recorded a run in which an IPv6 handshake was tracked while
only the IPv4 half of the gate was in. That cannot happen: an iptables rule attaches IPv4 hooks
only. The run above replaces it.

An ACK for no socket does not do the same. The second version of this model recorded such a
run (`strayAck`): the ACK is picked up as an entry, the guest answers with a reset, and the
entry was taken to stay and to let a later SYN through. It does not stay: the reset is that
entry's only reply, and the tracker deletes an unreplied entry on a reset
(nf_conntrack_proto_tcp.c:1344-1352). The run is withdrawn, and with the rule corrected
`staleTupleGrown` is the shortest run the search finds on `ctstate` here.

Where: every family the guest tracks only while a rule asks. That is both families in routed
mode with no nat rule, and IPv6 in a guest whose nat rules are IPv4 only: a `/loopback` port's
DNAT and SNAT are `iptables` rules (fc-agent/src/network.rs `run_iptables`), and only the egress
proxy installs an ip6tables nat rule (fc-agent/src/proxy.rs `setup_ip6tables_redirect`). So
"routed, no published port" is too narrow: a routed guest with a `/loopback` port still gets
stale IPv6 entries. By source; and a routed clone attaches IPv6 tracking for good when its
restore builds the retired-address chain in ip6tables nat (fc-agent/src/retired_address.rs),
which the model does not hold.

### `halfOpenRetransmit`, `reusedTuple`: the same in a guest that always tracks (new, on #2106)

`ctstate`. P3 `smuggledLate`. Run `nat`, 19 and 20 steps. Medium. Fixed by #2113
(`flags_keeps_halfOpenRetransmit`, `flags_keeps_reusedTuple`). The first is reported as a
comment on #2106.

```
client A sends a SYN; the listener answers and holds a request socket
snapshot create ... pause; a clone is restored
restore: verify, UDP rule, barrier, open_egress, load, SOCK_DESTROY
client A sends its SYN again          => the entry (SYN_RECV) ignores it: not NEW, it passes
=> a new request socket behind the clone's closed gate, after the retirement
```

A retired request socket sends no reset (`tcp_abort`, net/ipv4/tcp.c:5062-5069), so the
tracker's entry outlives it for its 60 s. It needs a client whose SYN-ACK was lost across the
snapshot.

`reusedTuple` is the same with an established connection, and needs nothing lost:

```
client A connects (SYN, ACK); snapshot create ... pause; a clone is restored
restore: verify, UDP rule, barrier, open_egress, load
SOCK_DESTROY                          => the guest resets client A's connection
client A connects again from the same address and port
=> its SYN is not NEW: it passes the clone's closed gate
```

A SYN on a closed entry is NEW only when the tracker recycles the entry, and it recycles one
only if a FIN was seen or the last packet was a reset from the SYN's own direction
(net/netfilter/nf_conntrack_proto_tcp.c:1036-1069). The retirement's reset is from the guest's
side, so the SYN is ignored and passes on the old entry, which has seen a reply. The window is
the entry's timeout: 10 s after the reset in 6.18.60 (`nf_conntrack_tcp_timeout_close`). In
7.3.3 a reset whose sequence does not match leaves the entry ESTABLISHED with the 300 s
unacknowledged timeout, where 6.18.60 gives it 10 s (the faithfulness review's comparison of
the two kernels; not read again here). Where: every family the guest always tracks: rootless,
or IPv4 with a `/loopback` port.

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
needs a listener in SYN-queue overflow (or `tcp_syncookies=2`) at the snapshot.

### `loopbackClient`, `loopbackClientA`: a `/loopback` port's client looks like the guest itself

`ctstate`, `flags`, `cookieA`. P3. Run `loopback-port`, 22 and 26 steps. High for such ports.
Fixed at 27add63dd (the review of a80e1ad34, P2).

```
client A connects to the port published with /loopback
snapshot create ... the manifest leaves the socket out: its peer is 127.0.0.1
a clone is restored and published
=> the clone holds the source's connection from client A
```

The nat INPUT rule that rewrites the client's source runs after the filter hook and before the
socket lookup, so the gate sees the client and the socket sees loopback. Under `cookieA` the
restore's own list skips a loopback peer the same way. Where: `--publish
HOST:GUEST/loopback`, rootless and routed.

### `twoDumps`, `oneDump`: the list is two dumps

`cookieA`, and `cookieB` without its second dump. P2. Run `pairs`, 21 steps. Low.

```
a clone is restored; its restore: cookies off, verify, UDP rule, barrier, load
its own list: the IPv4 dump
a process connects to a loopback listener from the guest's own address
its own list: the IPv6 dump           => the server's end is listed, the client's is not
open_egress; SOCK_DESTROY             => the server's end is destroyed, and its reset takes
                                         the client's
```

The review of a80e1ad34 said a connection made inside the guest between the two dumps could
lose an end. The model agrees, with two things the review did not have. At a80e1ad34 the end
that is listed must not record a loopback peer (those are kept by address), so what is left is
a client that binds to the guest's address and connects to a loopback listener of the other
family: real, and close to unreachable. And the end that is not destroyed does not survive:
`SOCK_DESTROY` is `tcp_abort`, which sends a reset, and the gate lets a reset through on `lo`
(`Limits.lean`), so the whole connection goes. `oneDump` is why 27add63dd, where the mirror
image alone decides, needs the second dump for every loopback connection.

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
the question). Low. Part of #2138.

```
a clone is restored; its restore writes 0 to tcp_syncookies
the next step fails: the restore fails closed, on its way to shutdown_vm
snapshot create of that clone, before the power-off: prepare, pause
=> the image holds tcp_syncookies at 0
```

Yes. `failedCloneSnapshotEnds` then follows a clone of that image: its only step is the
power-off, because the agent in the image had already left its restore.
`earlySnapshotCaptures` (20 steps) is the other way to the same record, with no failure: a
restore that begins between a prepare's exit and the pause.

### `proxyConnectA`, `proxyConnect`, `proxyStale`: a connect behind the closed egress half

`cookieA`, `cookieB`: P2, run `proxy`, 21 and 22 steps. Low. Documented at 27add63dd.
`ctstate`, `flags`: P3, run `proxy`, 22 steps. Low.

```
a clone is restored
a process outside the container connects out: nat OUTPUT redirects it to the proxy
the restore: cookies off, ..., its own list     => both sockets listed, neither a mirror image
open_egress; SOCK_DESTROY                       => both retired
```

Where: rootless only (the proxy), and only for a process the boundary does not freeze: a system
daemon, an `fcvm exec --vm` command.

`proxyStale` is the same connect on the source, between its dump and its pause, under `ctstate`
and `flags`, which take no list: the two sockets are in the image and in no manifest, and the
clone publishes holding them. It is P3 only by this model's wider reading of "a connection that
leaves the guest": the relay behind the proxy is vsock, the restore resets it by other means,
and the model does not hold that, so what happens to the two TCP sockets after publication is
not shown.

### `hostDies`: a dead `snapshot create` (#1936)

All four. P6. Run `host`, 9 steps. Known, open; #1941 is its fix.

## New breaks, in #2124 at its head

All are in code #2124 does not change and so are in `ctstate` and #2113 as well, except where
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

The conditions, from the faithfulness review of this model: the host checks neither health nor
`lifecycle_ready` before a create, and a restore holds no per-VM snapshot lock. Exec answers
only after the restore asks for the re-register, so the window is the restore's phases before
the boundary. An agent already waiting on the boundary lock gets it when the prepare exits,
ahead of the resume one-shot, so the 22-step run needs the whole create to fit before the agent
reaches the lock, or a prepare that fails (the 13-step run). Otherwise the outcome is an image
taken inside a restore. A routed clone without a published port is not reachable before its
address move.

What a clone of an image taken inside a restore does is modelled since the review: the agent's
memory is in the image, so its task goes on with the old restore from the step it was at and
the watcher then begins the new epoch's (`State.again`). Today's rule books only: the design
never has an unpublished clone in a snapshot.

### `agentDies`, `agentDiesSource`, `agentDiesVsock`: fc-agent dies between the prepare and the reopen (#2149)

All four. P6 and P1. Run `kills`, 4 steps; run `kills-vsock`, 6 steps. Medium: it needs
fc-agent to die (an OOM kill, a crash) inside a `snapshot create`. By source, not run in a VM.

```
snapshot create: the prepare one-shot freezes the container; close, IPv4 chains and hooks
fc-agent is killed; systemd kills the unit's cgroup: the one-shot and the container with it
the host runs the resume one-shot: no fc-agent answers the exec
=> snapshot create returns its error; the source keeps the gate's rules
```

Both one-shots are exec'd through the long-running agent's exec server
(src/commands/common.rs:6021). With the agent dead the reopen cannot be delivered. The host
makes one exec, which tries its connect 41 times over about 54 s (src/commands/exec.rs,
`MAX_EXEC_CONNECT_ATTEMPTS`), and then `snapshot create` returns its error. The unit restarts
the agent after `RestartSec=1`, once the old cgroup is gone, which a frozen container that does
not die on SIGTERM can hold up for systemd's stop timeout (90 s by default, by reading, not
observed). What the new agent can do decides the outcome:

- **A restored guest** (most guests) is served no boot plan. The new agent loops for ever
  before it starts an exec server. Gated for good.
- **A guest that booted without the cache, on Firecracker**, fetches its plan over MMDS, which
  the closed egress half answers with a reset. It loops the same way. Gated for good. (If the
  agent died before the prepare closed the IPv4 half, the fetch gets through and this guest
  takes the next leg.)
- **A guest that booted without the cache, on Cloud Hypervisor**, gets its plan over vsock
  behind the closed gate and starts its exec server. If that is inside the host's 54 s, the
  exec connects, the resume one-shot runs and the gate opens: nothing is broken. If the restart
  takes longer (the 90 s stop), the host has given up, and the guest runs on with an agent, a
  new container and a closed gate that nothing is going to open (`agentDiesVsock`).

This is the reopen that cannot be attempted; #2142 is the reopen that is attempted and fails.
The same holds for a kill at any later step of the create up to the reopen.

### `containerMidBoundary`: a container started inside a prepare, and captured running

All four. P4. Run `kills-vsock`, 13 steps. Low: Cloud Hypervisor only, a guest that booted
without the cache only, and a narrow window. By source, not run in a VM.

```
snapshot create: prepare (freeze, close, barrier, dump, save); the one-shot exits 0
fc-agent is killed; systemd kills the container with it
systemd restarts fc-agent: it gets its boot plan over vsock and serves exec again
the host pauses the VM: the image holds the new agent, its container not yet started
a clone is restored from the image
the clone's agent starts the container           => it runs behind the closed egress half
```

Nothing in `agent::run` reads the gate, and a provisioned guest's restarted agent starts the
existing container (`existing_container_id`). The host notices nothing: the prepare's exec and
the pause are adjacent calls (src/commands/common.rs:7225-7247 for Cloud Hypervisor), so the
agent must die after the prepare's reply and be back, through `RestartSec=1` at the least,
before the next call the host makes. With the container started in the source before the pause
instead, the image holds it running and every clone resumes it behind its closed gate. A second
boot path that fails a mount shuts the VM down instead of coming back.

The first version of this document said this was found only by a proof and had no recorded
run. That was for want of a Cloud Hypervisor configuration in the search; the three `vsock`
configurations find it on every one of today's rule books.

### What a killed fc-agent leaves in a clone (the first version's #2139, corrected)

The first version of this model had a restarted fc-agent run the restore again and recorded
three breaks from it (`restartCookies`, `restartEgress`, `restartRetires`). They cannot
happen. `agent::run` fetches its boot plan in a loop with no end before it starts the restore
watcher (fc-agent/src/agent.rs:74-86, 215), and neither backend serves a plan to a guest
restored from a memory image: Firecracker's MMDS store is replaced with a `latest` object that
has no `container-plan` (src/commands/common.rs:4456-4487), and the Cloud Hypervisor restore
document has none (src/commands/snapshot.rs:4309-4361). systemd has also killed the unit's
cgroup, the container in it, before the restart. So in every rule book:

- an agent killed inside a restore leaves the clone as it was, gated, until the host's
  `snapshot run` gives up waiting and ends it (15 minutes). No property is broken
  (`orphanWaits` in `Ends.lean` is the run under the design);
- an agent killed in a published clone leaves a guest with no exec, no container and no
  boundary held. Since most guests are restored guests, this is what an fc-agent crash does to
  most running VMs.

Not in the model: before its plan loop the new process writes /etc/resolv.conf from the boot
command line and adds the command line's IPv6 address (agent.rs:66-67,
`configure_ipv6_from_cmdline`). In a routed clone that is the source's address, which the
restore had moved the clone off. It is not P1 (nothing in the source's guest changes) and not
P5 as tested (no setting the boundary changed is left changed), but it undoes part of the
restore in a clone whose workload is already dead: new IPv6 connections from that guest can
take the source's address as theirs, and their replies are routed to the source. By source,
not run.

### `watchdogThaw`: the freeze watchdog thaws behind the closed gate (#2140)

All four. P4. Run `slow`, 11 steps. Medium.

```
snapshot create ... pause; a clone is restored (frozen, both halves closed)
the payload-freeze watchdog thaws the container
=> the container runs with egress closed
```

Conditions: about 60 s of time the guest runs, from the prepare's freeze to the clone's
`open_egress`. The effect on the workload (its connects are answered with the gate's reset)
needs a mode without the egress proxy: with it, a connect is redirected before the gate sees
it. The watchdog is a thread of the long-running agent and dies with it.

### `barrierKill`, `barrierKillCaptured`: `rcu_expedited` left on (#2141)

All four. P1, then P5 `capturedChange`. Run `kills`, 7 and 16 steps. Low. By source.

```
snapshot create: freeze, close, close; receive barrier: rcu_expedited written 1
the prepare one-shot is killed
the host runs the resume one-shot: gate opened, manifest removed, thaw
=> no boundary in progress, rcu_expedited still 1; the next snapshot's image carries it
```

The window is the barrier: 0.1 to 0.2 ms by the faithfulness review's reading.

### `reopenFails`, `reopenFailsStuck`: a failed reopen leaves the source gated (#2142)

All four. P1 and P6. Run `fails`, 11 steps. Medium. By source.

```
snapshot create ... pause, resume
the resume one-shot fails to open the gate; it removes the manifest, thaws, exits non-zero
=> snapshot create has returned; both halves of the source's gate are in; nothing will retry
```

The first version listed this under P6 while its test passed the state, because it counted the
user's next `snapshot create` as a step of the system. It no longer does (`Properties.own`).

### `writeBackFails`: published without SYN cookies after a failed write

`cookieA`, `cookieB`. P5. Run `fails`, 26 steps. Low. Documented in #2124, which keeps the
clone published with a warning; a later snapshot of it captures the 0. The design fails the
restore instead (`REPAIRS.md`, 7).

## Known, and not in the model

### A `/loopback` client that picks its source port (#2152)

`cookieB`, and so the design. P3. Found by the head review of #2124, by reading, not run.

A client that connects to a `/loopback` port P from source port P makes a server socket whose
own end and peer are both 127.0.0.1:P, which is its own mirror image; two such ports and two
clients make a pair. The mirror rule takes them for a connection inside the guest and every
clone keeps them. The model cannot make this run: a kind fixes its tuple, and no run chooses a
client's source port. `design_noStaleAtPublication` is therefore about clients that do not
(`Limits.lean`; `README.md`, "Assumed"). The fix (a reserved source port range for the rewrite)
is in flight with the others.

## Fixes in flight

None of the repairs in `REPAIRS.md` is in this pull request. As of this writing:

| Break | Issue | Fix |
|---|---|---|
| `earlySnapshot`, `earlySnapshotFails`, `earlySnapshotCaptures`, `failedCloneSnapshot` | #2138 | #2151: on the host, `snapshot create` refuses a clone whose restore is not acknowledged (repair 2) |
| `watchdogThaw` | #2140 | a pull request stacked on #2124 by its author, built to the two rulings of `REPAIRS.md`, 3 and 8 |
| `barrierKill`, `barrierKillCaptured` | #2141 | the same |
| `reopenFails`, `reopenFailsStuck` | #2142 | the same |
| `agentDies`, `agentDiesSource`, `agentDiesVsock`, `containerMidBoundary` | #2149 | the same |
| a `/loopback` client that picks its source port | #2152 | the same |
| `hostDies` | #1936 | #1941 |

The rule book of each fix is not written yet: when one lands, its commit gets a rule book in
`Today.lean` and the search says which of these runs it closes.

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
  can arrive at all is the host side's matter and is not in the model.
- **What it does not open**: a datagram on a flow a client began is in the original direction
  of its entry, stale or new, and is rejected once the rule is in. `design_udpRuleHolds` is
  that as a theorem: a client's datagram reaches a clone of the design that still has an
  ingress half only before the rule is in, or in a clone whose restore has failed.
- **Before the rule is in** nothing in the guest stops a datagram, a client's or a reply
  (`clientDatagram`, run `udp-reach`, 11 steps; #1114), in any rule book, the design included.
  The host does: its filter on the clone's TAP for published ports, and a routed clone's
  address. That is assumed (`Cfg.udpReach = false`), not proved, and not in the model.
- **Not in the model**: the entries each restore leaves in a guest that otherwise tracks
  nothing, which is the rest of #2112.

## Everything at once

`all-routed` and `all-proxy` turn on failures, kills, a dying host, slow boundaries, early
snapshots, cookies, connections inside the guest and outbound connects together, with two
socket cookies and the boot plan over MMDS. They run only when named, and CI does not run
them.

| | `all-routed` | `all-proxy` |
|---|---|---|
| `ctstate` | capped at 10,000,000 states | 2,914,335 states, complete |
| `flags` | 7,214,191 states, complete | 2,516,499, complete |
| `cookieA` | capped | 5,098,605, complete |
| `cookieB` | capped | 4,984,357, complete |
| `cookieB` without its second dump | capped | 4,741,741, complete |
| design | 1,971,774 states, complete, no break | 521,538, complete, no break |

One run each, on a shared dev box: 7 min 50 s and 2.5 GB for today's five rule books, 15 s and 0.5 GB for
the design. A capped search has looked at the first ten million states in breadth-first order
and says nothing of the rest.

In both configurations each of today's rule books breaks only properties it breaks in the
table above: none breaks one the table calls `holds`. In particular `cookieB` has no
`smuggledLate` in `all-proxy` (complete) and none in the first ten million states of
`all-routed`. The first version of this model had one, at 32 steps: it went through an fc-agent
restarted into a second restore, which cannot happen.
