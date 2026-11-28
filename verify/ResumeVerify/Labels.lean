import ResumeVerify.Spec

/-!
# Every step there is

The list the search expands at each state, and that it leaves no step out. A constructor added
to `Label` or `GAct` and not to `allLabels` fails `labels_complete`, so the search cannot
silently skip a step the proofs cover.
-/

namespace Resume

def vids : List Vid := [false, true]

def allLabels : List Label :=
  (vids.flatMap fun v =>
      ([GAct.arm, .toIdle, .wfi, .ack .tmr, .ack .sgi, .ack .spi, .eoi, .send, .l2].map (Label.g v))
        ++ [Label.fire v, Label.wake v])
    ++ ([.dev, .tick, .wait, .pause, .save, .resume, .restore] : List Label)

theorem labels_complete : ∀ l : Label, l ∈ allLabels := by
  intro l
  cases l with
  | g v a => cases v <;> cases a <;> first | decide | (rename_i i; cases i <;> decide)
  | fire v => cases v <;> decide
  | wake v => cases v <;> decide
  | _ => decide

end Resume
