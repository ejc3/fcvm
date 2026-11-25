import Lean
import WakeVerify.Proofs
import WakeVerify.Breaks

/-!
# What each theorem rests on

`lake build` fails if any theorem of the model comes to depend on anything beyond Lean's own
axioms (`propext`, `Classical.choice`, `Quot.sound`): no `sorry` (`sorryAx`), no added axiom, no
`native_decide` (which adds an axiom of its own for what the compiled code returned).

Two checks. The sweep below visits every theorem in the `Wake` namespace, so a theorem added later
is covered without being listed. The `#guard_msgs` lines after it pin the exact axioms of the
theorems the documents cite, so the build also fails when one of those is renamed or removed.
-/

open Lean Elab Command in
/-- Fails if a theorem under `Wake` depends on an axiom outside the allowed three, or if it finds
too few theorems, or not the ones it must (a sweep that inspects nothing must not pass). -/
elab "#check_wake_axioms" : command => do
  let env ← getEnv
  let allowed : List Name := [``propext, ``Quot.sound, ``Classical.choice]
  let theorems := env.constants.fold (init := #[]) fun acc name info =>
    match info with
    | .thmInfo _ => if (`Wake).isPrefixOf name then acc.push name else acc
    | _ => acc
  let mut bad : Array String := #[]
  for name in theorems do
    let axioms ← collectAxioms name
    let extra := axioms.toList.filter (!allowed.contains ·)
    if !extra.isEmpty then bad := bad.push s!"{name} depends on {extra}"
  if theorems.size < 100 || !theorems.contains `Wake.design_noLostKick
      || !theorems.contains `Wake.breaks_pageReady then
    throwError "the axiom sweep found {theorems.size} theorems under Wake and not the ones it must"
  if !bad.isEmpty then
    throwError "theorems resting on more than Lean's own axioms:\n{"\n".intercalate bad.toList}"

#check_wake_axioms

open Wake

/-- info: 'Wake.design_noLostKick' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noLostKick
/-- info: 'Wake.design_noHang' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noHang
/-- info: 'Wake.design_noStuckCall' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noStuckCall
/-- info: 'Wake.design_mutualExclusion' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_mutualExclusion
/-- info: 'Wake.design_noOrphanHalt' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noOrphanHalt
/-- info: 'Wake.design_noLostTimer' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noLostTimer
/-- info: 'Wake.design_noStrandedNode' depends on axioms: [propext, Quot.sound] -/
#guard_msgs in #print axioms design_noStrandedNode
/-- info: 'Wake.design_sound' depends on axioms: [propext] -/
#guard_msgs in #print axioms design_sound
/-- info: 'Wake.v614Kick_sound' depends on axioms: [propext] -/
#guard_msgs in #print axioms v614Kick_sound
/-- info: 'Wake.v614_sound_without_snapshots' depends on axioms: [propext] -/
#guard_msgs in #print axioms v614_sound_without_snapshots
/-- info: 'Wake.v733Patched_sound_without_snapshots' depends on axioms: [propext] -/
#guard_msgs in #print axioms v733Patched_sound_without_snapshots
/-- info: 'Wake.v733_not_keepsKick' depends on axioms: [propext] -/
#guard_msgs in #print axioms v733_not_keepsKick
/-- info: 'Wake.hlt_rechecks' depends on axioms: [propext] -/
#guard_msgs in #print axioms hlt_rechecks
/-- info: 'Wake.design_survives' depends on axioms: [propext] -/
#guard_msgs in #print axioms design_survives
/-- info: 'Wake.breaks_pageReady' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_pageReady
/-- info: 'Wake.breaks_snapshot' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_snapshot
/-- info: 'Wake.breaks_wakeAll' depends on axioms: [propext] -/
#guard_msgs in #print axioms breaks_wakeAll
