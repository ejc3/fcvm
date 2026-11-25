import Lean
import GateVerify.Proofs
import GateVerify.Ends
import GateVerify.Limits
import GateVerify.Breaks

/-!
# What each theorem rests on

`lake build` fails if any theorem of this model comes to depend on anything beyond Lean's own
axioms (`propext`, `Classical.choice`, `Quot.sound`): no `sorry` (`sorryAx`), no added axiom, no
`native_decide` (which adds an axiom of its own for what the compiled code returned).

Two checks, as for the wake-up model. The sweep visits every theorem in the `Gate` namespace, so
a theorem added later is covered without being listed. The `#guard_msgs` lines pin the exact
axioms of the theorems the documents cite, so the build also fails when one of those is renamed
or removed.
-/

open Lean Elab Command in
/-- Fails if a theorem under `Gate` depends on an axiom outside the allowed three, or if it finds
too few theorems, or not the ones it must (a sweep that inspects nothing must not pass). -/
elab "#check_gate_axioms" : command => do
  let env ← getEnv
  let allowed : List Name := [``propext, ``Quot.sound, ``Classical.choice]
  let theorems := env.constants.fold (init := #[]) fun acc name info =>
    match info with
    | .thmInfo _ => if (`Gate).isPrefixOf name then acc.push name else acc
    | _ => acc
  let mut bad : Array String := #[]
  for name in theorems do
    let axioms ← collectAxioms name
    let extra := axioms.toList.filter (!allowed.contains ·)
    if !extra.isEmpty then bad := bad.push s!"{name} depends on {extra}"
  if theorems.size < 250 || !theorems.contains `Gate.design_neverStuck
      || !theorems.contains `Gate.design_noStaleAtPublication
      || !theorems.contains `Gate.design_fixedGeneration || !theorems.contains `Gate.design_bounded
      || !theorems.contains `Gate.design_settles || !theorems.contains `Gate.loneEndRetired
      || !theorems.contains `Gate.reachable_scal || !theorems.contains `Gate.breaks_cookieAck then
    throwError "the axiom sweep found {theorems.size} theorems under Gate and not the ones it must"
  if !bad.isEmpty then
    throwError "theorems resting on more than Lean's own axioms:\n{"\n".intercalate bad.toList}"

#check_gate_axioms

open Gate

/-- info: 'Gate.design_sourceUndisturbed' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_sourceUndisturbed
/-- info: 'Gate.design_noStaleAtPublication' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms design_noStaleAtPublication
/-- info: 'Gate.reachable_sk' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms reachable_sk
/-- info: 'Gate.design_fixedGeneration' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms design_fixedGeneration
/-- info: 'Gate.design_retiresOnly' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms design_retiresOnly
/-- info: 'Gate.reachable_gn' depends on axioms: [propext, Classical.choice, Quot.sound] -/
#guard_msgs in #print axioms reachable_gn
/-- info: 'Gate.own_decreases' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms own_decreases
/-- info: 'Gate.design_bounded' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_bounded
/-- info: 'Gate.design_ends' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_ends
/-- info: 'Gate.design_settles' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_settles
/-- info: 'Gate.kill_restart_repeats' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms kill_restart_repeats
/-- info: 'Gate.loneEndRetired' depends on axioms: [propext] -/
#guard_msgs in #print axioms loneEndRetired
/-- info: 'Gate.design_noSmuggledLate' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noSmuggledLate
/-- info: 'Gate.design_egressBeforeRetirement' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_egressBeforeRetirement
/-- info: 'Gate.design_hookPlaced' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_hookPlaced
/-- info: 'Gate.design_workloadEgress' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_workloadEgress
/-- info: 'Gate.design_noLastingChange' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noLastingChange
/-- info: 'Gate.design_noCapturedChange' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noCapturedChange
/-- info: 'Gate.design_neverStuck' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_neverStuck
/-- info: 'Gate.design_noClientDatagram' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noClientDatagram
/-- info: 'Gate.reachable_scal' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms reachable_scal
/-- info: 'Gate.reachable_flags' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms reachable_flags
/-- info: 'Gate.breaks_staleTuple' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_staleTuple
/-- info: 'Gate.breaks_cookieAck' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_cookieAck
/-- info: 'Gate.breaks_earlySnapshot' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_earlySnapshot
/-- info: 'Gate.breaks_restartCookies' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_restartCookies
/-- info: 'Gate.failedCloneSnapshotEnds' depends on axioms: [propext] -/
#guard_msgs in #print axioms failedCloneSnapshotEnds
