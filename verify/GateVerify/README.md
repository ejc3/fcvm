# GateVerify: the snapshot and restore network boundary, in Lean

A Lean 4 model of the boundary fc-agent keeps around a memory snapshot: the gate's rules, the
receive barrier, the manifest of socket cookies, the restore's retirement of the snapshot's
sockets, and publication. It has one rule book for each state of the code (`Today.lean`, every
rule citing file and line at a named commit) and one for the repaired system (`Design.lean`). It
is the second model in this directory and follows the first (`../README.md`): plain data, one
function a step, properties as decidable tests, an exhaustive search against the code as it is,
proofs for the design.

It was written because this boundary drew a new defect in almost every review round of #2113
and #2124, to find the ones no review had drawn yet.

- [`BREAKS.md`](BREAKS.md): every break the search found, by rule book, with its run, what it is
  in the real system, where it can happen, and a severity.
- [`REPAIRS.md`](REPAIRS.md): what each break needs changed, where, and the wording proposed.

## Build

```sh
make verify              # cd verify && lake build: both models, every proof and recorded break
make verify-gate-search  # lake exe gate-breaks: the exhaustive search, every rule book
make verify-gate-drift   # lake exe gate-seq check: the model's order against the Rust source
```

`make verify-gate-search GATE_ARGS=traces` prints each shortest run, step by step and as a Lean
term. A run's name or a rule book's name in `GATE_ARGS` limits the search to it.

Measured on a shared arm64 dev box (96 cores, load average 24 to 30 during the two builds and
4 to 10 during the searches, one run each), toolchain already installed. Peak memory is the
largest single process:

| Command | Wall time | Peak memory |
|---|---|---|
| `lake build` from cold, both models | 1 min 28 s | 1.4 GB |
| `lake build GateVerify gate-breaks gate-seq` from cold, this model alone | 24 s | 1.4 GB |
| `lake exe gate-breaks` (18 runs, 6 rule books, 1.62 million states in all) | 5.0 s | 68 MB |
| `lake exe gate-breaks design all-routed all-proxy` (696,354 states) | 4.2 s | 138 MB |

## The files

| File | What it holds |
|---|---|
| `Spec.lean` | `State` (one guest: its sockets, tracked connections, rules, settings, files under /run, processes), the kernel's handling of a segment and of a datagram, the guest's own processes, `snapshot create` and the restore one step at a time with each step able to fail or its process to be killed, `step`, `replay`, `Reachable`. `Rules` is a rule book's type; `Cfg` is what exists around the guest and which moves a search allows |
| `Today.lean` | `Today.main`, `Today.flags`, `Today.cookieA`, `Today.cookieB`: four commits, each field cited. `Today.cookieBOneLook`: the head of #2124 without its second dump |
| `Design.lean` | `Design.rules`: the head of #2124 with six repairs |
| `Properties.lean` | The properties, as tests on a state |
| `Configs.lean` | The configurations the search runs, by name |
| `Breaks.lean` | Each run the search found, replayed and checked by the kernel (`by decide`), and the same run under the design |
| `Lemmas.lean` | The invariants `Scal` (settings, rules and phases) and `Flags` (the records steps leave), and that every step of the design keeps them |
| `Socks.lean` | The invariant `Sk` over the sockets, the dump and the lists of cookies; `Evo`, what a step of the environment can do to the socket list; and that every step of the design keeps `Sk` |
| `Gens.lean` | The invariant `Gn` over the generations (which sockets the manifest and the list can name, and that a connection inside the guest keeps both ends); `Evo2`, what a step of the environment can do to such connections; and that every step of the design keeps `Gn` |
| `Ends.lean` | `measure`, the number every step of the system makes smaller, and the P6 theorems built on it: `design_bounded`, `design_ends`, `design_settles`, `kill_restart_repeats` |
| `Limits.lean` | What the proof of P2 does not cover: a connection inside the guest with one end freed, added by hand to a run of the design |
| `Proofs.lean` | The `design_*` theorems |
| `Axioms.lean` | Fails the build if any theorem under `Gate` rests on more than `propext`, `Quot.sound`, `Classical.choice`, and pins the axioms of the cited ones |
| `../GateSearch.lean` | `lake exe gate-breaks`: breadth-first search, shortest runs |
| `../GateSeq.lean` | `lake exe gate-seq`: the rule books' restore order against the order fc-agent's tests pin |

## The requirements, in words

From `.claude/CLAUDE.md` ("SUSPENDING A VM DOES NOT DISTURB IT", "On restore, the gate's egress
half opens as soon as the stale sockets are gone") and the issues.

1. **Source undisturbed.** `snapshot create` reads its source. While it runs, no established
   connection of the source is reset. When it is over, with or without an error, the source has
   no half of the gate, no manifest and no setting changed.
2. **Fixed generation.** A restore destroys the snapshot's sockets, by their kernel cookie and
   never by their four-tuple, and what a packet from outside made behind the closed gate. It
   never destroys a socket a process of the clone opened, nor one end of a connection inside
   the guest whose other end lives on.
3. **Nothing stale or smuggled at publication.** When a clone's ingress opens, the clone holds
   no TCP socket whose other end is outside the guest and that was in the image or was made
   behind the closed gate, by any route: a manifest that missed it, a SYN cookie's ACK, a peer
   address a NAT rule rewrote, the other address family. And from the retirement to publication
   no packet from outside makes one.
4. **Egress before retirement, and the hook between.** Egress opens before the first socket is
   retired. The hook runs with egress open and ingress closed. While a clone's egress half is
   closed, nothing of the workload runs into it: the container is frozen and nothing has been
   retired.
5. **No lasting change to the clone.** Whatever the restore changes (`tcp_syncookies`, a rule)
   is as it was at boot once the clone is published, on every way out, and no snapshot captures
   a changed value.
6. **Never left closed.** No single failure leaves a gate closed for good: while a gate is
   closed, something of the system can still act, to open it or to report the guest failed.

And for UDP, which the gate handles with one conntrack rule (#2112): no datagram on a flow a
client began reaches a clone before it has published (U1).

## The properties

Each is a `Bool` test for "broken here" in `Properties.lean`; the property is that no reachable
state passes the test.

| | Test | What it reads |
|---|---|---|
| P1 | `sourceDisturbed` | the record `rst` (the gate reset an established connection of a guest being snapshotted); or no boundary in progress and a half of the gate, the manifest or `rcu_expedited` still there |
| P2 | `generationBroken` | the record `bad`: a retirement destroyed a socket a process of the clone opened, or an end of a connection inside the guest that the manifest does not name and whose other end is alive |
| P3 | `staleAtPublication` | a guest that was restored, ingress open in some family, and a socket with its other end outside that is `stale` (in the image) or `smuggled` (made by a packet from outside behind a clone's closed ingress half) |
| P3 | `smuggledLate` | the record `late`: such a socket was made after the clone's retirement |
| P4 | `retiredBehindEgress` | the record `eg`: a retirement ran with the egress half in |
| P4 | `hookMisplaced` | the record `hk`: the hook ran with egress closed or ingress open |
| P4 | `workloadBehindEgress` | a restore in progress, egress closed in some family, and the container running or sockets already retired |
| P5 | `lastingChange` | a published clone no snapshot holds, with `tcp_syncookies` not as at boot, the UDP rule, or the restore's record |
| P5 | `capturedChange` | the record `cap`: the VMM paused the guest with `tcp_syncookies` or `rcu_expedited` changed |
| P6 | `stuck` | a half of the gate in, or the guest paused, and none of the system's steps enabled (`progressLabels`: the snapshot's next step, the guard's, the restore's, the agent's restart, the power-off, the host's timeout) |
| U1 | `clientDatagram` | the record `udpIn` |

`replyDatagram` is not a property. It records a datagram on a flow the guest began reaching an
unpublished clone, which the UDP rule passes by design.

## Results

| | `main` 4adc1dee6 | `flags` a1e5fc687 (#2113) | `cookieA` a80e1ad34 | `cookieB` 27add63dd (#2124) | Design |
|---|---|---|---|---|---|
| P1 `sourceDisturbed` | broken | broken | broken | broken | **proved** |
| P2 `generationBroken` | broken | broken | broken | broken | **proved**, for connections inside the guest that keep both ends (`Limits.lean`) |
| P3 `staleAtPublication` | broken | broken | broken | broken (early snapshot only) | **proved** |
| P3 `smuggledLate` | broken | broken | broken | searched: holds | **proved** |
| P4 `retiredBehindEgress` | searched: holds | searched: holds | searched: holds | searched: holds | **proved** |
| P4 `hookMisplaced` | searched: holds | searched: holds | searched: holds | searched: holds | **proved** |
| P4 `workloadBehindEgress` | broken | broken | broken | broken | **proved** |
| P5 `lastingChange` | searched: holds | searched: holds | broken | broken | **proved** |
| P5 `capturedChange` | broken | broken | broken | broken | **proved** |
| P6 `stuck` | broken | broken | broken | broken | **proved**, and that every boundary ends (`Ends.lean`) |
| U1 `clientDatagram` | holds with the host's gate | the same | the same | the same | **proved**, with the host's gate |

"broken" is a run in `Breaks.lean`, or one of the same kind (`BREAKS.md` has which). Most of
what breaks `main` and #2113 beyond the known issues breaks the head of #2124 too, because it
is in code #2124 does not touch: the one-shots, the watchdog, the agent's restart.

## What is proved, what is searched, what is assumed

**Proved** (Lean's kernel, no `sorry`, axioms `propext`, `Quot.sound` and, for the socket
invariant, `Classical.choice`), for every configuration `C` and every state reachable from
`init C` under `Design.rules`, by runs of any length:

- `design_sourceUndisturbed` (P1), `design_fixedGeneration` and `design_retiresOnly` (P2),
  `design_noStaleAtPublication` and `design_noSmuggledLate` (P3),
  `design_egressBeforeRetirement`, `design_hookPlaced`, `design_workloadEgress` (P4),
  `design_noLastingChange`, `design_noCapturedChange` (P5), `design_neverStuck`,
  `design_bounded`, `design_ends` and `design_settles` (P6), and `design_noClientDatagram`
  (U1), which carries the hypothesis `C.udpReach = false`.
- The proofs are four inductive invariants over the same `step` the search runs: `Scal`
  (`reachable_scal`: for each place a snapshot or a restore can be, what the gate,
  `tcp_syncookies`, `rcu_expedited`, the freeze, the manifest and the record are there),
  `Flags` (`reachable_flags`: no step has left a record of a break) and `Sk` (`reachable_sk`:
  after the IPv4 dump every IPv4 socket that must go has been seen; after the IPv6 dump every
  socket that must go has its cookie in the manifest or the list; after the retirement none is
  left; and no packet makes another in between) and `Gn` (`reachable_gn`: a socket the manifest
  names is the image's; a socket still on the restore's list after its second look was made by
  a packet behind the closed gate or was in the image, and has no mirror image; a connection
  inside the guest that a process of the clone opened has both its ends).
- `design_neverStuck` is progress: a step of the system is always enabled while a gate is
  closed. `Ends.lean` adds the end, in three parts (below, "P6: every boundary ends").
- "Every configuration" is every value of `Cfg`: with and without nat rules, the proxy, a
  `/loopback` port, cookies, failures of any step, kills of the agent and of a one-shot at any
  step, a dying host, boundaries held past the freeze deadline, snapshots asked of a clone
  early, any number of restores, any number of sockets.
- Every run in `Breaks.lean`: replayed step by step and the test evaluated, by `decide`, under
  the rule book it breaks and under the design.

**P2 is proved for the connections `step` can make, and one kind it cannot make breaks it.**
`design_fixedGeneration` says no retirement destroys a socket a process of the clone opened, a
socket a packet made with the gate open, or an end of a connection inside the guest whose other
end stays. `design_retiresOnly` says the same without the record `bad`: at the retirement,
every socket whose cookie is in the manifest or on the restore's list was in the image or was
made by a packet behind the closed gate, and one the manifest does not name has no mirror image.
With `Sk` (everything that must go is named) the retirement destroys exactly the generation it
should.

The proof uses that a connection inside the guest gets both its sockets in one step and loses
both in one step (`pair`, `unpair`; clause `gP` of `Gn`). The kernel can hold one end alone:
after the other end closed and its TIME-WAIT socket expired (60 s by default), or while a SYN
to a loopback listener with a full accept queue goes unanswered. `Limits.lean` adds that by
hand to a run of the design. If a process outside the container has such a lone end, made
after the clone resumed, when the restore takes its list, the restore lists it, its second look
finds no mirror image, and the retirement destroys it (`loneEndRetired`): the process reads an
error on a connection of its own. The same end lost in the source before the snapshot is in the
manifest and every clone retires it, which P2 as written allows and snapshot_network.rs says
of `manifest_sockets` (`loneEndInImage`). The first needs a clone that ran for the FIN timeout
before its restore took its list, or a loopback listener with a full accept queue at that
moment. It is not in `step`, so no theorem above is about it. Whether it is a break to repair or a cost to accept has not been decided; `step`
gets the lone end when it has.

**Searched exhaustively, not proved**: that the rule books of today have no break other than
those listed, in the bounded configurations (18 runs, and for the design the two `all-` runs
that turn everything on at once: 568,336 and 128,018 states). The bounds are 2 to 5 socket
cookies and one or two restores. The search program is compiled Lean and is not itself
verified.

### P6: every boundary ends

`measure` (`Ends.lean`) reads a number off a state, at most 34: the steps a `snapshot create`
has left (at most 12), the steps a restore has left (at most 21: its 18 steps, or a failure and
the power-off, or with the agent dead the restart first), and 1 while the host waits for the
clone's acknowledgement.

- `own_decreases`: every step of the system makes the measure smaller, in any state, reachable
  or not. The system's steps are the steps of a `snapshot create` that has begun, the guard's,
  the restore's, either of them failing, systemd's restart of the agent, a failed clone's
  power-off, and the host's timeout.
- `step_account`, `calm_keeps`: a step of the environment that begins nothing leaves the
  measure where it was or lower (packets, datagrams, processes connecting and closing, the
  watchdog, a dying host, a killed one-shot). Three moves raise it: the user's next
  `snapshot create` (by 12), a clone restored from the image (to at most 21), fc-agent killed
  (by at most 1, in reachable states).
- `design_bounded`: so in any run from a reachable state, the steps of the system number at
  most the measure at its start, plus 1 for each kill, 12 for each `snapshot create` begun and
  21 for each clone restored. With none of the three, at most 34 (`design_ends`); one restore
  of a clone, at most 21 (`design_restore_ends`).
- `design_settles`: a reachable state in which no step moves a boundary on (the host's timeout
  and the failures not counted) is settled: powered off, or running with no snapshot and no
  restore under way, the agent alive and no half of the gate installed. A clone at that point
  has published or has been powered off.
- `kill_restart_repeats`: with no bound on kills there is no bound on steps. A kill before the
  restore's first step has landed (no record on disk yet) is followed by a restart that begins
  the restore again, and for every `k` there is a run of `k` kills holding `k` steps of the
  system that ends where it began. With at most `k` kills the bound is 34 + k. A kill after the
  first step is not of this kind: the restart finds the record and fails the restore, two steps
  from its end.

What these do not say: that a step which stays enabled is taken. That the guest's scheduler
runs the agent, that systemd restarts it, and that the host's `snapshot create` runs on, is
assumed. The bound counts steps of the model, not time: one step of the restore can wait on
the kernel (the receive barrier, a `SOCK_DESTROY` acknowledgement) for as long as the code's
own budgets allow.

**The hypothesis `design_noClientDatagram` carries: `C.udpReach = false`.** A clone's UDP rule
goes in at the restore's fourth step. Before that nothing in the guest stops a client's
datagram, in any rule book, the design included (`clientDatagram` and `design_too_clientDatagram`
in `Breaks.lean`). What stops it is outside the guest: the host's filter on the clone's TAP for
published UDP ports until the restore is acknowledged (src/network/udp_gate.rs, used by
src/network/pasta.rs and bridged.rs), and for a routed clone that the network reaches it only at
the address it takes after the UDP rule (`move_to_clone_address`, fc-agent/src/restore.rs). The
model takes both as given; neither is in it.

**Assumed** (each abstraction, and whether it can hide a break):

| Assumption | Conservative? |
|---|---|
| One listener a port, one socket a client and port, at most two clients, one outbound connection a process | Bounds for the search. The proofs do not use them |
| A dump of one family is one instant; the list is two instants (IPv4, then IPv6) and a second look is a third | **Not conservative.** The kernel walks the hash table a bucket at a time (net/ipv4/tcp_diag.c:479-520), so a dump can also miss one of two sockets of the same family. The connections inside the guest in the model have their ends in different families, which gives the same split |
| Connection tracking is modelled for the inbound TCP tuples and two UDP flows only | **Not conservative** for the tracker's table as a whole: a tracker the gate attaches also tracks loopback and outbound flows. No property reads those entries |
| A tracked entry is `new`, `est` or `closed`. One-sided closes (FIN_WAIT, CLOSE_WAIT, LAST_ACK) are folded into `closed` | **Not conservative** for `main`: those states also let a SYN through as not NEW (`sIG`, nf_conntrack_proto_tcp.c:138). It hides runs of the #2106 kind on `main` only |
| A SYN cookie a client holds stays valid | Conservative: the real one expires (2 minutes of guest run time), which removes runs |
| The accept queue has room | Conservative for the sockets a client can make: a full one refuses the socket. **Not conservative** for P2: see the next row |
| A connection inside the guest gets both its sockets in one step and loses both in one step | **Not conservative** for P2. One end can be alone in the kernel (the other closed and its TIME-WAIT expired; a SYN unanswered at a full accept queue), and the proof of P2 uses that none is (`Limits.lean`, `loneEndRetired`) |
| TIME-WAIT sockets are not modelled; a close removes the socket | **Not conservative** for the manifest's TIME-WAIT handling (it names them, and the second look skips them). A reused tuple is modelled through a process that closes and reconnects |
| A packet with a loopback source cannot arrive on eth0 | Kernel: the source check refuses a local source address without `accept_local` (net/ipv4/fib_frontend.c:382-389), also with `route_localnet` on |
| A connect to the guest's own address that is not loopback is refused by the closed egress half | Read from the chain (`gate_chain_rules`: OUTPUT returns for a loopback destination only), as the PR says of itself. Not run. If it were let through, it would be another connection inside the guest made behind the gate, which the model has two kinds of |
| The gate's removal is one transaction for both families; its installation is one step a family | As the code: `remove_halves` is one nf_tables batch, `close` is `install_family` twice |
| A step of the agent is atomic | A kill lands between steps. Inside one step the code makes several writes only in `close` (chains, then two hooks) |
| The restore's steps that touch no network state are left out (clock, NFS remount, exec and proxy waits, GPUs) | They run between the retirement and the thaw and change nothing the properties read |
| The host's `snapshot run` ends a clone that does not acknowledge (`timeout`) | src/commands/snapshot.rs, 15 minutes. Without it P6 has no backstop on the clone's side |
| A restarted agent that must read its boot plan over MMDS cannot start behind a closed egress half (`Cfg.mmds`) | Read from fc-agent/src/agent.rs and bootplan.rs. Not run |

## The two things least sure to be faithful

1. **What a restarted fc-agent does.** The model has it run the restore again from the first
   step (`Rules.rerun`), stopped only by the MMDS fetch behind a closed egress half. That is read
   from `mmds.rs` and `agent.rs` (#2139); nobody has killed an fc-agent in a restoring clone and
   watched. Everything `agent::run` does before the watcher (the nat rules appended a second
   time, the containment rule inserted above the gate's hook, the proxy's listener bound again)
   is not in the model.
2. **Connection tracking.** Three states an entry, hooks attached "while a rule uses the
   tracker and a little longer" (`lag`), entries for the modelled inbound tuples only. The runs
   that reproduce #2106 depend on exactly these rules (when an entry is NEW, when a SYN is
   ignored, that a retired request socket sends no reset). The kernel functions are cited and
   were read at v6.18.60; none was driven by a harness as the first model's were. Some kernel
   profiles run 7.3.3 (rootfs-config.toml), which was not read.

## What is modelled

One guest at a time. A snapshot is taken of it, and at the pause the run either stays with the
source or moves to a clone restored from the image, which has the same sockets, tracked
connections, rules, settings and files under /run. A published clone is a source for the next
generation.

- **Sockets**: a kernel cookie as identity (unique, never reused, kept by the socket a request
  socket becomes), a kind (which fixes its tuple, the family that dumps it, whether its peer
  address is loopback, whether its other end is outside the guest, and its mirror image if it
  has one), a state (request socket or established), and how it came to be (`Gen`).
- **Connection tracking**: an entry a client tuple, present only while a rule uses the tracker
  (`tracked`), outliving the rule.
- **SYN cookies**: `tcp_syncookies`, and which clients hold a cookie.
- **The gate**: its two halves in each family, and the clone's UDP rule.
- **The receive barrier**, as the two writes of `rcu_expedited` around it.
- **The manifest**, the restore's own list (two dumps and a second look), the design's record.
- **Processes**: the container (frozen across the boundary), a process outside it, the
  long-running agent, the one-shots, the host's `snapshot create`, the payload-freeze watchdog.
- **Failure and kill** as transitions: any step of the agent or of a one-shot fails
  (`agentFail`, `snapFail`); the agent or a one-shot is killed between any two steps (`kill`,
  `snapKill`) and the agent restarted; the host dies (`hostDie`).

### The kernel's rules and their source (Linux v6.18.60)

| Rule in `Spec.lean` | Source |
|---|---|
| Hook order: tracking, nat PREROUTING and OUTPUT, filter, nat INPUT, confirm | include/uapi/linux/netfilter_ipv4.h:34-44; net/netfilter/nf_nat_proto.c:787-815 |
| `ctIn`: a new tuple is NEW; a segment that is not a SYN is picked up, still NEW | net/netfilter/nf_conntrack_proto_tcp.c:876-925 (`tcp_new`), 1638 (`tcp_loose = 1`); nf_conntrack_core.c:1868-1885 |
| `ctIn`: a SYN on an entry that has seen a reply is ignored and the entry stands | nf_conntrack_proto_tcp.c:138 (`sIG`), 1069-1125 |
| `ctIn`: a SYN on a closed entry deletes it and starts a new one | nf_conntrack_proto_tcp.c:1036-1067 |
| `--ctstate NEW`, `--ctdir ORIGINAL` | net/netfilter/xt_conntrack.c:167-197 |
| `tracked`: hooks attached while a rule uses the tracker | xt_conntrack.c:270, 279; net/netfilter/nf_conntrack_proto.c:585-627 |
| `rejected`: REJECT with tcp-reset answers and drops | net/ipv4/netfilter/ipt_REJECT.c:58-66; nf_reject_ipv4.c:271-340 |
| `pkt`: a request socket and an ACK make the established socket, with the request's cookie | net/ipv4/tcp_minisocks.c:689-941; inet_connection_sock.c:1263-1264 |
| `pkt`: a SYN makes a request socket, or a cookie when the queue is full and `tcp_syncookies` is not 0 | net/ipv4/tcp_input.c:7434-7467 |
| `pkt`: a cookie's ACK makes a socket when `tcp_syncookies` is not 0, in both families | net/ipv4/syncookies.c:405-423; tcp_ipv4.c:1876-1883; net/ipv6/syncookies.c |
| `pkt`: a listener answers any other ACK with a reset | net/ipv4/tcp_input.c:6965-6967; tcp_ipv4.c:1944-1975 |
| `Sock.id`: cookies are a counter | net/core/sock_diag.c:27-40 |
| Dumps show request sockets | net/ipv4/tcp_diag.c:320-327, 479-520 |
| `retire`: destroy by tuple and cookie; a request socket is dropped without a reset | net/ipv4/tcp_diag.c:570-600, 641-656; tcp.c:5058-5110 |
| `conn`: REDIRECT of a local packet is to 127.0.0.1, before filter OUTPUT | net/netfilter/nf_nat_redirect.c:56-60 |

### fc-agent's and the host's steps and their source

Cited in `Today.lean`, per commit. In short: `gate_chain_rules`, `udp_ingress_rule_args`,
`prepare_with`, `restore_boundary_with`, `publish_with`, `resume_source_network`,
`sockets_leaving_the_guest`, `manifest_sockets`, `unlisted_sockets`, `refuse_syn_cookies` and
`restore_syn_cookies` in fc-agent/src/snapshot_network.rs; `handle_clone_restore` in restore.rs;
`apply_restore_epoch` and `watch_restore_epoch` in mmds.rs; the one-shots in main.rs;
payload_freeze.rs; `loopback_client_snat_args` in network.rs; `setup_iptables_redirect_v4` in
proxy.rs; `create_snapshot_core` in src/commands/common.rs.

## The model against the code

`lake exe gate-seq check ../fc-agent/src/snapshot_network.rs` reads every sequence of events
that fc-agent's own model tests pin (`ModelGate`, `ModelBarrier`, `ModelDiag`, `ModelHook`: the
`vec![...]` literals of event names) and compares them with each rule book's restore order. It
prints the rule books the source pins and fails when there is none. CI runs it after the build,
and a pull request that changes that file starts the job.

| Source at | Rule books it pins |
|---|---|
| `main` 4adc1dee6, a1e5fc687 | `main`, `flags` |
| a80e1ad34, 27add63dd | `cookieA`, `cookieB`, `design` |
| `main` with `gate-udp-ingress` and `rx-barrier` swapped in one test | none: exit 1 |

On this branch the source is `main`'s, so the check reports `main` and `flags`. When #2113 and
#2124 land it will report the other three, and `lake exe gate-seq check <file> design` can then
be the CI line. The events do not show the steps the Rust models do not log (the thaw, the
manifest's removal, `tcp_syncookies` written back): the order of those is in `Today.lean` by
citation only, and `cookieA` and `cookieB` differ in exactly such a step.

## The invariant behind P2

`Sk` (`Socks.lean`) has the cookies below `next`, one kind a cookie, and where the sockets that
must go are. `Gn` (`Gens.lean`) adds, for a clone whose restore has completed `n` steps of the
design's 18:

- every cookie in the manifest and in the list is below `next`;
- a socket whose cookie the manifest names is `stale`; while ingress is closed none is `fresh`;
  while egress is closed the only `own` sockets are ends of connections inside the guest, and
  each such end has its mirror image, also `own`;
- after the IPv6 dump every listed cookie is a seen socket's; after the second look, a socket
  whose cookie is in the list is not `own`, not `fresh`, and has no mirror image among the
  sockets (and none can appear: a connection inside the guest is made only when neither of its
  kinds exists).

`Evo2` says three things of a step of the environment: a socket which goes takes its mirror
image with it, a new socket is nobody's mirror image unless both ends are new, and a process
connects out only with egress open (in the design the proxy's redirect is closed with the
egress half). The first is the abstraction `Limits.lean` is about.

## What is not modelled

- **More than one snapshot of a guest at a time, and the snapshot generation locks** on the host.
- **Routes, neighbours, addresses.** No step of any rule book here takes a link down or writes
  one of those tables (`prepare_with` and `restore_boundary_with` have no such call), so they are
  not in the state. The Rust tests that pin that are `ModelBarrier`'s link assertion and
  `snapshot_leaves_the_source_network_untouched`. A routed clone's move to its own address is
  left out with them.
- **ICMP and protocols other than TCP and UDP**, which the gate does not handle.
- **A workload's own network namespace or NAT rules.**
- **The relay behind the egress proxy** (vsock), which the restore resets by other means.
- **Time**: the payload freeze's deadline is a switch (`Cfg.slow`), a tracked entry never
  expires, a SYN cookie never expires.
- **The tracker's table as state the restore leaves behind** (#2112): each restore attaches the
  tracker through the UDP rule and leaves entries. The design does not change that.
- **The pending merge of `main` into #2124**, which reorders `handle_clone_restore` (#2088).
  `Today.cookieB` is 27add63dd.

## Conventions

As the first model's. Namespace `Gate`. A break found by the search is pasted into `Breaks.lean`
by name and stays a kernel-checked fact; the search's run names are the configuration names in
`Configs.lean`.
