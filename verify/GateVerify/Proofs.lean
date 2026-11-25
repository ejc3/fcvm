import GateVerify.Lemmas

/-!
# The design keeps its properties, in every reachable state

For every configuration `C` (every combination of what exists around the guest and of the moves
the environment makes: clients, cookies, failures, kills, a dying host, slow boundaries,
snapshots asked early) and every state reachable from `init C` under `Design.rules`, by runs of
any length.

No `sorry`, and no axiom beyond Lean's own (`Axioms.lean` checks it on every build). The proofs
are inductive invariants (`Scal` and `Flags` in `Lemmas.lean`) over the same `step` the search
runs, not the search.

What is NOT proved here, and is only searched (`verify/README.md`): P2 (`generationBroken`) and
the first half of P3 (`staleAtPublication`). Both are about which sockets a retirement destroys
and leaves, and need an invariant over the socket list and the lists of cookies that this file
does not have.
-/

namespace Gate

set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

variable {C : Cfg} {s : State}

/-- **P1.** No established connection of a guest being snapshotted is reset, and once no boundary
is in progress the guest has no half of the gate, no manifest, and `rcu_expedited` as it was. -/
theorem design_sourceUndisturbed (hr : Reachable Design.rules C s) : sourceDisturbed s = false := by
  have hi := reachable_scal hr
  have hf := reachable_flags hr
  unfold sourceDisturbed
  cases hpw : s.pw
  · simp [hf.1]
  · have hi := hi hpw
    cases hrp : s.rp <;> cases hsp : s.sp <;>
      simp_all [SrcOk, State.gated, hf.1, inPrepare]

/-- **P3, second half.** No packet from outside makes a socket in a clone between its retirement
and its publication. -/
theorem design_noSmuggledLate (hr : Reachable Design.rules C s) : smuggledLate s = false :=
  (reachable_flags hr).2.2.2.2.1

/-- **P4.** No socket is retired with the egress half closed. -/
theorem design_egressBeforeRetirement (hr : Reachable Design.rules C s) :
    retiredBehindEgress s = false :=
  (reachable_flags hr).2.1

/-- **P4.** The hook runs with egress open and ingress closed. -/
theorem design_hookPlaced (hr : Reachable Design.rules C s) : hookMisplaced s = false :=
  (reachable_flags hr).2.2.1

/-- **P4.** While a clone's egress half is closed its container is frozen and nothing of it has
been retired. -/
theorem design_workloadEgress (hr : Reachable Design.rules C s) :
    workloadBehindEgress s = false := by
  have hi := reachable_scal hr
  unfold workloadBehindEgress
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    cases hrp : s.rp with
    | «at» n =>
      simp only [hrp] at hi
      have hn : n < 18 := hi.1
      rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
        | h0 | h0 | h0 | h0 <;> subst h0 <;> simp_all [CloneOk]
    | _ => simp

/-- **P5.** A published clone that no snapshot holds has `tcp_syncookies` as it was at boot, no
UDP rule and no record of a restore. -/
theorem design_noLastingChange (hr : Reachable Design.rules C s) : lastingChange C s = false := by
  have hi := reachable_scal hr
  unfold lastingChange
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    cases hrp : s.rp <;> simp_all [SrcOk]

/-- **P5.** No memory image is taken while a setting is changed. -/
theorem design_noCapturedChange (hr : Reachable Design.rules C s) : capturedChange s = false :=
  (reachable_flags hr).2.2.2.1

/-- **P6.** While a half of the gate is installed or the guest is paused, a step of the system
is enabled: the snapshot's next step, the guard's, the restore's, or the host's timeout. -/
theorem design_neverStuck (hr : Reachable Design.rules C s) : stuck Design.rules C s = false := by
  have hi := reachable_scal hr
  unfold stuck
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    cases hrp : s.rp with
    | «at» n =>
      have hw : s.waiting = true := by simp_all [CloneOk]
      simp [progressLabels, step, timeout, hpw, hw]
    | failed =>
      have hw : s.waiting = true := by simp_all [FailedOk]
      simp [progressLabels, step, timeout, hpw, hw]
    | none =>
      cases hsp : s.sp <;> cases hh : s.host <;>
        simp_all [SrcOk, State.gated, progressLabels, step, snap, lockRestore, Design.rules, inPrepare]
    | done =>
      cases hsp : s.sp <;> cases hh : s.host <;>
        simp_all [SrcOk, State.gated, progressLabels, step, snap, lockRestore, Design.rules, inPrepare]

/-- **U1.** Where the host keeps client datagrams from a clone that has not published, none
reaches one. -/
theorem design_noClientDatagram (hu : C.udpReach = false) (hr : Reachable Design.rules C s) :
    clientDatagram s = false :=
  (reachable_flags hr).2.2.2.2.2 hu

end Gate
