import GateVerify.Properties
import GateVerify.Today
import GateVerify.Design
import GateVerify.Configs

/-!
# The rules as they are, broken: each run the search found, checked by the kernel

Copied exactly from `lake exe gate-breaks traces`: for each kind of break, the shortest run from
`init` after which a property is broken, under the rule book and the configuration it was found
in. `by decide` replays the run and evaluates the property in the kernel, so each is a checked
fact about the model: a run that stops being possible, or stops breaking its property, fails the
build. `verify/BREAKS.md` has each in words, with its steps.

Each is followed by the same run under `Design.rules`: there the run is not possible, or the
property holds at its end (`design_keeps_`). The two UDP runs are the exception: the design does
not change them (`design_too_`).
-/

namespace Gate

set_option maxRecDepth 100000

/-! ## Known breaks -/

/-- **The second fault of #2106.** On `main`, a guest that tracks nothing: closing the gate attaches
the tracker, a segment of a connection that was already open is the first it sees of it, so it is
NEW, and the gate answers a live client of the source with a reset (5 steps). -/
def Run.liveReset : List Label :=
  [.pkt .a .plain .syn, .pkt .a .plain .ack, .snap, .snap,
   .pkt .a .plain .ack]

theorem breaks_liveReset :
    after (replay Today.main Runs.routed Run.liveReset (init Runs.routed))
      sourceDisturbed := by decide

theorem design_keeps_liveReset :
    ¬ after (replay Design.rules Runs.routed Run.liveReset (init Runs.routed))
      sourceDisturbed := by decide

/-- **#2106.** On `main`, a SYN on a tuple the guest still tracks as ESTABLISHED is not NEW: it passes
the source's closed gate after the dump, the request socket is in the image and in no manifest,
and the clone publishes with it (22 steps). -/
def Run.staleTuple : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .pkt .a .plain .syn,
   .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent]

theorem breaks_staleTuple :
    after (replay Today.main Runs.routedStale Run.staleTuple (init Runs.routedStale))
      staleAtPublication := by decide

theorem design_keeps_staleTuple :
    ¬ after (replay Design.rules Runs.routedStale Run.staleTuple (init Runs.routedStale))
      staleAtPublication := by decide

/-- #2106 on the clone: the same SYN passes the clone's closed gate after its retirement (18 steps). -/
def Run.staleTupleLate : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .pkt .a .plain .syn]

theorem breaks_staleTupleLate :
    after (replay Today.main Runs.routedStale Run.staleTupleLate (init Runs.routedStale))
      smuggledLate := by decide

theorem design_keeps_staleTupleLate :
    ¬ after (replay Design.rules Runs.routedStale Run.staleTupleLate (init Runs.routedStale))
      smuggledLate := by decide

/-- #2106 with nothing assumed: where the stale entry comes from. The gate's own rule attaches the
tracker during one snapshot, a client's handshake is tracked, the tracker is detached before the
client closes, and the next snapshot's clone publishes with that client's new connection (37
steps). -/
def Run.staleTupleGrown : List Label :=
  [.snap, .snap, .pkt .b .plain .syn, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .snap, .detach, .pkt .b .plain .fin, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .pkt .b .plain .syn, .snap, .snap, .restore, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem breaks_staleTupleGrown :
    after (replay Today.main Runs.routed Run.staleTupleGrown (init Runs.routed))
      staleAtPublication := by decide

theorem design_keeps_staleTupleGrown :
    ¬ after (replay Design.rules Runs.routed Run.staleTupleGrown (init Runs.routed))
      staleAtPublication := by decide

/-- A variant of #2106 in a guest that always tracks. A request socket in the manifest is retired
without a reset (`tcp_abort`, tcp.c:5062-5069), so its tracked entry stays; the client's SYN, sent
again, is ignored by the tracker, is not NEW, and passes the clone's closed gate (19 steps). -/
def Run.halfOpenRetransmit : List Label :=
  [.pkt .a .plain .syn, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .pkt .a .plain .syn]

theorem breaks_halfOpenRetransmit :
    after (replay Today.main Runs.nat Run.halfOpenRetransmit (init Runs.nat))
      smuggledLate := by decide

theorem design_keeps_halfOpenRetransmit :
    ¬ after (replay Design.rules Runs.nat Run.halfOpenRetransmit (init Runs.nat))
      smuggledLate := by decide

/-- #2113's flag rule refuses that SYN like any other. -/
theorem flags_keeps_halfOpenRetransmit :
    ¬ after (replay Today.flags Runs.nat Run.halfOpenRetransmit (init Runs.nat)) smuggledLate := by
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

theorem design_keeps_cookieAck :
    ¬ after (replay Design.rules Runs.cookie Run.cookieAck (init Runs.cookie))
      staleAtPublication := by decide

/-- #2123 on the clone: a cookie's ACK makes a socket after the retirement, before publication (19
steps). -/
def Run.cookieAckClone : List Label :=
  [.pkt .a .plain .synFull, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .pkt .a .plain .ack]

theorem breaks_cookieAckClone :
    after (replay Today.flags Runs.cookie Run.cookieAckClone (init Runs.cookie))
      smuggledLate := by decide

theorem design_keeps_cookieAckClone :
    ¬ after (replay Design.rules Runs.cookie Run.cookieAckClone (init Runs.cookie))
      smuggledLate := by decide

/-- **The `/loopback` port.** A connection from a client of a port published with `/loopback` records
a loopback peer, so the manifest leaves it out as a connection inside the guest, and every clone
publishes with it. The same on `main` (22 steps). -/
def Run.loopbackClient : List Label :=
  [.pkt .a .lb .syn, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent]

theorem breaks_loopbackClient :
    after (replay Today.flags Runs.loopbackPort Run.loopbackClient (init Runs.loopbackPort))
      staleAtPublication := by decide

theorem design_keeps_loopbackClient :
    ¬ after (replay Design.rules Runs.loopbackPort Run.loopbackClient (init Runs.loopbackPort))
      staleAtPublication := by decide

/-- The review of a80e1ad34, P2: its own list skips a loopback peer the same way, so such a socket,
made by a cookie or not, is published (26 steps). -/
def Run.loopbackClientA : List Label :=
  [.pkt .a .lb .syn, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent]

theorem breaks_loopbackClientA :
    after (replay Today.cookieA Runs.loopbackPort Run.loopbackClientA (init Runs.loopbackPort))
      staleAtPublication := by decide

theorem design_keeps_loopbackClientA :
    ¬ after (replay Design.rules Runs.loopbackPort Run.loopbackClientA (init Runs.loopbackPort))
      staleAtPublication := by decide

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

theorem design_keeps_twoDumps :
    ¬ after (replay Design.rules Runs.pairs Run.twoDumps (init Runs.pairs))
      generationBroken := by decide

/-- What 27add63dd's second dump is for: without it, and with the mirror image deciding, any
connection made inside the guest between the two dumps loses an end (21 steps). -/
def Run.oneDump : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .pair false, .agent, .agent,
   .agent]

theorem breaks_oneDump :
    after (replay Today.cookieBOneLook Runs.pairs Run.oneDump (init Runs.pairs))
      generationBroken := by decide

theorem design_keeps_oneDump :
    ¬ after (replay Design.rules Runs.pairs Run.oneDump (init Runs.pairs))
      generationBroken := by decide

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

theorem design_keeps_cookiesBackFirst :
    ¬ after (replay Design.rules Runs.cookie Run.cookiesBackFirst (init Runs.cookie))
      smuggledLate := by decide

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

theorem design_keeps_proxyConnectA :
    ¬ after (replay Design.rules Runs.proxy Run.proxyConnectA (init Runs.proxy))
      generationBroken := by decide

/-- The same at 27add63dd, which documents it: both sockets are retired and the process connects again
(22 steps). -/
def Run.proxyConnect : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .conn .dmn, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent]

theorem breaks_proxyConnect :
    after (replay Today.cookieB Runs.proxy Run.proxyConnect (init Runs.proxy))
      generationBroken := by decide

theorem design_keeps_proxyConnect :
    ¬ after (replay Design.rules Runs.proxy Run.proxyConnect (init Runs.proxy))
      generationBroken := by decide

/-- **#1936.** The host's `snapshot create` dies after the prepare: nothing opens the source's gate (9
steps). -/
def Run.hostDies : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .hostDie]

theorem breaks_hostDies :
    after (replay Today.cookieB Runs.host Run.hostDies (init Runs.host))
      (stuck Today.cookieB Runs.host) := by decide

theorem design_keeps_hostDies :
    ¬ after (replay Design.rules Runs.host Run.hostDies (init Runs.host))
      (stuck Design.rules Runs.host) := by decide

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

theorem design_keeps_earlySnapshot :
    ¬ after (replay Design.rules Runs.early Run.earlySnapshot (init Runs.early))
      staleAtPublication := by decide

/-- The same with a prepare that fails: its rollback opens the gate (13 steps). -/
def Run.earlySnapshotFails : List Label :=
  [.pkt .a .plain .syn, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .snap, .snap, .restore, .snap, .snapFail]

theorem breaks_earlySnapshotFails :
    after (replay Today.cookieB Runs.earlyFails Run.earlySnapshotFails (init Runs.earlyFails))
      staleAtPublication := by decide

theorem design_keeps_earlySnapshotFails :
    ¬ after (replay Design.rules Runs.earlyFails Run.earlySnapshotFails (init Runs.earlyFails))
      staleAtPublication := by decide

/-- A snapshot of a clone whose restore begins between the prepare and the pause: the image holds
`tcp_syncookies` at 0 (20 steps). -/
def Run.earlySnapshotCaptures : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .restore, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap, .agent, .snap]

theorem breaks_earlySnapshotCaptures :
    after (replay Today.cookieB Runs.early Run.earlySnapshotCaptures (init Runs.early))
      capturedChange := by decide

theorem design_keeps_earlySnapshotCaptures :
    ¬ after (replay Design.rules Runs.early Run.earlySnapshotCaptures (init Runs.early))
      capturedChange := by decide

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

theorem design_keeps_failedCloneSnapshot :
    ¬ after (replay Design.rules Runs.earlyFails Run.failedCloneSnapshot (init Runs.earlyFails))
      capturedChange := by decide

/-- The clone of that image: it is restored, and the only step its agent has left is the
power-off. -/
theorem failedCloneSnapshotEnds :
    (replay Today.cookieB { Runs.earlyFails with maxGens := 2 }
        (Run.failedCloneSnapshot ++ [.restore, .off]) (init Runs.earlyFails)).map (·.pw)
      = some false
    ∧ (replay Today.cookieB { Runs.earlyFails with maxGens := 2 }
        (Run.failedCloneSnapshot ++ [.restore, .agent]) (init Runs.earlyFails)) = none := by
  decide

/-- fc-agent is killed during the retirement (after `refuse_syn_cookies`, with egress open, before the
manifest is removed) and systemd restarts it. The second run of the restore reads 0, has nothing
to write back, and the clone is published without SYN cookies for good; every snapshot of it
carries the 0 (38 steps). -/
def Run.restartCookies : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .kill, .restart, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent, .agent, .agent, .agent]

theorem breaks_restartCookies :
    after (replay Today.cookieB Runs.kills Run.restartCookies (init Runs.kills))
      (lastingChange Runs.kills) := by decide

theorem design_keeps_restartCookies :
    ¬ after (replay Design.rules Runs.kills Run.restartCookies (init Runs.kills))
      (lastingChange Runs.kills) := by decide

/-- fc-agent is killed after the retirement. The second run of the restore finds the egress half gone
and closes it again: the daemons whose connections were just retired reconnect into a reset (25
steps). -/
def Run.restartEgress : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .kill, .restart, .agent, .agent]

theorem breaks_restartEgress :
    after (replay Today.cookieB Runs.kills Run.restartEgress (init Runs.kills))
      workloadBehindEgress := by decide

theorem design_keeps_restartEgress :
    ¬ after (replay Design.rules Runs.kills Run.restartEgress (init Runs.kills))
      workloadBehindEgress := by decide

/-- fc-agent is killed after egress opened and a process connected out. The second run's list is taken
with that connection in it and retires it (34 steps). -/
def Run.restartRetires : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .conn .dmn, .kill, .restart, .agent, .agent, .agent, .agent, .agent, .agent, .agent, .agent,
   .agent, .agent, .agent]

theorem breaks_restartRetires :
    after (replay Today.cookieB Runs.kills Run.restartRetires (init Runs.kills))
      generationBroken := by decide

theorem design_keeps_restartRetires :
    ¬ after (replay Design.rules Runs.kills Run.restartRetires (init Runs.kills))
      generationBroken := by decide

/-- The prepare one-shot is killed inside the receive barrier: `/sys/kernel/rcu_expedited` stays 1 on
the source after the snapshot has failed and the gate is open again (7 steps). -/
def Run.barrierKill : List Label :=
  [.snap, .snap, .snap, .snap, .snapKill, .snap, .snap]

theorem breaks_barrierKill :
    after (replay Today.cookieB Runs.kills Run.barrierKill (init Runs.kills))
      sourceDisturbed := by decide

theorem design_keeps_barrierKill :
    ¬ after (replay Design.rules Runs.kills Run.barrierKill (init Runs.kills))
      sourceDisturbed := by decide

/-- A boundary held past the payload freeze's deadline: the watchdog thaws the container while both
halves of the clone's gate are closed (11 steps). -/
def Run.watchdogThaw : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .restore, .watchdog]

theorem breaks_watchdogThaw :
    after (replay Today.cookieB Runs.slow Run.watchdogThaw (init Runs.slow))
      workloadBehindEgress := by decide

theorem design_keeps_watchdogThaw :
    ¬ after (replay Design.rules Runs.slow Run.watchdogThaw (init Runs.slow))
      workloadBehindEgress := by decide

/-- The resume one-shot fails to open the gate. It removes the manifest, thaws, exits non-zero, and
nothing tries again: the source stays gated (11 steps). -/
def Run.reopenFails : List Label :=
  [.snap, .snap, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snapFail]

theorem breaks_reopenFails :
    after (replay Today.cookieB Runs.fails Run.reopenFails (init Runs.fails))
      sourceDisturbed := by decide

theorem design_keeps_reopenFails :
    ¬ after (replay Design.rules Runs.fails Run.reopenFails (init Runs.fails))
      sourceDisturbed := by decide

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
    ¬ after (replay Design.rules Runs.fails Run.writeBackFails (init Runs.fails))
      (lastingChange Runs.fails) := by decide

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

/-- The design does not change this. Its restore has one more step before the UDP rule. -/
def Run.replyDatagramDesign : List Label :=
  [.udpS, .snap, .snap, .snap, .snap, .snap, .snap,
   .snap, .snap, .snap, .restore, .agent, .agent, .agent, .agent, .udpR]

theorem design_too_replyDatagram :
    after (replay Design.rules Runs.udp Run.replyDatagramDesign (init Runs.udp))
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
