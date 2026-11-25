# What the breaks need

`Design.rules` (`Design.lean`) is PR #2124 at its head, 27add63dd, with the changes below. Each
is one field of the rule book. For each: what it is in the real system, where, the break it
answers (`BREAKS.md`), what the model shows of it, and wording proposed for the code or its
documents.

With all of them the search finds no break of P1 to P6 in any configuration (`lake exe
gate-breaks design`, and the two `all-` runs), and P1 to P6 are proved (`Proofs.lean`,
`Ends.lean`), P2 for the connections the model makes (`Limits.lean`). U1 is not among them:
`lake exe gate-breaks design udp-reach` reports `clientDatagram` for the design as for every
rule book, and what keeps a client's datagram from a clone before its UDP rule is the host's,
assumed.

Each repair is carried by a proof: with its field put back, `lake build GateVerify.Proofs`
fails (the table at the end).

None of this is implemented. The fixes #2113 and #2124 already make are not repeated here: the
flag rule (`tcpByCt := false`), the restore's own list with the mirror image deciding and a
second dump, `tcp_syncookies` refused from the restore's first step and written back after the
gate is removed.

## 1. Withdrawn: a record of the restore under /run

The first version of this model had a restarted fc-agent run the restore a second time, and
proposed a record under `/run` so that a restarted agent would fail closed instead. A restarted
agent never runs a restore again: it loops on its boot plan before the restore watcher exists,
and a guest restored from a memory image is served no plan (`BREAKS.md`, "What a killed
fc-agent leaves in a clone"). Nothing would read the record. The restore's order in the design
is 27add63dd's, unchanged.

What that repair also carried, the write of `tcp_syncookies` back, is repair 7.

## 2. `snapshot create` refuses a clone that has not published (#2138)

Model: `snapUnpublished := false`.

Change: on the host, `create_snapshot_core` and `create_snapshot_ch` refuse a VM whose restore
has not been acknowledged or has failed. `lifecycle_ready` is already written after the
acknowledgement (src/commands/snapshot.rs:5022) and nothing reads it. In the guest, as a second
line: `--resume-snapshot-network` and `rollback_preparation` open only a gate that a prepare of
the same `snapshot create` closed (the boundary token #1941 plans to pass to the prepare and to
every reopen would carry that).

A gap in the first wording, from the faithfulness review: a prepare one-shot that refuses must
refuse before the freeze. `main.rs:112-116` freezes, then prepares, and thaws when the prepare
fails; a refusal placed in the prepare would thaw a restoring clone's container behind its
closed gate.

Answers: `earlySnapshot`, `earlySnapshotFails`, `earlySnapshotCaptures`,
`failedCloneSnapshot`.

What the model shows: an unpublished clone of the design is never in a snapshot
(`CloneOk`: `sp = idle`), on which the proofs of P2, P3, P4 and P5 rest.

Proposed wording, for the doc of `resume_source_network`:

> The reopen is the undo of one prepare. It opens the gate that prepare closed and no other: a
> clone that has not published has a closed gate that is not a snapshot's to open.

## 3. A source's reopen is owed until it lands, and a guest that cannot be reopened is ended (#1936, #2142, #2149)

Model: `guard := true`, `reopenRetry := true`.

Change:

- #1941's guard, which runs the reopen when the create dies.
- A reopen that fails: `resume_source_network` leaves the manifest in place when `reopen_with`
  fails (it is the one marker that the boundary is still closed), and the host tries the
  reopen again before `snapshot create` returns, as the guard does after it.
- A reopen that cannot be delivered because no fc-agent answers the exec is a failed try like
  any other.
- After a bounded number of tries (three in the model, `retryMax`) the host, or the guard,
  ends the guest, as `snapshot run` ends a clone that does not acknowledge its restore. A
  guest whose fc-agent is dead and cannot come back has no container and no exec; leaving it
  running behind a closed gate helps nobody.
- The container stays frozen while a reopen is owed, and is thawed by the resume one-shot that
  lands (the model's `opened` to `idle`).

The last two are choices, and the second is the owner's: the alternative to ending the guest
is a source left gated and reported, which requirement 1 does not allow.

Answers: `hostDies`, `reopenFails`, `reopenFailsStuck`, `agentDies`, `agentDiesSource`.

What the model shows: `design_sourceUndisturbed` (no boundary in progress and a half of the
gate still in is unreachable), `design_neverStuck`, and `design_bounded` with
`design_settles`: a `snapshot create` that has begun is over within 20 steps of the system,
with the source running ungated or ended. `sourceEnded` in `Ends.lean` is the run where the
agent dies during the prepare on Firecracker: three tries, then the guest is ended.

Proposed wording, for the comment in `resume_source_network`:

> A reopen that fails leaves the boundary owed. The manifest stays, so the guest still says a
> boundary is closed, and whoever asked for the reopen asks again. A guest that cannot be
> reopened is ended: it is not left closed.

## 4. The reopen writes `rcu_expedited` back (#2141)

Model: `rcuUndo := true`.

Change: `ExpeditedGracePeriods::begin` writes a file under `/run/fcvm/` before it writes 1 to
the switch and removes it after writing the switch back. `resume_source_network` writes 0 to
the switch and removes the file when it finds one. On a clone nothing needs it: a restore
killed inside its barrier leaves a clone that never publishes and that the host ends.

Answers: `barrierKill`, `barrierKillCaptured`.

What the model shows: with it, `rcu_expedited` on outside a barrier in progress or an aborted
prepare is unreachable in a source (`SrcOk`), which is part of `design_sourceUndisturbed` and
of `design_noCapturedChange`.

## 5. The proxy's redirect closes with the egress half

Model: `natGate := true`.

Change: `close` adds, first in nat OUTPUT, a rule that accepts TCP without translating it while
the gate's egress half is in (a jump to a third gate chain holding `-p tcp -j ACCEPT`, installed
and removed with the OUTPUT half and verified with it). A connect then reaches filter OUTPUT
with the destination it asked for and is refused there like any other.

Answers: `proxyConnectA`, `proxyConnect`, `proxyStale`.

This one is a choice. 27add63dd documents the other answer: let the connect through, retire it
with the list, the process connects again. The repair makes "nothing leaves the guest behind
the closed egress half" true in rootless mode as it is in routed mode, which is what the
invariant for P2 needs (`Gn.gO`: while egress is closed, the only sockets a process of the clone
has opened are ends of connections inside the guest). Without it, P2 has to say that the
restore may destroy a new connection of the clone's own.

Two costs the first wording left out, from the faithfulness review:

- an ACCEPT first in nat OUTPUT also goes ahead of the retired-address chain, which a routed
  restore inserts first in ip6tables nat OUTPUT (fc-agent/src/retired_address.rs). The two
  have to be ordered on purpose;
- a rootless source then refuses the connects of processes outside the container for as long
  as a `snapshot create` holds its gate, where today they pass through the proxy.

Not verified: that iptables-nft accepts the rule where the proxy's REDIRECT is, and what it
costs the gate's verification on the restore path.

## 6. The freeze watchdog leaves a boundary in progress alone (#2140)

Model: `wdMidBoundary := false`.

Change: `payload_freeze`'s watchdog skips a freeze from the moment a prepare one-shot makes it
until that `snapshot create` has resumed the guest, and while a restore is running. The first
wording keyed this on "the manifest is present", which is narrower than the model: the model's
condition (`inPrepare`) begins at the freeze, before the manifest is saved. What ends a
boundary that never finishes is the host: its acknowledgement timeout for a clone, repair 3
for a source.

Answers: `watchdogThaw`.

What the model shows: `design_workloadEgress`: while a clone's egress half is closed, its
container does not run and nothing of it has been retired.

Proposed wording, for the module doc of `payload_freeze.rs`, under "The deadline":

> The deadline is for a freeze nobody owns any more. While the boundary that made the freeze is
> in progress, the boundary owns it: thawing a container behind a closed gate gives its
> connections the gate's reset.

## 7. A write of `tcp_syncookies` back that keeps failing fails the restore

Model: `backFails := true`.

Change: `restore_syn_cookies` returns its error after its tries, and the restore fails
(`shutdown_vm`) instead of going on with a warning. The acknowledgement is sent after the
write has landed: a clone the host has been told is ready has the setting it booted with.

The ingress half is already out when the write is tried (it has to be: `cookiesBackFirst`), so
between the two the clone serves with `tcp_syncookies` at 0, and a clone whose write fails is
reachable until its power-off. That window is bounded: three tries and the power-off
(`design_restore_ends`).

Answers: `writeBackFails`, and the snapshot that captures the 0 after it.

What the model shows: `design_noLastingChange`: a published clone has `tcp_syncookies` as it
was at boot.

## 8. An fc-agent that starts behind a closed egress half waits for it to open

Model: `startWaits := true`.

Found by the proof, not by a recorded run: the invariant "a guest in a snapshot's prepare has
its container frozen" does not hold without it. A cold-booted guest's restarted agent is
served its boot plan again. Over MMDS the closed egress half already stops it. Over vsock
(Cloud Hypervisor) nothing does: the new agent runs its boot path behind the closed gate and
starts a new container while the prepare holds the guest for frozen, and if the host then
pauses, the image holds a running container, which every clone resumes behind its closed gate.

Change: in `agent::run`, before the boot plan loop, wait while the gate's OUTPUT chain is
hooked. With repair 3 the host has by then tried the reopen and ended the guest it could not
reach.

Answers: no recorded run. It needs fc-agent to die between a prepare's exit and the pause, and
to be back with a container before the pause, on Cloud Hypervisor.

What the model shows: with `startWaits := false` the build fails at `restart_scal`
(`Lemmas.lean`).

## What carries each repair

`lake build GateVerify.Proofs` with one field of `Design.rules` put back (`Proofs` does not
import the recorded runs, so these are the proofs failing, not a replayed run):

| Field put back | The build fails at |
|---|---|
| 2: `snapUnpublished := true` | `snap_scal` (`Lemmas.lean`) |
| 3: `reopenRetry := false` | `snap_scal` |
| 3: `guard := false` | `settle_src` (`Ends.lean`) |
| 4: `rcuUndo := false` | `snap_scal` |
| 5: `natGate := false` | `conn_evo2` (`Gens.lean`) |
| 6: `wdMidBoundary := true` | `watchdog_scal` (`Lemmas.lean`) |
| 7: `backFails := false` | `agentFail_scal` (`Lemmas.lean`) |
| 8: `startWaits := false` | `restart_scal` (`Lemmas.lean`) |
| inherited from #2113: `tcpByCt := true` | `rejected_fl` (`Lemmas.lean`) |
| inherited from #2124: `look := false` | `manifest_loop` (`Gens.lean`) |
| inherited from #2124: `manAddr := true` | `manifest_loop` |
| inherited from #2124: `list := .addr` | `sk_dump6` (`Socks.lean`) |

Run on a copy of this directory, one field at a time; every one exits 1 and the unchanged copy
exits 0.

## What the design does not repair

- **The tracker's residue (#2112).** The clone's UDP rule still attaches connection tracking
  and each restore still leaves entries. The model does not carry them as a property.
- **UDP before the rule is in.** Left to the host (`Cfg.udpReach = false`), assumed.
- **A snapshot's source that completes a SYN-cookie handshake between its dump and its pause.**
  By design, as in #2124: the socket is the source's own and the clone's list retires it.
- **One end of a connection inside the guest left alone by the kernel** (`Limits.lean`): an
  accepted cost of the mirror rule.
- **What a restarted fc-agent writes before its plan loop** (resolv.conf, the boot command
  line's IPv6 address): not in the model.
