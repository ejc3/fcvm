import ResumeVerify.Invariant

set_option linter.unusedSimpArgs false

/-!
# The guest's steps on vCPU 1 keep the invariant

One case for each place the guest CPU can be and each thing it can do there. `Guest0.lean` and
`Guest1.lean` are the same proof for the two vCPUs, in two files so that they build side by
side.
-/

namespace Resume

set_option maxHeartbeats 3200000 in
theorem guest_ok1 {C : Cfg} {m m' : VM} {a : GAct} (h : MOk C m)
    (hs : guest C true a m = some m') : MOk C m' := by
  obtain ⟨v0, v1, pa, p1, pl, pn, gi, pe⟩ := h
  cases v0
  cases v1
  rcases hg : m.c1.g with _ | _ | (_ | _ | _) <;> cases a
  case ack i | ack i | ack i | ack i | ack i =>
    all_goals cases i <;>
      simp only [guest, VM.get_false, VM.get_true, VM.set_false, VM.set_true, Bool.not_true,
        Bool.not_false, hg] at hs <;> peel <;> first | (simp at hs; done) | finish
  all_goals
    simp only [guest, VM.get_false, VM.get_true, VM.set_false, VM.set_true, Bool.not_true,
      Bool.not_false, hg] at hs <;> peel <;> first | (simp at hs; done) | finish

theorem guest_ctr1 {C : Cfg} {m m' : VM} {a : GAct} (hs : guest C true a m = some m') :
    m'.ctr = m.ctr := by
  rcases hg : m.c1.g with _ | _ | (_ | _ | _) <;> cases a
  case ack i | ack i | ack i | ack i | ack i =>
    all_goals cases i <;>
      simp only [guest, VM.get_false, VM.get_true, VM.set_false, VM.set_true, Bool.not_true,
        Bool.not_false, hg] at hs <;> peel <;>
      first | (simp at hs; done) | (simp only [Option.some.injEq] at hs; subst hs; rfl)
  all_goals
    simp only [guest, VM.get_false, VM.get_true, VM.set_false, VM.set_true, Bool.not_true,
      Bool.not_false, hg] at hs <;> peel <;>
    first | (simp at hs; done) | (simp only [Option.some.injEq] at hs; subst hs; rfl)

end Resume
