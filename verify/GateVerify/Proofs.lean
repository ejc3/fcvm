import GateVerify.Lemmas
import GateVerify.Socks
import GateVerify.Gens

/-!
# The design keeps its properties, in every reachable state

For every configuration `C` (every combination of what exists around the guest and of the moves
the environment makes: clients, cookies, failures, kills, a dying host, slow boundaries,
snapshots asked early) and every state reachable from `init C` under `Design.rules`, by runs of
any length.

No `sorry`, and no axiom beyond Lean's own (`Axioms.lean` checks it on every build). The proofs
are inductive invariants (`Scal` and `Flags` in `Lemmas.lean`, `Sk` in `Socks.lean`, `Gn` in
`Gens.lean`) over the same `step` the search runs, not the search.

P6 has two more theorems in `Ends.lean`: a measure every step of the system makes smaller
(`design_bounded`), and where a run ends (`design_settles`).

P2 is proved for the connections `step` can make. One it cannot make is a connection inside the
guest with one end freed and the other kept; `Limits.lean` has what follows from one.
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

/-- **P2.** No retirement destroys a socket a process of the clone opened, a socket a packet made
with the gate open, or an end of a connection inside the guest whose other end stays. -/
theorem design_fixedGeneration (hr : Reachable Design.rules C s) : generationBroken s = false :=
  (reachable_gn hr).gB

/-- **P2, without the record.** When the restoring agent is at its retirement, every socket the
retirement names, by cookie, in the manifest or on the restore's own list, is one it must
destroy: it was in the image, or a packet made it behind the closed gate; one the manifest does
not name has no mirror image among the guest's sockets; and one the manifest names was in the
image. -/
theorem design_retiresOnly (hr : Reachable Design.rules C s) (h11 : s.rp = .at 11)
    (ha : s.ag = true) :
    ∀ x ∈ s.socks, x.id ∈ s.man.getD [] ++ s.lst →
      (x.gen = .stale ∨ x.gen = .smuggled)
      ∧ (x.id ∈ s.man.getD [] → x.gen = .stale)
      ∧ (x.id ∉ s.man.getD [] → hasMirror s.socks x = false) := by
  have hg := reachable_gn hr
  intro x hx hin
  have hM : x.id ∈ s.man.getD [] → x.gen = .stale := hg.gM 11 h11 x hx
  have hL : x.id ∈ s.lst → x.gen ≠ .own ∧ x.gen ≠ .fresh ∧ hasMirror s.socks x = false :=
    hg.gL 11 h11 (by omega) (by omega) ha x hx
  rw [List.mem_append] at hin
  refine ⟨?_, hM, ?_⟩
  · rcases hin with h | h
    · exact Or.inl (hM h)
    · obtain ⟨h1, h2, -⟩ := hL h
      cases hgen : x.gen <;> simp_all
  · intro hn
    rcases hin with h | h
    · exact absurd h hn
    · exact (hL h).2.2

/-- **P3, first half.** Once a restored guest's ingress is open in either family, it holds no
socket with its other end outside the guest that was in the image or made behind the closed
gate. -/
theorem design_noStaleAtPublication (hr : Reachable Design.rules C s) :
    staleAtPublication s = false := by
  have hi := reachable_scal hr
  have hk := reachable_sk hr
  unfold staleAtPublication
  cases hpw : s.pw
  · simp
  · have hi := hi hpw
    -- with ingress open, the retirement is behind
    have key : s.cut = true →
        (s.socks.any fun x => x.kind.outside && (x.gen == .stale || x.gen == .smuggled)) = false := by
      intro hc
      rw [List.any_eq_false]
      intro x hx hbad
      simp only [Bool.and_eq_true, Bool.or_eq_true, beq_iff_eq] at hbad
      exact hk.k3 hc x hx hbad.1 hbad.2
    cases hrp : s.rp with
    | none => simp
    | done =>
      have hc : s.cut = true := by simp_all [SrcOk]
      simp [key hc]
    | failed =>
      cases hc : s.cut
      · have : s.in4 = true ∧ s.in6 = true := by simp_all [FailedOk]
        simp [this.1, this.2]
      · simp [key hc]
    | «at» n =>
      simp only [hrp] at hi
      cases hc : s.cut
      · have hn : n < 18 := hi.1
        have : s.in4 = true ∧ s.in6 = true := by
          rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
            | h0 | h0 | h0 | h0 | h0 <;> subst h0 <;> simp_all [CloneOk]
        simp [this.1, this.2]
      · simp [key hc]

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
