import GateVerify.Properties
import GateVerify.Today
import GateVerify.Design
import GateVerify.Configs

/-!
# The rules as they are, broken: each run the search found, checked by the kernel

Written by `../gen-breaks.py` from the trace output; edit that, not this file.

Copied exactly from `lake exe gate-breaks traces`: for each kind of break, the shortest run from
`init` after which a property is broken, under the rule book and the configuration it was found
in, and a few runs written by hand where the search reports a break only through a shorter run
of another kind (`handRuns` in `GateSearch.lean`). `by decide` replays the run and evaluates the
property in the kernel, so each is a checked fact about the model: a run that stops being
possible, or stops breaking its property, fails the build. `BREAKS.md` has each in words.

Each is followed by what the design does with the same run, aligned to the design's own restore
order: where the run has the agent do a restore step, the design's agent does its steps up to
and including that one (`printRun` in `GateSearch.lean`). Either the design refuses a step of
it (`design_refuses_`: the aligned run is not possible, and the refused step is its last), or
the run is possible and the property holds at its end (`design_keeps_`). These say what the
design does with one run. That the design keeps a property on every run is `Proofs.lean`. The
two UDP runs are the exception: the design does not change them (`design_too_`).
-/

namespace Gate


/-! ## Known breaks -/

/-- **The second fault of #2106.** On `ctstate`, a guest that tracks nothing: closing the gate
attaches the tracker, a segment of a connection that was already open is the first it sees of it,
so it is NEW, and the gate answers a live client of the source with a reset (5 steps). -/
def Run.liveReset : List Label :=
  [.pkt .a .plain .syn, .pkt .a .plain .ack, .snap, .snap,
   .pkt .a .plain .ack]

theorem breaks_liveReset :
    after (replay Today.ctstate Runs.routed Run.liveReset (init Runs.routed))
      sourceDisturbed := by decide

theorem design_keeps_liveReset :
    (replay Design.rules Runs.routed Run.liveReset (init Runs.routed)).map
      sourceDisturbed = some false := by decide

/-- **#2106.** On `ctstate`, a SYN on a tuple the guest still tracks as ESTABLISHED is not NEW: it
passes the source's closed gate after the dump, the request socket is in the image and in no
manifest, and the clone publishes with it (22 steps). -/
def Run.staleTuple : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .pkt .a .plain .syn,
   .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent]

theorem breaks_staleTuple :
    after (replay Today.ctstate Runs.routedStale Run.staleTuple (init Runs.routedStale))
      staleAtPublication := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.staleTupleDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap,
   .pkt .a .plain .syn, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem design_keeps_staleTuple :
    (replay Design.rules Runs.routedStale Run.staleTupleDesign (init Runs.routedStale)).map
      staleAtPublication = some false := by decide

/-- #2106 on the clone: the same SYN passes the clone's closed gate after its retirement (18 steps). -/
def Run.staleTupleLate : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .pkt .a .plain .syn]

theorem breaks_staleTupleLate :
    after (replay Today.ctstate Runs.routedStale Run.staleTupleLate (init Runs.routedStale))
      smuggledLate := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.staleTupleLateDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .pkt .a .plain .syn]

theorem design_keeps_staleTupleLate :
    (replay Design.rules Runs.routedStale Run.staleTupleLateDesign (init Runs.routedStale)).map
      smuggledLate = some false := by decide

/-- #2106 with nothing assumed, by the issue's own route: where the stale entry comes from. The gate's
rule attaches IPv4 tracking during one snapshot and it stays attached a little after the gate is
removed; a client's handshake is tracked then; the tracker is detached before the client closes,
so the close is not seen; and the next snapshot's clone publishes with that client's new
connection (37 steps). -/
def Run.staleTupleGrown : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .pkt .a .plain .syn, .detach, .pkt .a .plain .fin, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .pkt .a .plain .syn, .snap, .snap, .snap, .restore, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem breaks_staleTupleGrown :
    after (replay Today.ctstate Runs.routed Run.staleTupleGrown (init Runs.routed))
      staleAtPublication := by decide

/-- The same run with the design's restore: it refuses the last step. -/
def Run.staleTupleGrownDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .pkt .a .plain .syn, .detach]

theorem design_refuses_staleTupleGrown :
    replay Design.rules Runs.routed Run.staleTupleGrownDesign (init Runs.routed) = none
    ∧ (replay Design.rules Runs.routed (Run.staleTupleGrownDesign.take 12) (init Runs.routed)).isSome = true := by decide

/-- A variant of #2106 in a guest that always tracks. A request socket in the manifest is retired
without a reset (`tcp_abort`, tcp.c:5062-5069), so its tracked entry stays; the client's SYN, sent
again, is ignored by the tracker, is not NEW, and passes the clone's closed gate (19 steps). -/
def Run.halfOpenRetransmit : List Label :=
  [.pkt .a .plain .syn, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .pkt .a .plain .syn]

theorem breaks_halfOpenRetransmit :
    after (replay Today.ctstate Runs.nat Run.halfOpenRetransmit (init Runs.nat))
      smuggledLate := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.halfOpenRetransmitDesign : List Label :=
  [.pkt .a .plain .syn, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .pkt .a .plain .syn]

theorem design_keeps_halfOpenRetransmit :
    (replay Design.rules Runs.nat Run.halfOpenRetransmitDesign (init Runs.nat)).map
      smuggledLate = some false := by decide

/-- #2113's flag rule refuses that SYN like any other. -/
theorem flags_keeps_halfOpenRetransmit :
    ¬ after (replay Today.flags Runs.nat Run.halfOpenRetransmit (init Runs.nat)) smuggledLate := by
  decide

/-- And with an established connection: the clone's retirement resets it from the guest's side, which
leaves the tracked entry closed by a reset that was not the client's. A client that connects again
from the same address and port within the entry's timeout (10 s in Linux 6.18.60) sends a SYN the
tracker does not recycle: not NEW, it passes the clone's closed gate. `ctstate` only. Written by
hand: the search reports the shorter `halfOpenRetransmit` (20 steps). -/
def Run.reusedTuple : List Label :=
  [.pkt .a .plain .syn, .pkt .a .plain .ack, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .pkt .a .plain .syn]

theorem breaks_reusedTuple :
    after (replay Today.ctstate Runs.nat Run.reusedTuple (init Runs.nat))
      smuggledLate := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.reusedTupleDesign : List Label :=
  [.pkt .a .plain .syn, .pkt .a .plain .ack, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .pkt .a .plain .syn]

theorem design_keeps_reusedTuple :
    (replay Design.rules Runs.nat Run.reusedTupleDesign (init Runs.nat)).map
      smuggledLate = some false := by decide

/-- #2113's flag rule refuses that SYN too. -/
theorem flags_keeps_reusedTuple :
    ¬ after (replay Today.flags Runs.nat Run.reusedTuple (init Runs.nat)) smuggledLate := by
  decide

/-- **#2123.** With the flag rule, the ACK that completes a SYN-cookie handshake passes the source's
closed gate between its dump and its pause. The socket is in the image and in no manifest, and the
clone publishes with it (23 steps). -/
def Run.cookieAck : List Label :=
  [.pkt .a .plain .synFull, .snap, .snap, .snap, .snap, .snap,
   .snap, .pkt .a .plain .ack, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem breaks_cookieAck :
    after (replay Today.flags Runs.cookie Run.cookieAck (init Runs.cookie))
      staleAtPublication := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.cookieAckDesign : List Label :=
  [.pkt .a .plain .synFull, .snap, .snap, .snap, .snap,
   .snap, .snap, .pkt .a .plain .ack, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem design_keeps_cookieAck :
    (replay Design.rules Runs.cookie Run.cookieAckDesign (init Runs.cookie)).map
      staleAtPublication = some false := by decide

/-- #2123 on the clone: a cookie's ACK makes a socket after the retirement, before publication (19
steps). -/
def Run.cookieAckClone : List Label :=
  [.pkt .a .plain .synFull, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .pkt .a .plain .ack]

theorem breaks_cookieAckClone :
    after (replay Today.flags Runs.cookie Run.cookieAckClone (init Runs.cookie))
      smuggledLate := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.cookieAckCloneDesign : List Label :=
  [.pkt .a .plain .synFull, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .pkt .a .plain .ack]

theorem design_keeps_cookieAckClone :
    (replay Design.rules Runs.cookie Run.cookieAckCloneDesign (init Runs.cookie)).map
      smuggledLate = some false := by decide

/-- **The `/loopback` port.** A connection from a client of a port published with `/loopback` records
a loopback peer, so the manifest leaves it out as a connection inside the guest, and every clone
publishes with it. The same on `ctstate` (22 steps). -/
def Run.loopbackClient : List Label :=
  [.pkt .a .lb .syn, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent]

theorem breaks_loopbackClient :
    after (replay Today.flags Runs.loopbackPort Run.loopbackClient (init Runs.loopbackPort))
      staleAtPublication := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.loopbackClientDesign : List Label :=
  [.pkt .a .lb .syn, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem design_keeps_loopbackClient :
    (replay Design.rules Runs.loopbackPort Run.loopbackClientDesign (init Runs.loopbackPort)).map
      staleAtPublication = some false := by decide

/-- The review of a80e1ad34, P2: its own list skips a loopback peer the same way, so such a socket,
made by a cookie or not, is published (26 steps). -/
def Run.loopbackClientA : List Label :=
  [.pkt .a .lb .syn, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem breaks_loopbackClientA :
    after (replay Today.cookieA Runs.loopbackPort Run.loopbackClientA (init Runs.loopbackPort))
      staleAtPublication := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.loopbackClientADesign : List Label :=
  [.pkt .a .lb .syn, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem design_keeps_loopbackClientA :
    (replay Design.rules Runs.loopbackPort Run.loopbackClientADesign (init Runs.loopbackPort)).map
      staleAtPublication = some false := by decide

/-- The review of a80e1ad34, the list is two dumps: a connection made inside the guest between the
IPv4 dump and the IPv6 dump has one end listed, and the restore retires it. It needs a connection
whose listed end does not record a loopback peer: a client bound to the guest's own address (21
steps). -/
def Run.twoDumps : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .pair true, .agent, .agent,
   .agent]

theorem breaks_twoDumps :
    after (replay Today.cookieA Runs.pairs Run.twoDumps (init Runs.pairs))
      generationBroken := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.twoDumpsDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .pair true, .agent,
   .agent, .agent, .agent]

theorem design_keeps_twoDumps :
    (replay Design.rules Runs.pairs Run.twoDumpsDesign (init Runs.pairs)).map
      generationBroken = some false := by decide

/-- What 27add63dd's second dump is for: without it, and with the mirror image deciding, any
connection made inside the guest between the two dumps loses an end (21 steps). -/
def Run.oneDump : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .pair false, .snap,
   .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent]

theorem breaks_oneDump :
    after (replay Today.cookieBOneLook Runs.pairs Run.oneDump (init Runs.pairs))
      generationBroken := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.oneDumpDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .pair false,
   .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent]

theorem design_keeps_oneDump :
    (replay Design.rules Runs.pairs Run.oneDumpDesign (init Runs.pairs)).map
      generationBroken = some false := by decide

/-- The review of a80e1ad34, publication's order: `tcp_syncookies` is written back before the ingress
half is removed, so a cookie's ACK in between makes a socket in a clone that has not published
(and one that never does, if the removal fails) (26 steps). -/
def Run.cookiesBackFirst : List Label :=
  [.pkt .a .plain .synFull, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .pkt .a .plain .ack]

theorem breaks_cookiesBackFirst :
    after (replay Today.cookieA Runs.cookie Run.cookiesBackFirst (init Runs.cookie))
      smuggledLate := by decide

/-- The same run with the design's restore: the same steps in its own order. -/
def Run.cookiesBackFirstDesign : List Label :=
  [.pkt .a .plain .synFull, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .pkt .a .plain .ack]

theorem design_keeps_cookiesBackFirst :
    (replay Design.rules Runs.cookie Run.cookiesBackFirstDesign (init Runs.cookie)).map
      smuggledLate = some false := by decide

/-- The review of a80e1ad34, the egress proxy: nat OUTPUT redirects a connect before the gate's OUTPUT
chain sees it, so a process outside the frozen container connects out behind the closed egress
half, and the restore's list retires its two sockets (21 steps). -/
def Run.proxyConnectA : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .conn .dmn, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent]

theorem breaks_proxyConnectA :
    after (replay Today.cookieA Runs.proxy Run.proxyConnectA (init Runs.proxy))
      generationBroken := by decide

/-- The same run with the design's restore: it refuses the last step. -/
def Run.proxyConnectADesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .restore, .conn .dmn]

theorem design_refuses_proxyConnectA :
    replay Design.rules Runs.proxy Run.proxyConnectADesign (init Runs.proxy) = none
    ∧ (replay Design.rules Runs.proxy (Run.proxyConnectADesign.take 10) (init Runs.proxy)).isSome = true := by decide

/-- The same at 27add63dd, which documents it: both sockets are retired and the process connects again
(22 steps). -/
def Run.proxyConnect : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .conn .dmn, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent]

theorem breaks_proxyConnect :
    after (replay Today.cookieB Runs.proxy Run.proxyConnect (init Runs.proxy))
      generationBroken := by decide

/-- The same run with the design's restore: it refuses the last step. -/
def Run.proxyConnectDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .restore, .conn .dmn]

theorem design_refuses_proxyConnect :
    replay Design.rules Runs.proxy Run.proxyConnectDesign (init Runs.proxy) = none
    ∧ (replay Design.rules Runs.proxy (Run.proxyConnectDesign.take 10) (init Runs.proxy)).isSome = true := by decide

/-- The same connect on the source, where `ctstate` and #2113 have no list: made behind the source's
closed egress half after the dump, its two sockets are in the image and in no manifest, and the
clone publishes holding them. (The relay behind the proxy is vsock and the restore resets it by
other means, which the model does not hold; the two TCP sockets are what the test reads.) (22
steps). -/
def Run.proxyStale : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .conn .dmn, .snap,
   .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent]

theorem breaks_proxyStale :
    after (replay Today.flags Runs.proxy Run.proxyStale (init Runs.proxy))
      staleAtPublication := by decide

/-- The same run with the design's restore: it refuses the last step. -/
def Run.proxyStaleDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .conn .dmn]

theorem design_refuses_proxyStale :
    replay Design.rules Runs.proxy Run.proxyStaleDesign (init Runs.proxy) = none
    ∧ (replay Design.rules Runs.proxy (Run.proxyStaleDesign.take 6) (init Runs.proxy)).isSome = true := by decide

/-- **#1936.** The host's `snapshot create` dies after the prepare: nothing opens the source's gate (9
steps). -/
def Run.hostDies : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .hostDie]

theorem breaks_hostDies :
    after (replay Today.cookieB Runs.host Run.hostDies (init Runs.host))
      (stuck Today.cookieB Runs.host) := by decide

theorem design_keeps_hostDies :
    (replay Design.rules Runs.host Run.hostDies (init Runs.host)).map
      (stuck Design.rules Runs.host) = some false := by decide

/-! ## Breaks the search found in PR #2124 at its head -/

/-- `snapshot create` of a clone whose restore has not taken the boundary lock yet: the prepare closes
a gate that is already closed, and the resume one-shot opens it. The clone is open to clients with
every socket of its source and no restore (22 steps). -/
def Run.earlySnapshot : List Label :=
  [.pkt .a .plain .syn, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .restore, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap]

theorem breaks_earlySnapshot :
    after (replay Today.cookieB Runs.early Run.earlySnapshot (init Runs.early))
      staleAtPublication := by decide

/-- The same run with the design's restore: it refuses the last step. -/
def Run.earlySnapshotDesign : List Label :=
  [.pkt .a .plain .syn, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .snap]

theorem design_refuses_earlySnapshot :
    replay Design.rules Runs.early Run.earlySnapshotDesign (init Runs.early) = none
    ∧ (replay Design.rules Runs.early (Run.earlySnapshotDesign.take 11) (init Runs.early)).isSome = true := by decide

/-- The same with a prepare that fails: its rollback opens the gate (13 steps). -/
def Run.earlySnapshotFails : List Label :=
  [.pkt .a .plain .syn, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .snap, .snapFail]

theorem breaks_earlySnapshotFails :
    after (replay Today.cookieB Runs.earlyFails Run.earlySnapshotFails (init Runs.earlyFails))
      staleAtPublication := by decide

/-- The same run with the design's restore: it refuses the last step. -/
def Run.earlySnapshotFailsDesign : List Label :=
  [.pkt .a .plain .syn, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .snap]

theorem design_refuses_earlySnapshotFails :
    replay Design.rules Runs.earlyFails Run.earlySnapshotFailsDesign (init Runs.earlyFails) = none
    ∧ (replay Design.rules Runs.earlyFails (Run.earlySnapshotFailsDesign.take 11) (init Runs.earlyFails)).isSome = true := by decide

/-- A snapshot of a clone whose restore begins between the prepare and the pause: the image holds
`tcp_syncookies` at 0 (20 steps). -/
def Run.earlySnapshotCaptures : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .restore, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .agent, .snap]

theorem breaks_earlySnapshotCaptures :
    after (replay Today.cookieB Runs.early Run.earlySnapshotCaptures (init Runs.early))
      capturedChange := by decide

/-- The same run with the design's restore: it refuses the last step. -/
def Run.earlySnapshotCapturesDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .restore, .snap]

theorem design_refuses_earlySnapshotCaptures :
    replay Design.rules Runs.early Run.earlySnapshotCapturesDesign (init Runs.early) = none
    ∧ (replay Design.rules Runs.early (Run.earlySnapshotCapturesDesign.take 10) (init Runs.early)).isSome = true := by decide

/-- **Can a snapshot capture `tcp_syncookies` at 0?** Yes. A restore that fails after
`refuse_syn_cookies` leaves the 0 and goes to `shutdown_vm`; a `snapshot create` asked of the
clone before the power-off lands is not refused, and its image holds the 0. A clone of that image
resumes in the same `shutdown_vm` and powers off (`failedCloneSnapshotEnds`) (21 steps). -/
def Run.failedCloneSnapshot : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .restore, .agent, .agentFail, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap]

theorem breaks_failedCloneSnapshot :
    after (replay Today.cookieB Runs.earlyFails Run.failedCloneSnapshot (init Runs.earlyFails))
      capturedChange := by decide

/-- The same run with the design's restore: it refuses the last step. -/
def Run.failedCloneSnapshotDesign : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .restore, .agent, .agentFail, .snap]

theorem design_refuses_failedCloneSnapshot :
    replay Design.rules Runs.earlyFails Run.failedCloneSnapshotDesign (init Runs.earlyFails) = none
    ∧ (replay Design.rules Runs.earlyFails (Run.failedCloneSnapshotDesign.take 12) (init Runs.earlyFails)).isSome = true := by decide

/-- The clone of that image: it is restored, and the only step its agent has left is the
power-off. -/
theorem failedCloneSnapshotEnds :
    (replay Today.cookieB { Runs.earlyFails with maxGens := 2 }
        (Run.failedCloneSnapshot ++ [.restore, .off]) (init Runs.earlyFails)).map (·.pw)
      = some false
    ∧ (replay Today.cookieB { Runs.earlyFails with maxGens := 2 }
        (Run.failedCloneSnapshot ++ [.restore, .agent]) (init Runs.earlyFails)) = none := by
  decide

/-- fc-agent dies between the prepare and the reopen. The one-shots are exec'd through it, so the
host's resume one-shot cannot run, `snapshot create` returns its error, and the source keeps what
the prepare had installed. On Firecracker the restarted agent fetches its boot plan over MMDS,
behind the closed egress half, and never gets it: the source stays gated for good (4 steps). -/
def Run.agentDies : List Label :=
  [.snap, .snap, .kill, .snap]

theorem breaks_agentDies :
    after (replay Today.cookieB Runs.kills Run.agentDies (init Runs.kills))
      (stuck Today.cookieB Runs.kills) := by decide

theorem design_keeps_agentDies :
    (replay Design.rules Runs.kills Run.agentDies (init Runs.kills)).map
      (stuck Design.rules Runs.kills) = some false := by decide

/-- The same run read as P1: `snapshot create` is over and the source has a half of the gate (4
steps). -/
def Run.agentDiesSource : List Label :=
  [.snap, .snap, .kill, .snap]

theorem breaks_agentDiesSource :
    after (replay Today.cookieB Runs.kills Run.agentDiesSource (init Runs.kills))
      sourceDisturbed := by decide

theorem design_keeps_agentDiesSource :
    (replay Design.rules Runs.kills Run.agentDiesSource (init Runs.kills)).map
      sourceDisturbed = some false := by decide

/-- The same over vsock (Cloud Hypervisor), where a guest that booted without the snapshot cache is
served its boot plan behind the closed gate: the agent comes back with its exec server and starts
the container, after the host's exec has given up, and nothing asks for the reopen. (If it is back
inside the exec's 54 s of connect retries, the reopen lands and nothing is broken.) (6 steps). -/
def Run.agentDiesVsock : List Label :=
  [.snap, .snap, .kill, .snap, .restart, .boot]

theorem breaks_agentDiesVsock :
    after (replay Today.cookieB Runs.killsVsock Run.agentDiesVsock (init Runs.killsVsock))
      (stuck Today.cookieB Runs.killsVsock) := by decide

theorem design_refuses_agentDiesVsock :
    replay Design.rules Runs.killsVsock Run.agentDiesVsock (init Runs.killsVsock) = none
    ∧ (replay Design.rules Runs.killsVsock (Run.agentDiesVsock.take 5) (init Runs.killsVsock)).isSome = true := by decide

/-- Over vsock again: fc-agent dies after the prepare one-shot has replied and is back before the
host's pause, which is the next call the host makes. The image holds the new agent with its
container not yet started, and the clone's agent starts it during the restore, behind the closed
egress half. It needs the unit's restart (`RestartSec=1`, and up to the 90 s stop timeout if the
frozen container does not die on SIGTERM) to fit between two adjacent host calls (13 steps). -/
def Run.containerMidBoundary : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .kill, .restart, .snap, .restore, .boot]

theorem breaks_containerMidBoundary :
    after (replay Today.cookieB Runs.killsVsock Run.containerMidBoundary (init Runs.killsVsock))
      workloadBehindEgress := by decide

theorem design_refuses_containerMidBoundary :
    replay Design.rules Runs.killsVsock Run.containerMidBoundary (init Runs.killsVsock) = none
    ∧ (replay Design.rules Runs.killsVsock (Run.containerMidBoundary.take 12) (init Runs.killsVsock)).isSome = true := by decide

/-- The prepare one-shot is killed inside the receive barrier: `/sys/kernel/rcu_expedited` stays 1 on
the source after the snapshot has failed and the gate is open again. Written by hand: in this
configuration the search reports the shorter `agentDiesSource` (7 steps). -/
def Run.barrierKill : List Label :=
  [.snap, .snap, .snap, .snap, .snapKill, .snap, .snap]

theorem breaks_barrierKill :
    after (replay Today.cookieB Runs.kills Run.barrierKill (init Runs.kills))
      sourceDisturbed := by decide

theorem design_keeps_barrierKill :
    (replay Design.rules Runs.kills Run.barrierKill (init Runs.kills)).map
      sourceDisturbed = some false := by decide

/-- And every later snapshot of that guest captures the 1: the next barrier finds the switch on and
leaves it on (16 steps). -/
def Run.barrierKillCaptured : List Label :=
  [.snap, .snap, .snap, .snap, .snapKill, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap]

theorem breaks_barrierKillCaptured :
    after (replay Today.cookieB Runs.kills Run.barrierKillCaptured (init Runs.kills))
      capturedChange := by decide

theorem design_keeps_barrierKillCaptured :
    (replay Design.rules Runs.kills Run.barrierKillCaptured (init Runs.kills)).map
      capturedChange = some false := by decide

/-- A boundary held past the payload freeze's deadline: the watchdog thaws the container while both
halves of the clone's gate are closed (11 steps). -/
def Run.watchdogThaw : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .watchdog]

theorem breaks_watchdogThaw :
    after (replay Today.cookieB Runs.slow Run.watchdogThaw (init Runs.slow))
      workloadBehindEgress := by decide

theorem design_refuses_watchdogThaw :
    replay Design.rules Runs.slow Run.watchdogThaw (init Runs.slow) = none
    ∧ (replay Design.rules Runs.slow (Run.watchdogThaw.take 10) (init Runs.slow)).isSome = true := by decide

/-- The resume one-shot fails to open the gate. It removes the manifest, thaws, exits non-zero, and
nothing tries again: the source stays gated (11 steps). -/
def Run.reopenFails : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snapFail]

theorem breaks_reopenFails :
    after (replay Today.cookieB Runs.fails Run.reopenFails (init Runs.fails))
      sourceDisturbed := by decide

theorem design_keeps_reopenFails :
    (replay Design.rules Runs.fails Run.reopenFails (init Runs.fails)).map
      sourceDisturbed = some false := by decide

/-- The same run read as P6: the gate is closed and no step of the system is left that would open it
(11 steps). -/
def Run.reopenFailsStuck : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snapFail]

theorem breaks_reopenFailsStuck :
    after (replay Today.cookieB Runs.fails Run.reopenFailsStuck (init Runs.fails))
      (stuck Today.cookieB Runs.fails) := by decide

theorem design_keeps_reopenFailsStuck :
    (replay Design.rules Runs.fails Run.reopenFailsStuck (init Runs.fails)).map
      (stuck Design.rules Runs.fails) = some false := by decide

/-- The write of `tcp_syncookies` back fails three times: the clone is published without SYN cookies,
with a warning (documented in the PR) (26 steps). -/
def Run.writeBackFails : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agentFail]

theorem breaks_writeBackFails :
    after (replay Today.cookieB Runs.fails Run.writeBackFails (init Runs.fails))
      (lastingChange Runs.fails) := by decide

theorem design_keeps_writeBackFails :
    (replay Design.rules Runs.fails Run.writeBackFails (init Runs.fails)).map
      (lastingChange Runs.fails) = some false := by decide

/-! ## UDP -/

/-- What a stale entry opens: a datagram from the address and port the guest itself sent to before the
snapshot is in the reply direction of an entry the clone inherits, and the UDP rule passes it. Not
a break: the rule says so (15 steps). -/
def Run.replyDatagram : List Label :=
  [.udpS, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .restore, .agent, .agent, .agent, .udpR]

theorem breaks_replyDatagram :
    after (replay Today.cookieB Runs.udp Run.replyDatagram (init Runs.udp))
      replyDatagram := by decide

/-- The design does not change this. -/
theorem design_too_replyDatagram :
    after (replay Design.rules Runs.udp Run.replyDatagram (init Runs.udp))
      replyDatagram := by decide

/-- Without the host's gate, a client's datagram reaches a clone between its resume and its UDP rule
(#1114) (11 steps). -/
def Run.clientDatagram : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .udpC]

theorem breaks_clientDatagram :
    after (replay Today.cookieB Runs.udpReach Run.clientDatagram (init Runs.udpReach))
      clientDatagram := by decide

/-- The design does not change this. -/
theorem design_too_clientDatagram :
    after (replay Design.rules Runs.udpReach Run.clientDatagram (init Runs.udpReach))
      clientDatagram := by decide

end Gate
