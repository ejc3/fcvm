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

Nothing in it was run in a VM. The rules are read from the source at the commits named and from
Linux v6.18.60. Two independent reviews of its first version (207b5bc59) found it wrong about
what a restarted fc-agent does and blind to an fc-agent that dies in a source; this version has
their corrections, and `BREAKS.md` says which recorded breaks changed.

## Build

```sh
make verify              # check-imports.sh, then cd verify && lake build: both models
make verify-gate-search  # lake exe gate-breaks: the exhaustive search, every rule book
make verify-gate-drift   # lake exe gate-seq check: the model's order against the Rust source
```

`make verify-gate-search GATE_ARGS=traces` prints each shortest run, step by step, as a Lean
term, and as the same run under the design. A run's name or a rule book's name in `GATE_ARGS`
limits the search to it; `limit=N` caps a search at N states.

Measured on a shared arm64 dev box (96 cores, load average 4 to 25, one run each), toolchain
already installed. Peak memory is the largest single process:

| Command | Wall time | Peak memory |
|---|---|---|
| `lake build` from cold, both models | 1 min 27 s | 1.5 GB |
| `lake build GateVerify gate-breaks gate-seq` from cold, this model alone | 37 s | 1.5 GB |
| `lake exe gate-breaks` (17 runs, 6 rule books, 1,146,457 states in all) | 4.3 s | 98 MB |
| `lake exe gate-breaks design all-routed all-proxy` (2,347,140 states) | 15 s | 0.5 GB |
| `lake exe gate-breaks limit=10000000 all-routed all-proxy` for today's five rule books | 7 min 30 s | 2.5 GB |

CI builds both models in the one job, under its 15 minute cap (`../../.github/workflows/verify.yml`).

## The files

| File | What it holds |
|---|---|
| `Spec.lean` | `State` (one guest: its sockets, tracked connections, rules, settings, files under /run, processes), the kernel's handling of a segment and of a datagram, the guest's own processes, `snapshot create` and the restore one step at a time with each step able to fail or its process to be killed, `step`, `replay`, `Reachable`. `Rules` is a rule book's type; `Cfg` is what exists around the guest and which moves a search allows |
| `Today.lean` | `Today.main`, `Today.flags`, `Today.cookieA`, `Today.cookieB`: four commits, each field cited. `Today.cookieBOneLook`: the head of #2124 without its second dump |
| `Design.lean` | `Design.rules`: the head of #2124 with seven repairs |
| `Properties.lean` | The properties, as tests on a state |
| `Configs.lean` | The configurations the search runs, by name |
| `Breaks.lean` | Each recorded run, replayed and checked by the kernel (`by decide`), and what the design does with the same run. Generated from `lake exe gate-breaks traces` |
| `Lemmas.lean` | The invariants `Scal` (settings, rules and phases) and `Flags` (the records steps leave), and that every step of the design keeps them |
| `Socks.lean` | The invariant `Sk` over the sockets, the dump and the lists of cookies; `Evo`, what a step of the environment can do to the socket list |
| `Gens.lean` | The invariant `Gn` over the generations (which sockets the manifest and the list can name, and that a connection inside the guest keeps both ends); `Evo2` |
| `Ends.lean` | `measure`, the number every step of the system makes smaller, and the P6 theorems built on it |
| `Limits.lean` | Connections inside the guest: the reset that takes the other end, the manifest's second look, and one end left alone by the kernel, which the model does not make |
| `Proofs.lean` | The `design_*` theorems |
| `Axioms.lean` | Fails the build if anything a module of the library declares rests on more than `propext`, `Quot.sound`, `Classical.choice`; pins the axioms of the cited theorems |
| `../check-imports.sh` | Fails if a file of either model is not imported by its `Axioms.lean`. Outside lake, because lake does not elaborate `Axioms.lean` again when a file it does not import is added |
| `../GateSearch.lean` | `lake exe gate-breaks`: breadth-first search, shortest runs |
| `../GateSeq.lean` | `lake exe gate-seq`: the rule books' restore order against the order fc-agent's tests pin |

## The requirements, in words

From `.claude/CLAUDE.md` ("SUSPENDING A VM DOES NOT DISTURB IT", "On restore, the gate's egress
half opens as soon as the stale sockets are gone") and the issues.

1. **Source undisturbed.** `snapshot create` reads its source. While it runs, no established
   connection of the source is reset. When it is over, with or without an error, the source has
   no half of the gate, no manifest, no setting changed, and its container thawed.
2. **Fixed generation.** A restore destroys the snapshot's sockets, by their kernel cookie and
   never by their four-tuple, and what a packet from outside made behind the closed gate. It
   never destroys a socket a process of the clone opened, nor a loopback connection inside the
   guest that has both its ends. (Destroying one end of a connection inside the guest resets
   the other: `SOCK_DESTROY` sends a reset and the gate lets it through on `lo`. So "one end
   destroyed and the other left" is not a state the kernel leaves, and the requirement is
   about whole connections.)
3. **Nothing stale or smuggled at publication.** When a clone's ingress opens, the clone holds
   no TCP socket of a connection that leaves the guest, directly or through the egress proxy,
   that was in the image or was made behind the closed gate, by any route: a manifest that
   missed it, a SYN cookie's ACK, a peer address a NAT rule rewrote, the other address family.
   And from the retirement to publication no packet from outside makes one.
4. **Egress before retirement, and the hook between.** Egress opens before the first socket is
   retired. The hook runs with egress open and ingress closed. While a clone's egress half is
   closed, nothing of the workload runs into it: the container does not run and nothing has
   been retired.
5. **No lasting change to the clone.** Whatever the restore changes (`tcp_syncookies`, a rule)
   is as it was at boot once the clone is published, on every way out, and no snapshot captures
   a changed value.
6. **Never left closed.** No single failure leaves a gate closed for good: while a gate is
   closed, something of the system can still act, to open it or to end the guest and report
   it, and it does so within a bounded number of steps.

And for UDP, which the gate handles with one conntrack rule (#2112): no datagram on a flow a
client began reaches a clone before it has published (U1).

## The properties

Each is a `Bool` test for "broken here" in `Properties.lean`; the property is that no reachable
state passes the test.

| | Test | What it reads |
|---|---|---|
| P1 | `sourceDisturbed` | the record `rst` (the gate reset an established connection of a guest being snapshotted); or no boundary in progress and a half of the gate, the manifest, `rcu_expedited`, or a living container left frozen |
| P2 | `generationBroken` | the record `bad`: a retirement destroyed, by cookie or by the reset a destroyed socket sends its other end, a socket a process of the clone opened or one a packet made with the gate open; or it destroyed an end of a loopback connection that had both ends; or an end of any connection inside the guest that had both ends and that the manifest does not name |
| P3 | `staleAtPublication` | a guest that was restored, ingress open in some family, and a socket of a connection that leaves the guest (`inb`, `outb`, and the proxy's two) that is `stale` (in the image) or `smuggled` (made by a packet from outside behind a clone's closed ingress half) |
| P3 | `smuggledLate` | the record `late`: such a socket was made after the clone's retirement |
| P4 | `retiredBehindEgress` | the record `eg`: a retirement ran with the egress half in |
| P4 | `hookMisplaced` | the record `hk`: the hook ran with egress closed or ingress open |
| P4 | `workloadBehindEgress` | a restore in progress, egress closed in some family, and the container running or sockets already retired |
| P5 | `lastingChange` | a published clone no snapshot holds, with `tcp_syncookies` not as at boot, or the UDP rule |
| P5 | `capturedChange` | the record `cap`: the VMM paused the guest with `tcp_syncookies` or `rcu_expedited` changed |
| P6 | `stuck` | a half of the gate in, or the guest paused, and no step that moves a boundary on enabled (`moves`: the snapshot's next step once begun, the guard's, the restore's, the agent's restart, a failed clone's power-off, the host's timeout). The user's next `snapshot create` is not such a step |
| U1 | `clientDatagram` | the record `udpIn` |

`replyDatagram` is not a property. It records a datagram on a flow the guest began reaching an
unpublished clone, which the UDP rule passes by design.

P1 and P5 read a guest whose boundary is over. A reopen or a write-back that is tried again and
never lands would never be read by them. In the design each is tried `retryMax` times and then
the guest is ended (`REPAIRS.md`, 3 and 7), each failed try is a step of the model, and
`design_bounded` bounds the steps: so "the boundary is over" is reached, and P1 and P5 are
about every run and not only the ones that end by themselves.

## Results

| | `main` 4adc1dee6 | `flags` a1e5fc687 (#2113) | `cookieA` a80e1ad34 | `cookieB` 27add63dd (#2124) | Design |
|---|---|---|---|---|---|
| P1 `sourceDisturbed` | broken | broken | broken | broken | **proved** |
| P2 `generationBroken` | broken | broken | broken | broken | **proved** for the connections `step` makes (`Limits.lean`) |
| P3 `staleAtPublication` | broken | broken | broken | broken (by an early snapshot, in every run found) | **proved** |
| P3 `smuggledLate` | broken | broken | broken | searched: holds, one configuration capped | **proved** |
| P4 `retiredBehindEgress` | searched: holds | searched: holds | searched: holds | searched: holds | **proved** |
| P4 `hookMisplaced` | searched: holds | searched: holds | searched: holds | searched: holds | **proved** |
| P4 `workloadBehindEgress` | broken | broken | broken | broken | **proved** |
| P5 `lastingChange` | searched: holds | searched: holds | broken | broken | **proved** |
| P5 `capturedChange` | broken | broken | broken | broken | **proved** |
| P6 `stuck` | broken | broken | broken | broken | **proved**, and that every boundary ends (`Ends.lean`) |
| U1 `clientDatagram` | assumed: the host's gate | the same | the same | the same | assumed; the guest's part **proved** (`design_udpRuleHolds`) |

"broken" is a run in `Breaks.lean` (`BREAKS.md` has which). "searched: holds" is the 17 default
configurations searched to the end, and the two `all-` configurations as far as `BREAKS.md`,
"Everything at once", says: `all-proxy` to the end for every rule book, `all-routed` to the end
for `flags` and the design and capped at ten million states for the others. Most of what
breaks `main` and #2113 beyond the known issues breaks the head of #2124 too, because it is in
code #2124 does not touch: the one-shots, the watchdog, what a dead fc-agent leaves.

## What is proved, what is searched, what is assumed

**The universe.** "Every configuration" and "every reachable state" mean the states `Spec.lean`
can build. Unbounded: the number of socket cookies handed out, of restores, of snapshots one
after another, of packets, kills and failures. Bounded by the model's shape, in the proofs as
in the search:

- fourteen kinds of socket, at most one socket of a kind at a time: two clients (one IPv4, one
  IPv6), two ports (one plain, one `/loopback`), one request or established socket a client and
  port, one outbound connection a process for two processes (direct or through the proxy), one
  loopback connection inside the guest and one from the guest's own address;
- three tracked TCP tuples and two UDP flows;
- one `snapshot create` of a guest at a time, and one guest followed (at a pause the run stays
  with the source or moves to one clone).

The search bounds two more things (`Cfg.maxId`, an exclusive bound on cookies, so 1 to 4
sockets ever in a run and 2 in the `all-` runs; `Cfg.maxGens`, one or two restores). The proofs
do not.

**Proved** (Lean's kernel, no `sorry`, axioms `propext`, `Quot.sound` and, for the two socket
invariants, `Classical.choice`), for every value of `Cfg` and every state reachable from
`init C` under `Design.rules`, by runs of any length:

- `design_sourceUndisturbed` (P1), `design_fixedGeneration` and `design_retiresOnly` (P2),
  `design_noStaleAtPublication` and `design_noSmuggledLate` (P3),
  `design_egressBeforeRetirement`, `design_hookPlaced`, `design_workloadEgress` (P4),
  `design_noLastingChange`, `design_noCapturedChange` (P5), `design_neverStuck`,
  `design_bounded`, `design_ends`, `design_restore_ends`, `design_settles` (P6), and
  `design_udpRuleHolds` (the guest's part of U1).
- The proofs are four inductive invariants over the same `step` the search runs: `Scal`
  (`reachable_scal`: for each place a snapshot or a restore can be, what the gate,
  `tcp_syncookies`, `rcu_expedited`, the freeze, the manifest and the agent are there), `Flags`
  (`reachable_flags`: no step has left a record of a break), `Sk` (`reachable_sk`: after the
  IPv4 dump every IPv4 socket that must go has been seen; after the IPv6 dump every socket that
  must go has its cookie in the manifest or the list; after the retirement none is left; and no
  packet makes another in between) and `Gn` (`reachable_gn`: a socket the manifest names is the
  image's and is no end of a loopback connection that has both ends; a socket still on the
  restore's list after its second look was made by a packet behind the closed gate or was in
  the image, and has no mirror image; a connection inside the guest that a process of the clone
  opened has both its ends).
- Every run in `Breaks.lean` and in `Limits.lean`: replayed step by step and the test
  evaluated, by `decide`.
- Each repair, and each rule the design inherits from #2113 and #2124, is carried by a proof:
  with the field put back the proofs do not build (`REPAIRS.md`, the last table).

**P2 is proved for the connections `step` can make.** `design_fixedGeneration` says no
retirement destroys, by cookie or by a reset, what the test above names. `design_retiresOnly`
says it without the record: at the retirement every socket named was in the image or was made
by a packet behind the closed gate, one with a mirror image is named by the manifest and is no
loopback end, and no socket a process of the clone opened is reset. With `Sk` the retirement
destroys exactly the generation it should. `Limits.lean` has the three cases around it:

- a connection with one end on the guest's own address is named by one end and goes whole,
  because the reset takes the other (`mixedPairGoes`). A model without the reset shows the clone
  publishing with one end alone; the first version of this model did, and the proofs review
  found that run. It is an artefact of leaving the reset out;
- the manifest's second look is what keeps a loopback connection made between the source's two
  dumps (`manifestOneLook`), and `Gn.gML` is its obligation in the proof;
- one end left alone by the kernel with nothing destroyed (the other end closed and its
  TIME-WAIT expired; a SYN unanswered at a full accept queue) is not a step of the model. Such
  an end, made in a clone before its restore's list, is destroyed by the retirement
  (`loneEndRetired`). An accepted cost of the mirror rule; P2 is not proved for it.

**U1 is assumed, not proved.** A clone's UDP rule goes in at the restore's third step. Before
that nothing in the guest stops a client's datagram, in any rule book, the design included
(`clientDatagram` and `design_too_clientDatagram` in `Breaks.lean`). What stops it is outside
the guest: the host's filter on the clone's TAP for published UDP ports until the restore is
acknowledged (src/network/udp_gate.rs, used by src/network/pasta.rs and bridged.rs), and for a
routed clone that the network reaches it only at the address it takes after the UDP rule
(`move_to_clone_address`, fc-agent/src/restore.rs). The model takes that as `Cfg.udpReach =
false` and holds neither. With that switch the step that records a client's datagram is
disabled, so "no client datagram under `udpReach = false`" is true of every rule book and says
nothing of any of them; the first version listed it as a theorem of the design, and it is
withdrawn. What is proved is `design_udpRuleHolds`: a client's datagram reaches an unpublished
clone of the design only before the rule is in, or in a clone whose restore has failed and
that is on its way to the power-off.

**Searched exhaustively, not proved**: that the rule books of today have no break other than
those listed. The search program is compiled Lean and is not itself verified; the proofs
review rebuilt it and reproduced the first version's counts exactly.

### P6: every boundary ends

`design_neverStuck` is progress: while a gate is closed, a step that moves the boundary on is
enabled. It is not freedom from livelock. `Ends.lean` adds that, for the system's own steps.

`measure` reads a number off a state, at most 22 in a reachable one: the steps a `snapshot
create` has left (at most 20, its reopen's tries among them), the steps a restore has left (at
most 20: its 16 steps, its write-back's tries, a failure's power-off), 1 for the restart of a
dead agent in a guest that is served a boot plan, and 1 while the host waits for a clone's
acknowledgement.

- `own_decreases`: every step of the system makes the measure smaller, in every reachable
  state. The system's steps are the steps of a `snapshot create` that has begun, the guard's,
  the restore's, a failed try of either, systemd's restart of the agent, a failed clone's
  power-off, and the host's timeout.
- `step_account`, `calm_keeps`: a step of the environment that begins nothing leaves the
  measure where it was or lower (packets, datagrams, processes connecting and closing, the
  watchdog, a dying host, a killed one-shot, a killed agent in a clone). Three moves raise it:
  the user's next `snapshot create` (by 20), a clone restored from the image (to at most 21),
  and fc-agent killed in a guest that is served a boot plan (by 1).
- `design_bounded`: so in any run from a reachable state, the steps of the system number at
  most the measure at its start, plus 1 for each such kill, 20 for each `snapshot create` begun
  and 21 for each clone restored. With none of the three, at most 22 (`design_ends`).
- `design_restore_ends`: one restore of a clone is over within 21 steps of the system,
  whatever is killed in it. A guest restored from a memory image is served no boot plan, so its
  agent never comes back and no restore runs twice (`clone_never_restarts`); a kill in a clone
  adds nothing to the bound.
- `design_settles`: a reachable state in which no step moves a boundary on, the host's timeout
  not counted, is settled (powered off, or running with no snapshot and no restore under way
  and no half of the gate installed), or it is a clone that has not published and whose
  fc-agent is dead, for which the host's timeout is enabled. So the timeout is the way out for
  that clone and for nothing else.
- `kill_restart_repeats`: the kills the bound counts cannot be left out of it. In a cold-booted
  guest that no boundary holds, fc-agent can be killed and restarted any number of times, each
  restart a step of the system. Those restarts are recoveries (the agent comes back and starts
  a new container); none of them runs a restore.

What these do not say: that a step which stays enabled is taken. That the guest's scheduler
runs the agent, that systemd restarts it, and that the host's `snapshot create` runs on, is
assumed. The bound counts steps of the model, not time: one step of the restore can wait on
the kernel (the receive barrier, a `SOCK_DESTROY` acknowledgement) for as long as the code's
own budgets allow, and the host's timeout is 15 minutes.

### Assumed

Each abstraction, and whether it can hide a break:

| Assumption | Conservative? |
|---|---|
| A dump of one family is one instant; the list is two instants (IPv4, then IPv6) and a second look is a third | **Not conservative.** The kernel walks the hash table a bucket at a time (net/ipv4/tcp_diag.c:479-520), so a dump can also miss one of two sockets of the same family. The connections inside the guest in the model have their ends in different families, which gives the same split |
| Connection tracking is modelled for the inbound TCP tuples and two UDP flows only | **Not conservative** for the tracker's table as a whole: a tracker the gate attaches also tracks loopback and outbound flows, and on `main` a segment of a source's outbound connection can be reset the same way an inbound one is. No property reads those entries |
| A tracked entry is `new`, `est`, `closed` (a FIN seen) or `reset` (the guest's side reset it). One-sided closes (FIN_WAIT, CLOSE_WAIT, LAST_ACK) are folded into `closed`, and a `new` entry is both a SYN sent and a connection picked up in the middle | **Not conservative** for `main`: the one-sided states also let a SYN through as not NEW (`sIG`, nf_conntrack_proto_tcp.c:138), and a FIN on a `new` entry made by a SYN is invalid, not NEW. It hides runs of the #2106 kind on `main` only |
| An entry never expires | Conservative for the design; for `main` it makes `reusedTuple` and `strayAck` possible at any distance, where the kernel gives them 10 s (6.18.60) |
| A SYN cookie a client holds stays valid | Conservative: the real one expires (2 minutes of guest run time), which removes runs |
| The accept queue has room | Conservative for the sockets a client can make: a full one refuses the socket. **Not conservative** for P2: see the next row |
| A connection inside the guest gets both its sockets in one step and loses both in one step | **Not conservative** for P2. One end can be alone in the kernel, and the proof of P2 uses that none is (`Limits.lean`, `loneEndRetired`) |
| TIME-WAIT sockets are not modelled; a close removes the socket | The faithfulness review: this neither hides nor invents a break. The cookie is copied to the TIME-WAIT socket, `tcp_abort` removes it silently, and the gate rejects the SYN that would reuse its tuple |
| The egress proxy's two sockets are destroyed independently | Not checked. A reset from one reaches the other through the nat mapping; both are retired together in every recorded run |
| A packet with a loopback source cannot arrive on eth0 | Kernel: the source check refuses a local source address without `accept_local` (net/ipv4/fib_frontend.c:382-389), also with `route_localnet` on |
| A connect to the guest's own address that is not loopback is refused by the closed egress half | Read from the chain (`gate_chain_rules`: OUTPUT returns for a loopback destination only), as the PR says of itself. Not run. If it were let through, it would be another connection inside the guest made behind the gate, which the model has two kinds of |
| The gate's removal is one transaction for both families; its installation is one step a family | As the code: `remove_halves` is one nf_tables batch, `close` is `install_family` twice |
| A step of the agent is atomic | A kill lands between steps. Inside one step the code makes several writes only in `close` (chains, then two hooks) |
| The restore's steps that touch no network state are left out (clock, NFS remount, exec and proxy waits, GPUs) | They run between the retirement and the thaw and change nothing the properties read |
| The host's `snapshot run` ends a clone that does not acknowledge (`timeout`) | src/commands/snapshot.rs, 15 minutes. `design_settles` says exactly which states rest on it |
| systemd kills the unit's cgroup, the container and any one-shot, when fc-agent dies | From the unit file (no `KillMode`, `Delegate=yes`) and systemd's documented default. Not observed. The model keeps the dead container's sockets, which only adds sockets |
| A guest restored from a memory image is served no boot plan; a cold-booted one is, over MMDS only with the egress half open | Read from src/commands/common.rs, snapshot.rs, fc-agent/src/agent.rs and bootplan.rs. Not run. Firecracker's answer for the missing key and whether the fork persists its MMDS store were not checked |
| One `sp` counter a guest: a reopen cannot overtake a prepare that has not taken the boundary lock | **Not conservative.** The open finding on #1941 is that it can; the model cannot express it |

## The things least sure to be faithful

1. **What a dead and restarted fc-agent leaves.** The first version had it run the restore
   again; the faithfulness review showed it cannot. This version has the agent never come back
   in a restored guest, and come back in a cold-booted one unless MMDS is behind the closed
   egress half. All of it is read from the source and from systemd's documentation; nobody has
   killed an fc-agent in a guest and watched. What the new process does before its plan loop
   (resolv.conf, the boot command line's IPv6 address) and what a returning agent's boot path
   does to a guest a boundary still holds are not in the model.
2. **Connection tracking.** Four states an entry, hooks attached per family "while a rule uses
   the tracker and a little longer", entries for the modelled inbound tuples only, no timeouts.
   The runs that reproduce #2106 depend on exactly these rules. The kernel functions are cited
   and were read at v6.18.60; none was driven by a harness as the first model's were. The
   faithfulness review compared the cited functions in 7.3.3 (some kernel profiles run it) and
   found them the same in substance, with one timeout different (`BREAKS.md`, `reusedTuple`).
3. **A clone of an image taken inside a restore** (`State.again`): the old restore goes on and
   the watcher then begins the new epoch's. Whether the old restore's completion acknowledges
   the host was not read. Today's rule books with `earlySnap` only.

## What is modelled

One guest at a time. A snapshot is taken of it, and at the pause the run either stays with the
source or moves to a clone restored from the image, which has the same sockets, tracked
connections, rules, settings and files under /run, and the agent as the image has it. A
published clone is a source for the next generation.

- **Sockets**: a kernel cookie as identity (unique, never reused, kept by the socket a request
  socket becomes), a kind (which fixes its tuple, the family that dumps it, whether its peer
  address is loopback, whether its connection leaves the guest, and its mirror image if it has
  one), a state (request socket or established), and how it came to be (`Gen`).
- **Connection tracking**: an entry a client tuple, tracked per family only while a rule of
  that family uses the tracker (`tracked`), outliving the rule.
- **SYN cookies**: `tcp_syncookies`, and which clients hold a cookie.
- **The gate**: its two halves in each family, and the clone's UDP rule.
- **The receive barrier**, as the two writes of `rcu_expedited` around it.
- **The manifest**, the restore's own list (two dumps and a second look).
- **The retirement**: destruction by cookie, and the reset an established socket sends its
  other end inside the guest.
- **Processes**: the container (frozen across the boundary, killed with the agent), a process
  outside it, the long-running agent and whether a boot plan is served to it, the one-shots
  (exec'd through the agent), the host's `snapshot create`, the payload-freeze watchdog.
- **Failure and kill** as transitions: any step of the agent or of a one-shot fails
  (`agentFail`, `snapFail`); the agent is killed between any two steps, in a source or in a
  clone (`kill`), or a one-shot alone (`snapKill`); systemd restarts the agent (`restart`);
  the host dies (`hostDie`). In the design a reopen and a write-back are tried again, each
  failed try a step.

### The kernel's rules and their source (Linux v6.18.60)

| Rule in `Spec.lean` | Source |
|---|---|
| Hook order: tracking, nat PREROUTING and OUTPUT, filter, nat INPUT, confirm | include/uapi/linux/netfilter_ipv4.h:34-44; net/netfilter/nf_nat_proto.c:787-815 |
| `ctIn`: a new tuple is NEW; a segment that is not a SYN is picked up, still NEW | net/netfilter/nf_conntrack_proto_tcp.c:876-925 (`tcp_new`), 1638 (`tcp_loose = 1`); nf_conntrack_core.c:1868-1885 |
| `ctIn`: a SYN on an entry that has seen a reply is ignored and the entry stands | nf_conntrack_proto_tcp.c:138 (`sIG`), 1069-1125 |
| `ctIn`: a FIN with no entry is invalid: no entry, not NEW | nf_conntrack_proto_tcp.c:169 (the `fin` row from `sNO`) |
| `ctIn`: a SYN on an entry closed with a FIN seen deletes it and starts a new one; on one the guest's side reset, it is ignored and is not NEW | nf_conntrack_proto_tcp.c:1036-1069 (recycled only with `IP_CT_TCP_FLAG_CLOSE_INIT` or a last reset from the SYN's own direction), 1211-1261 |
| `--ctstate NEW`, `--ctdir ORIGINAL` | net/netfilter/xt_conntrack.c:167-197 |
| `tracked`: a family's hooks attached while a rule of that family uses the tracker | xt_conntrack.c:266-280; net/netfilter/nf_conntrack_proto.c:441-470 (`users4`, `users6`), 585-627 |
| `rejected`: REJECT with tcp-reset answers and drops | net/ipv4/netfilter/ipt_REJECT.c:58-66; nf_reject_ipv4.c:271-340 |
| `pkt`: a request socket and an ACK make the established socket, with the request's cookie | net/ipv4/tcp_minisocks.c:689-941; inet_connection_sock.c:1263-1264 |
| `pkt`: a SYN makes a request socket, or a cookie when the queue is full and `tcp_syncookies` is not 0 | net/ipv4/tcp_input.c:7434-7467 |
| `pkt`: a cookie's ACK makes a socket when `tcp_syncookies` is not 0, in both families | net/ipv4/syncookies.c:405-423; tcp_ipv4.c:1876-1883; net/ipv6/syncookies.c |
| `pkt`: a listener answers any other ACK with a reset | net/ipv4/tcp_input.c:6965-6967; tcp_ipv4.c:1944-1975 |
| `Sock.id`: cookies are a counter | net/core/sock_diag.c:27-40 |
| Dumps show request sockets | net/ipv4/tcp_diag.c:320-327, 479-520 |
| `retire`: destroy by tuple and cookie; a request socket is dropped without a reset; an established socket sends one, which reaches the other end inside the guest over `lo` (`resetBy`) | net/ipv4/tcp_diag.c:570-600, 641-656; tcp.c:5058-5110; `gate_chain_rules` for `lo` |
| `conn`: REDIRECT of a local packet is to 127.0.0.1, before filter OUTPUT | net/netfilter/nf_nat_redirect.c:56-60 |

### fc-agent's and the host's steps and their source

Cited in `Today.lean`, per commit. In short: `gate_chain_rules`, `udp_ingress_rule_args`,
`prepare_with`, `restore_boundary_with`, `publish_with`, `resume_source_network`,
`sockets_leaving_the_guest`, `manifest_sockets`, `unlisted_sockets`, `refuse_syn_cookies` and
`restore_syn_cookies` in fc-agent/src/snapshot_network.rs; `handle_clone_restore` in restore.rs;
`apply_restore_epoch` and `watch_restore_epoch` in mmds.rs; the one-shots in main.rs;
payload_freeze.rs; `loopback_client_snat_args` in network.rs; `setup_iptables_redirect_v4` and
`setup_ip6tables_redirect` in proxy.rs; `agent::run` in agent.rs and `fetch_plan` in bootplan.rs
(what a restarted agent does); `create_snapshot_core` and the restore's MMDS document in
src/commands/common.rs; the unit file in src/setup/rootfs.rs.

## The model against the code

`lake exe gate-seq check ../fc-agent/src/snapshot_network.rs <rule book>` reads every sequence
of events that fc-agent's own model tests pin (`ModelGate`, `ModelBarrier`, `ModelDiag`,
`ModelHook`: the `vec![...]` literals of event names) and compares them with each rule book's
restore order. It prints the rule books the source pins and fails unless the one named is among
them. CI names `main`, the rule book of this tree, so a tree that goes to an order the model
does not have fails, and so does one that goes back to an earlier order once the name has been
moved on. A pull request that changes that file starts the job.

| Source at | Rule books it pins |
|---|---|
| `main` 4adc1dee6, a1e5fc687 | `main`, `flags` |
| a80e1ad34 | `cookieA` |
| 27add63dd | `cookieB`, `design` |
| 27add63dd with the second `cookie-dump` deleted from the one test that pins two | `cookieA`: naming `cookieB` exits 1 |
| `main` with `gate-udp-ingress` and `rx-barrier` swapped in one test | none: exit 1 |

The pull request that lands #2113 or #2124 moves the name in `verify.yml` and in the Makefile's
`GATE_BOOK`.

What the check cannot see:

- anything but the tests' `vec![...]` literals. It reads no production code, so tests that pin
  an order the code no longer has are outside it;
- the steps the Rust models log no event for: the manifest's load and removal, the thaw, the
  write of `tcp_syncookies` back, the barrier as two writes. `cookieA` and `cookieB` differ in
  the place of the write-back and are told apart only by the second dump. The order of those
  steps is in `Today.lean` by citation only;
- the gate's rules, the manifest's and the list's rules, which are fields of a rule book and
  not an order;
- every file but snapshot_network.rs: `handle_clone_restore` in restore.rs, the host's
  `snapshot create`, the agent's start, the unit file.

## The invariants behind P2

`Sk` (`Socks.lean`) has the cookies below `next`, one kind a cookie, and where the sockets that
must go are. `Gn` (`Gens.lean`) adds, for a clone whose restore has completed `n` steps of the
design's 16:

- every cookie in the manifest and in the list is below `next`;
- a socket whose cookie the manifest names is `stale`, and is no end of a loopback connection
  that has both ends (what the manifest's second look gives); while ingress is closed no socket
  is `fresh`; while egress is closed the only `own` sockets are ends of connections inside the
  guest, and each such end has its mirror image, also `own`; no two sockets of one kind differ
  in how they came to be;
- after the IPv6 dump every listed cookie is a seen socket's; after the second look, a socket
  whose cookie is in the list is not `own`, not `fresh`, and has no mirror image among the
  sockets.

`Evo2` says of a step of the environment: a new socket is of a kind no socket has and is
nobody's mirror image unless both ends are new, a socket which goes takes its mirror image with
it, and a process connects out only with egress open (in the design the proxy's redirect is
closed with the egress half). The second is the abstraction `Limits.lean` is about.

## What is not modelled

- **More than one snapshot of a guest at a time, and the snapshot generation locks** on the
  host. A reopen that overtakes a prepare which has not taken the boundary lock (#1941's open
  finding) cannot be expressed.
- **Routes, neighbours, addresses.** No step of any rule book here takes a link down or writes
  one of those tables (`prepare_with` and `restore_boundary_with` have no such call), so they are
  not in the state. A routed clone's move to its own address is left out with them, and so is
  the address a restarted fc-agent adds from the boot command line.
- **ICMP and protocols other than TCP and UDP**, which the gate does not handle.
- **A workload's own network namespace or NAT rules.**
- **The relay behind the egress proxy** (vsock), which the restore resets by other means.
- **Time**: the payload freeze's deadline is a switch (`Cfg.slow`), a tracked entry never
  expires, a SYN cookie never expires, a step that is tried again is tried three times.
- **The tracker's table as state the restore leaves behind** (#2112): each restore attaches the
  tracker through the UDP rule and leaves entries, and a routed restore attaches IPv6 tracking
  for good through the retired-address chain. The design does not change that.
- **What a returning fc-agent's boot path does** to the rules of a guest a boundary holds (the
  nat rules appended a second time, the containment rule).
- **The pending merge of `main` into #2124**, which reorders `handle_clone_restore` (#2088).
  `Today.cookieB` is 27add63dd.

## Conventions

As the first model's. Namespace `Gate`. A break found by the search is pasted into `Breaks.lean`
by name (`../gen-breaks.py` writes the file from the trace output) and stays a kernel-checked
fact; the search's run names are the configuration names in `Configs.lean`.
