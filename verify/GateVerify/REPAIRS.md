# What the breaks need

`Design.rules` (`Design.lean`) is PR #2124 at its head, 27add63dd, with the six changes below.
Each is one field of the rule book. For each: what it is in the real system, where, the break it
answers (`BREAKS.md`), what the model shows of it, and wording proposed for the code or its
documents.

With all six the search finds no break in any configuration (`lake exe gate-breaks design`,
and the two `all-` runs), and P1, the second half of P3, P4, P5, P6 and U1 are proved
(`Proofs.lean`). P2 and the first half of P3 are searched and not proved (`README.md`).

None of this is implemented. The fixes #2113 and #2124 already make are not repeated here: the
flag rule (`tcpByCt := false`), the restore's own list with the mirror image deciding and a
second dump, `tcp_syncookies` refused from the restore's first step and written back after the
gate is removed.

## 1. The restore's changes are owned by a record under /run (#2139)

Model: `journal` as the restore's first step, `unjournal` as its last, `rerun := false`.

Change, in fc-agent:

- `restore_snapshot_network` writes a record under `/run/fcvm/` before `refuse_syn_cookies`:
  that a restore is in flight, the restore epoch, and the `tcp_syncookies` value it is about to
  replace. `/run` is guest memory, so the record goes where the change goes.
- Publication writes `tcp_syncookies` from the record, not from the gate object, and removes
  the record last. A write that fails leaves the record, and the watchdog thread tries the
  write again until it lands. The prepare one-shot refuses a guest that has the record.
- `agent::run` looks for the record before anything else, the boot plan included. An fc-agent
  that starts and finds it calls `shutdown_vm(1)`: it does not finish a restore it has no
  memory of.
- The epoch a restore published is kept under `/run` too, and `watch_restore_epoch` starts
  from it instead of from `None`, so a later restart does not dispatch it again.

Answers: `restartCookies`, `restartEgress`, `restartRetires`, the published clone that a
restarted agent closes and powers off, `writeBackFails`.

What the model shows: with the record, no kill of fc-agent at any of the restore's 18 steps
breaks a property (run `kills`, `kills-vsock`, `two-gen`), and `design_noLastingChange` is a
theorem: a published clone has `tcp_syncookies` as it was at boot.

Proposed wording, for `.claude/CLAUDE.md` under "On restore":

> A restore owns what it changes through a record under `/run`, written before the first change
> and removed after the last is undone. An fc-agent that starts and finds the record fails
> closed. A restore never runs twice: the second run cannot tell what the first one did.

## 2. `snapshot create` refuses a clone that has not published (#2138)

Model: `snapUnpublished := false`.

Change: on the host, `create_snapshot_core` and `create_snapshot_ch` refuse a VM whose restore
has not been acknowledged or has failed (the state `snapshot run` holds while it waits for the
acknowledgement, written where `snapshot create` can read it). In the guest, as a second line:
`--resume-snapshot-network` and `rollback_preparation` open only a gate that a prepare of the
same `snapshot create` closed (the boundary token #1941 plans to pass to the prepare and to
every reopen would carry that), and the prepare refuses while a restore is in flight (the
record of repair 1).

Answers: `earlySnapshot`, `earlySnapshotFails`, `earlySnapshotCaptures`,
`failedCloneSnapshot`.

What the model shows: an unpublished clone of the design is never in a snapshot
(`CloneOk`: `sp = idle`), on which the proofs of P3's second half, P4 and P5 rest.

Proposed wording, for the doc of `resume_source_network`:

> The reopen is the undo of one prepare. It opens the gate that prepare closed and no other: a
> clone that has not published has a closed gate that is not a snapshot's to open.

## 3. A source's reopen is owed until it lands (#1936, #2142)

Model: `guard := true`, `reopenRetry := true`.

Change: #1941's guard, which runs the reopen when the create dies. And for a reopen that
fails: `resume_source_network` leaves the manifest in place when `reopen_with` fails (it is the
one marker that the boundary is still closed), and the host tries the reopen again before
`snapshot create` returns, as the guard does after it.

Answers: `hostDies`, `reopenFails`.

What the model shows: `design_sourceUndisturbed` (no boundary in progress and a half of the
gate still in is unreachable) and `design_neverStuck` (while a gate is closed, a step of the
system can still run). `design_neverStuck` is progress, not termination: it does not say the
retries end.

Proposed wording, for the comment in `resume_source_network`:

> A reopen that fails leaves the boundary owed. The manifest stays, so the guest still says a
> boundary is closed, and whoever asked for the reopen asks again.

## 4. The reopen writes `rcu_expedited` back (#2141)

Model: `rcuUndo := true`.

Change: `ExpeditedGracePeriods::begin` writes a file under `/run/fcvm/` before it writes 1 to
the switch and removes it after writing the switch back. `resume_source_network` writes 0 to
the switch and removes the file when it finds one. On a clone, repair 1 covers it: an agent
killed inside the barrier is restarted into `shutdown_vm`.

Answers: `barrierKill`.

What the model shows: with it, `rcu_expedited` on outside a barrier in progress or an aborted
prepare is unreachable (`SrcOk`), which is part of `design_sourceUndisturbed` and of
`design_noCapturedChange`.

## 5. The proxy's redirect closes with the egress half

Model: `natGate := true`.

Change: `close` adds, first in nat OUTPUT, a rule that accepts TCP without translating it while
the gate's egress half is in (a jump to a third gate chain holding `-p tcp -j ACCEPT`, installed
and removed with the OUTPUT half and verified with it). A connect then reaches filter OUTPUT
with the destination it asked for and is refused there like any other.

Answers: `proxyConnectA`, `proxyConnect`.

This one is a choice. 27add63dd documents the other answer: let the connect through, retire it
with the list, the process connects again. The repair makes "nothing leaves the guest behind
the closed egress half" true in rootless mode as it is in routed mode, which is what the
invariant for P2 needs ("while egress is closed, the only sockets a process of the clone has
opened are ends of connections inside the guest"). Without it, P2 has to say that the restore
may destroy a new connection of the clone's own.

Not verified: that iptables-nft accepts the rule where the proxy's REDIRECT is, and what it
costs the gate's verification on the restore path.

## 6. The freeze watchdog leaves a boundary in progress alone (#2140)

Model: `wdMidBoundary := false`.

Change: `payload_freeze`'s watchdog skips a freeze while a prepare is running or has exited 0
and has not been resumed (the manifest is present and no restore has begun), and while a
restore is running (the record of repair 1). What ends a boundary that never finishes is the
host: its acknowledgement timeout for a clone, the guard of repair 3 for a source.

Answers: `watchdogThaw`.

What the model shows: `design_workloadEgress`: while a clone's egress half is closed, its
container is frozen and nothing of it has been retired.

Proposed wording, for the module doc of `payload_freeze.rs`, under "The deadline":

> The deadline is for a freeze nobody owns any more. While the boundary that made the freeze is
> in progress, the boundary owns it: thawing a container behind a closed gate gives its
> connections the gate's reset.

## What the design does not repair

- **The tracker's residue (#2112).** The clone's UDP rule still attaches connection tracking
  and each restore still leaves entries. The model does not carry them as a property.
- **UDP before the rule is in.** Left to the host (`Cfg.udpReach = false`).
- **A snapshot's source that completes a SYN-cookie handshake between its dump and its pause.**
  By design, as in #2124: the socket is the source's own and the clone's list retires it.
