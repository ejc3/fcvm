import WakeVerify.Proofs
import WakeVerify.Breaks

/-!
# What each theorem rests on

`lake build` fails if a theorem about the repaired system, or a recorded break, comes to depend on
anything beyond Lean's own axioms (`propext`, `Classical.choice`, `Quot.sound`): no `sorry`, no
added axiom, no `native_decide` (which would add `Lean.ofReduceBool`).
-/

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
