import WakeVerify.Properties
import WakeVerify.Today
import WakeVerify.Design

/-!
# Today's rules, broken: runs the search found, checked by the kernel

Copied from the output of `lake exe breaks`: for each kind of break listed in `verify/BREAKS.md`, a
shortest run from `init` after which a property fails. `by decide` replays the run and evaluates
the property in the kernel, so each is a checked fact about the model, not a printout: a run that
stops being possible, or stops breaking its property, fails the build (`breaks` is false for a run
that cannot be replayed). That a run is a shortest one is the search's output and is not proved.

`.vcpu .a` is vCPU 0 and `.vcpu .b` is vCPU 1. `.step` is the next instruction of the path the vCPU
is on.
-/

namespace Wake

set_option maxRecDepth 100000

/-- The fcvm configuration with the guest booted `no-kvmapf`. -/
def noKvmapf : Cfg := { Cfg.fcvm with pvApf := false }

/-- A VMM that writes `mp_state` back on a paused, live VM. -/
def writesBack : Cfg := { Cfg.fcvm with userSetMp := true }

/-- The idle loop programs its timer, in a guest booted `no-kvmapf`. -/
def timerNoApf : Cfg := { Cfg.fcvm with timer := true, pvApf := false }

/-- A page-ready completion clears the kick; at a queue head, interrupts off: the hang (15
steps). -/
def Run.pageReady : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultPv, .vcpu .b .lockIrq,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_pageReady :
    breaks (replay Today.v733 Cfg.fcvm Run.pageReady init) NoHang := by decide

/-- The snapshot does not hold the kick; at a queue head, interrupts off: the hang (16 steps). -/
def Run.snapshot : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_snapshot :
    breaks (replay Today.v733 Cfg.fcvm Run.snapshot init) NoHang := by decide

/-- The completion of a synthetic halt clears the kick; at a queue head, interrupts off: the
hang (17 steps). -/
def Run.synthHalt : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultSynth, .vcpu .b .extIrq,
   .vcpu .b .hostRun, .vcpu .b .lockIrq, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone,
   .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_synthHalt :
    breaks (replay Today.v733 Cfg.fcvm Run.synthHalt init) NoHang := by decide

/-- A page-ready completion clears the kick; at a queue node, interrupts off: the hang (23
steps). -/
def Run.pageReadyNode : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultPv, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .apfDone, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_pageReadyNode :
    breaks (replay Today.v733 Cfg.fcvm Run.pageReadyNode init) NoHang := by decide

/-- The snapshot does not hold the kick; at a queue node, interrupts off: the hang (24 steps). -/
def Run.snapshotNode : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_snapshotNode :
    breaks (replay Today.v733 Cfg.fcvm Run.snapshotNode init) NoHang := by decide

/-- The completion of a synthetic halt clears the kick; at a queue node, interrupts off: the
hang (25 steps). -/
def Run.synthHaltNode : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultSynth, .vcpu .a .extIrq, .vcpu .a .hostRun, .vcpu .a .lockIrq,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .apfDone, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_synthHaltNode :
    breaks (replay Today.v733 Cfg.fcvm Run.synthHaltNode init) NoHang := by decide

/-- The completion of a synthetic halt clears the kick; at a queue head, lock taken with
interrupts on: the sleeper waits for its next interrupt (17 steps). -/
def Run.synthHaltIrqsOn : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultSynth, .vcpu .b .extIrq,
   .vcpu .b .hostRun, .vcpu .b .lockPlain, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone,
   .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_synthHaltIrqsOn :
    breaks (replay Today.v733 Cfg.fcvm Run.synthHaltIrqsOn init) NoLostKick := by decide

/-- The completion of a synthetic halt clears the kick; at a queue node, lock taken with
interrupts on: the sleeper waits for its next interrupt (25 steps). -/
def Run.synthHaltNodeIrqsOn : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultSynth, .vcpu .a .extIrq, .vcpu .a .hostRun,
   .vcpu .a .lockPlain, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .apfDone, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun,
   .vcpu .a .step]

theorem breaks_synthHaltNodeIrqsOn :
    breaks (replay Today.v733 Cfg.fcvm Run.synthHaltNodeIrqsOn init) NoLostKick := by decide

/-- A page-ready completion clears the kick; then the other vCPU waits for ever in a cross-CPU
call to the sleeper (queue head) (17 steps). -/
def Run.pageReadyCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultPv, .vcpu .b .lockIrq,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .call, .vcpu .a .step, .vcpu .b .apfDone,
   .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_pageReadyCall :
    breaks (replay Today.v733 Cfg.fcvm Run.pageReadyCall init) NoStuckCall := by decide

/-- The snapshot does not hold the kick; then the other vCPU waits for ever in a cross-CPU call
to the sleeper (queue head) (18 steps). -/
def Run.snapshotCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .call, .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun,
   .vcpu .b .step]

theorem breaks_snapshotCall :
    breaks (replay Today.v733 Cfg.fcvm Run.snapshotCall init) NoStuckCall := by decide

/-- The completion of a synthetic halt clears the kick; then the other vCPU waits for ever in a
cross-CPU call to the sleeper (queue head) (19 steps). -/
def Run.synthHaltCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultSynth, .vcpu .b .extIrq,
   .vcpu .b .hostRun, .vcpu .b .lockIrq, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .call,
   .vcpu .a .step, .vcpu .b .apfDone, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_synthHaltCall :
    breaks (replay Today.v733 Cfg.fcvm Run.synthHaltCall init) NoStuckCall := by decide

/-- A page-ready completion clears the kick; then the other vCPU waits for ever in a cross-CPU
call to the sleeper (queue node) (25 steps). -/
def Run.pageReadyNodeCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultPv, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .apfDone, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step, .vcpu .b .call, .vcpu .b .step]

theorem breaks_pageReadyNodeCall :
    breaks (replay Today.v733 Cfg.fcvm Run.pageReadyNodeCall init) NoStuckCall := by decide

/-- The snapshot does not hold the kick; then the other vCPU waits for ever in a cross-CPU call
to the sleeper (queue node) (26 steps). -/
def Run.snapshotNodeCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .call,
   .vcpu .b .step, .pause, .clone, .resume, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_snapshotNodeCall :
    breaks (replay Today.v733 Cfg.fcvm Run.snapshotNodeCall init) NoStuckCall := by decide

/-- The completion of a synthetic halt clears the kick; then the other vCPU waits for ever in a
cross-CPU call to the sleeper (queue node) (27 steps). -/
def Run.synthHaltNodeCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultSynth, .vcpu .a .extIrq, .vcpu .a .hostRun, .vcpu .a .lockIrq,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .apfDone, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step,
   .vcpu .b .call, .vcpu .b .step]

theorem breaks_synthHaltNodeCall :
    breaks (replay Today.v733 Cfg.fcvm Run.synthHaltNodeCall init) NoStuckCall := by decide

/-- With no restore kick at all: the restore's wake-everyone completion clears a kick sent in
the restored VM (the #2125 defect, another completion); at a queue head, interrupts off: the
hang (34 steps). -/
def Run.wakeAllToday : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume, .vcpu .a .hostRun, .vcpu .b .hostRun,
   .pause, .clone, .resume, .vcpu .a .hostRun, .vcpu .a .step, .vcpu .a .irqApf,
   .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .b .hostRun, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun,
   .vcpu .a .step]

theorem breaks_wakeAllToday :
    breaks (replay Today.v733 Cfg.fcvm Run.wakeAllToday init) NoHang := by decide

/-- With no restore kick at all: the restore's wake-everyone completion clears a kick sent in
the restored VM (the #2125 defect, another completion); at a queue node, interrupts off: the
hang (37 steps). -/
def Run.wakeAllNodeToday : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .pause,
   .clone, .resume, .vcpu .a .hostRun, .vcpu .b .hostRun, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step, .vcpu .a .irqApf, .vcpu .a .lockIrq, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .hostRun,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_wakeAllNodeToday :
    breaks (replay Today.v733 Cfg.fcvm Run.wakeAllNodeToday init) NoHang := by decide

/-- With no restore kick at all: the restore's wake-everyone completion clears a kick sent in
the restored VM (the #2125 defect, another completion); then the other vCPU waits for ever in a
cross-CPU call to the sleeper (queue head) (36 steps). -/
def Run.wakeAllCallToday : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume, .vcpu .a .hostRun, .vcpu .b .hostRun,
   .pause, .clone, .resume, .vcpu .a .hostRun, .vcpu .a .step, .vcpu .a .irqApf,
   .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .b .hostRun, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun,
   .vcpu .a .step, .vcpu .b .call, .vcpu .b .step]

theorem breaks_wakeAllCallToday :
    breaks (replay Today.v733 Cfg.fcvm Run.wakeAllCallToday init) NoStuckCall := by decide

/-- With no restore kick at all: the restore's wake-everyone completion clears a kick sent in
the restored VM (the #2125 defect, another completion); then the other vCPU waits for ever in a
cross-CPU call to the sleeper (queue node) (39 steps). -/
def Run.wakeAllNodeCallToday : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .pause,
   .clone, .resume, .vcpu .a .hostRun, .vcpu .b .hostRun, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step, .vcpu .a .irqApf, .vcpu .a .lockIrq, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .hostRun,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step, .vcpu .b .call,
   .vcpu .b .step]

theorem breaks_wakeAllNodeCallToday :
    breaks (replay Today.v733 Cfg.fcvm Run.wakeAllNodeCallToday init) NoStuckCall := by decide

/-- Guest booted `no-kvmapf`: the snapshot does not hold the kick; at a queue head, interrupts
off: the hang (16 steps). -/
def Run.snapshotNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_snapshotNoApf :
    breaks (replay Today.v733 noKvmapf Run.snapshotNoApf init) NoHang := by decide

/-- Guest booted `no-kvmapf`: the completion of a synthetic halt clears the kick; at a queue
head, interrupts off: the hang (17 steps). -/
def Run.synthHaltNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultSynth, .vcpu .b .extIrq,
   .vcpu .b .hostRun, .vcpu .b .lockIrq, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone,
   .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_synthHaltNoApf :
    breaks (replay Today.v733 noKvmapf Run.synthHaltNoApf init) NoHang := by decide

/-- Guest booted `no-kvmapf`: the snapshot does not hold the kick; at a queue node, interrupts
off: the hang (24 steps). -/
def Run.snapshotNodeNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_snapshotNodeNoApf :
    breaks (replay Today.v733 noKvmapf Run.snapshotNodeNoApf init) NoHang := by decide

/-- Guest booted `no-kvmapf`: the completion of a synthetic halt clears the kick; at a queue
node, interrupts off: the hang (25 steps). -/
def Run.synthHaltNodeNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultSynth, .vcpu .a .extIrq, .vcpu .a .hostRun, .vcpu .a .lockIrq,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .apfDone, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_synthHaltNodeNoApf :
    breaks (replay Today.v733 noKvmapf Run.synthHaltNodeNoApf init) NoHang := by decide

/-- Guest booted `no-kvmapf`: the snapshot does not hold the kick; at a queue head, lock taken
with interrupts on: the sleeper waits for its next interrupt (16 steps). -/
def Run.snapshotIrqsOnNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockPlain, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_snapshotIrqsOnNoApf :
    breaks (replay Today.v733 noKvmapf Run.snapshotIrqsOnNoApf init) NoLostKick := by decide

/-- Guest booted `no-kvmapf`: the completion of a synthetic halt clears the kick; at a queue
head, lock taken with interrupts on: the sleeper waits for its next interrupt (17 steps). -/
def Run.synthHaltIrqsOnNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultSynth, .vcpu .b .extIrq,
   .vcpu .b .hostRun, .vcpu .b .lockPlain, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone,
   .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_synthHaltIrqsOnNoApf :
    breaks (replay Today.v733 noKvmapf Run.synthHaltIrqsOnNoApf init) NoLostKick := by decide

/-- Guest booted `no-kvmapf`: the snapshot does not hold the kick; at a queue node, lock taken
with interrupts on: the sleeper waits for its next interrupt (24 steps). -/
def Run.snapshotNodeIrqsOnNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockPlain, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_snapshotNodeIrqsOnNoApf :
    breaks (replay Today.v733 noKvmapf Run.snapshotNodeIrqsOnNoApf init) NoLostKick := by decide

/-- Guest booted `no-kvmapf`: the completion of a synthetic halt clears the kick; at a queue
node, lock taken with interrupts on: the sleeper waits for its next interrupt (25 steps). -/
def Run.synthHaltNodeIrqsOnNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultSynth, .vcpu .a .extIrq, .vcpu .a .hostRun,
   .vcpu .a .lockPlain, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .apfDone, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun,
   .vcpu .a .step]

theorem breaks_synthHaltNodeIrqsOnNoApf :
    breaks (replay Today.v733 noKvmapf Run.synthHaltNodeIrqsOnNoApf init) NoLostKick := by decide

/-- Guest booted `no-kvmapf`: the snapshot does not hold the kick; then the other vCPU waits for
ever in a cross-CPU call to the sleeper (queue head) (18 steps). -/
def Run.snapshotCallNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .call, .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun,
   .vcpu .b .step]

theorem breaks_snapshotCallNoApf :
    breaks (replay Today.v733 noKvmapf Run.snapshotCallNoApf init) NoStuckCall := by decide

/-- Guest booted `no-kvmapf`: the completion of a synthetic halt clears the kick; then the other
vCPU waits for ever in a cross-CPU call to the sleeper (queue head) (19 steps). -/
def Run.synthHaltCallNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultSynth, .vcpu .b .extIrq,
   .vcpu .b .hostRun, .vcpu .b .lockIrq, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .call,
   .vcpu .a .step, .vcpu .b .apfDone, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_synthHaltCallNoApf :
    breaks (replay Today.v733 noKvmapf Run.synthHaltCallNoApf init) NoStuckCall := by decide

/-- Guest booted `no-kvmapf`: the snapshot does not hold the kick; then the other vCPU waits for
ever in a cross-CPU call to the sleeper (queue node) (26 steps). -/
def Run.snapshotNodeCallNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .call,
   .vcpu .b .step, .pause, .clone, .resume, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_snapshotNodeCallNoApf :
    breaks (replay Today.v733 noKvmapf Run.snapshotNodeCallNoApf init) NoStuckCall := by decide

/-- Guest booted `no-kvmapf`: the completion of a synthetic halt clears the kick; then the other
vCPU waits for ever in a cross-CPU call to the sleeper (queue node) (27 steps). -/
def Run.synthHaltNodeCallNoApf : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .faultSynth, .vcpu .a .extIrq, .vcpu .a .hostRun, .vcpu .a .lockIrq,
   .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .apfDone, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .hostRun, .vcpu .a .step,
   .vcpu .b .call, .vcpu .b .step]

theorem breaks_synthHaltNodeCallNoApf :
    breaks (replay Today.v733 noKvmapf Run.synthHaltNodeCallNoApf init) NoStuckCall := by decide

/-- V6.14: the snapshot does not hold the kick; at a queue head, interrupts off: the hang (16
steps). -/
def Run.snapshot614 : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_snapshot614 :
    breaks (replay Today.v614 Cfg.fcvm Run.snapshot614 init) NoHang := by decide

/-- V6.14: the snapshot does not hold the kick; at a queue node, interrupts off: the hang (24
steps). -/
def Run.snapshotNode614 : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_snapshotNode614 :
    breaks (replay Today.v614 Cfg.fcvm Run.snapshotNode614 init) NoHang := by decide

/-- V6.14: the snapshot does not hold the kick; then the other vCPU waits for ever in a
cross-CPU call to the sleeper (queue head) (18 steps). -/
def Run.snapshotCall614 : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .call, .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun,
   .vcpu .b .step]

theorem breaks_snapshotCall614 :
    breaks (replay Today.v614 Cfg.fcvm Run.snapshotCall614 init) NoStuckCall := by decide

/-- V6.14: the snapshot does not hold the kick; then the other vCPU waits for ever in a
cross-CPU call to the sleeper (queue node) (26 steps). -/
def Run.snapshotNodeCall614 : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .call,
   .vcpu .b .step, .pause, .clone, .resume, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_snapshotNodeCall614 :
    breaks (replay Today.v614 Cfg.fcvm Run.snapshotNodeCall614 init) NoStuckCall := by decide

/-- The restore's wake-everyone completion clears the restore kick; at a queue head, interrupts
off: the hang (16 steps). -/
def Run.wakeAll : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_wakeAll :
    breaks (replay Partial.v733Kick Cfg.fcvm Run.wakeAll init) NoHang := by decide

/-- The restore's wake-everyone completion clears the restore kick; at a queue node, interrupts
off: the hang (24 steps). -/
def Run.wakeAllNode : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .pause, .clone, .resume,
   .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_wakeAllNode :
    breaks (replay Partial.v733Kick Cfg.fcvm Run.wakeAllNode init) NoHang := by decide

/-- The restore's wake-everyone completion clears the restore kick; then the other vCPU waits
for ever in a cross-CPU call to the sleeper (queue head) (18 steps). -/
def Run.wakeAllCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .call, .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun,
   .vcpu .b .step]

theorem breaks_wakeAllCall :
    breaks (replay Partial.v733Kick Cfg.fcvm Run.wakeAllCall init) NoStuckCall := by decide

/-- The restore's wake-everyone completion clears the restore kick; then the other vCPU waits
for ever in a cross-CPU call to the sleeper (queue node) (26 steps). -/
def Run.wakeAllNodeCall : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .a .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .call,
   .vcpu .b .step, .pause, .clone, .resume, .vcpu .a .hostRun, .vcpu .a .step]

theorem breaks_wakeAllNodeCall :
    breaks (replay Partial.v733Kick Cfg.fcvm Run.wakeAllNodeCall init) NoStuckCall := by decide

/-- V7.3.3 with the #2130 patch alone: the snapshot does not hold the kick; at a queue head,
interrupts off: the hang (16 steps). -/
def Run.snapshotPatched : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_snapshotPatched :
    breaks (replay Partial.v733Patched Cfg.fcvm Run.snapshotPatched init) NoHang := by decide

/-- A write-back of `mp_state` clears the kick; at a queue head, interrupts off: the hang (16
steps). -/
def Run.writeBack : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .vcpu .b .setMpSame, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_writeBack :
    breaks (replay Design.rules writesBack Run.writeBack init) NoHang := by decide

/-- V7.3.3: a vCPU asleep in `kvm_wait` with no kick and nobody ahead of it to send one. -/
def Run.noWaker : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .faultPv, .vcpu .b .lockIrq,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step,
   .vcpu .a .step, .vcpu .a .step, .vcpu .b .apfDone, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_noWaker :
    breaks (replay Today.v733 Cfg.fcvm Run.noWaker init) HasWaker := by decide

/-- Order: a restore kick sent before `KVM_SET_MP_STATE` is cleared by it on v6.15 and later. -/
def Run.kickFirst : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .b .step, .vcpu .a .step,
   .vcpu .a .step, .pause, .clone, .resume, .vcpu .b .hostRun, .vcpu .b .step]

theorem breaks_kickFirst :
    breaks (replay Order.kickFirst Cfg.fcvm Run.kickFirst init) NoHang := by decide

/-- Order: `MSR_IA32_TSC_DEADLINE` written before `KVM_SET_LAPIC` is dropped, and an idle vCPU
of a guest without async page faults sleeps for a timer it no longer has. -/
def Run.msrsFirst : List Label :=
  [.vcpu .a .idle, .vcpu .a .idArm, .vcpu .a .step, .pause, .clone]

theorem breaks_msrsFirst :
    breaks (replay Order.msrsFirst timerNoApf Run.msrsFirst init) NoLostTimer := by decide

/-- Order: without `KVM_SET_LAPIC` a pending call-function interrupt is not restored. -/
def Run.noLapic : List Label :=
  [.vcpu .a .call, .vcpu .a .step, .pause, .clone]

theorem breaks_noLapic :
    breaks (replay Order.noLapic Cfg.fcvm Run.noLapic init) NoStuckCall := by decide

/-- The 15-step #2125 run also breaks the rule book that has the restore kick and not the #2130
patch: the run of #2132 is the shortest of its own kind there, not the shortest hang. -/
theorem breaks_pageReady_with_kick :
    breaks (replay Partial.v733Kick Cfg.fcvm Run.pageReady init) NoHang := by decide

/-- The repaired system on the same runs: each run is possible under the design, and every vCPU
is fine after it (the proofs say so for every run; these are the particular ones, replayed). -/
theorem design_survives :
    after (replay Design.rules Cfg.fcvm Run.pageReady init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.pageReadyNode init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.pageReadyCall init) NoStuckCall ∧
    after (replay Design.rules Cfg.fcvm Run.snapshot init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.snapshotNode init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.wakeAll init) NoLostKick ∧
    after (replay Design.rules Cfg.fcvm Run.wakeAll init) HasWaker ∧
    after (replay Design.rules noKvmapf Run.synthHaltNoApf init) NoLostKick := by decide

/-- v6.14 on the run that breaks v7.3.3: the run is possible and the kick survives the
completion. -/
theorem v614_survives_pageReady :
    after (replay Today.v614 Cfg.fcvm Run.pageReady init) NoLostKick := by decide

/-- The design's theorems are not empty of cases: under the design a vCPU does sleep in `kvm_wait`
with its lock byte released, and what then holds is that its kick is on the way. -/
def Run.sleeperDue : List Label :=
  [.vcpu .a .lockIrq, .vcpu .a .step, .vcpu .a .step, .vcpu .b .lockIrq, .vcpu .b .step,
   .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .b .step, .vcpu .a .step, .vcpu .a .step]

theorem design_reaches_a_due_sleeper :
    after (replay Design.rules Cfg.fcvm Run.sleeperDue init)
      (fun s => s.due .b = true ∧ s.nothingPending .b = false) := by decide

/-- Every recorded run with its rule book and configuration, for the C harness
(`lake exe hostops`). -/
def recordedRuns : List (Rules × Cfg × List Label) :=
  [ (Today.v733, Cfg.fcvm, Run.pageReady),
    (Today.v733, Cfg.fcvm, Run.snapshot),
    (Today.v733, Cfg.fcvm, Run.synthHalt),
    (Today.v733, Cfg.fcvm, Run.pageReadyNode),
    (Today.v733, Cfg.fcvm, Run.snapshotNode),
    (Today.v733, Cfg.fcvm, Run.synthHaltNode),
    (Today.v733, Cfg.fcvm, Run.synthHaltIrqsOn),
    (Today.v733, Cfg.fcvm, Run.synthHaltNodeIrqsOn),
    (Today.v733, Cfg.fcvm, Run.pageReadyCall),
    (Today.v733, Cfg.fcvm, Run.snapshotCall),
    (Today.v733, Cfg.fcvm, Run.synthHaltCall),
    (Today.v733, Cfg.fcvm, Run.pageReadyNodeCall),
    (Today.v733, Cfg.fcvm, Run.snapshotNodeCall),
    (Today.v733, Cfg.fcvm, Run.synthHaltNodeCall),
    (Today.v733, Cfg.fcvm, Run.wakeAllToday),
    (Today.v733, Cfg.fcvm, Run.wakeAllNodeToday),
    (Today.v733, Cfg.fcvm, Run.wakeAllCallToday),
    (Today.v733, Cfg.fcvm, Run.wakeAllNodeCallToday),
    (Today.v733, noKvmapf, Run.snapshotNoApf),
    (Today.v733, noKvmapf, Run.synthHaltNoApf),
    (Today.v733, noKvmapf, Run.snapshotNodeNoApf),
    (Today.v733, noKvmapf, Run.synthHaltNodeNoApf),
    (Today.v733, noKvmapf, Run.snapshotIrqsOnNoApf),
    (Today.v733, noKvmapf, Run.synthHaltIrqsOnNoApf),
    (Today.v733, noKvmapf, Run.snapshotNodeIrqsOnNoApf),
    (Today.v733, noKvmapf, Run.synthHaltNodeIrqsOnNoApf),
    (Today.v733, noKvmapf, Run.snapshotCallNoApf),
    (Today.v733, noKvmapf, Run.synthHaltCallNoApf),
    (Today.v733, noKvmapf, Run.snapshotNodeCallNoApf),
    (Today.v733, noKvmapf, Run.synthHaltNodeCallNoApf),
    (Today.v614, Cfg.fcvm, Run.snapshot614),
    (Today.v614, Cfg.fcvm, Run.snapshotNode614),
    (Today.v614, Cfg.fcvm, Run.snapshotCall614),
    (Today.v614, Cfg.fcvm, Run.snapshotNodeCall614),
    (Partial.v733Kick, Cfg.fcvm, Run.wakeAll),
    (Partial.v733Kick, Cfg.fcvm, Run.wakeAllNode),
    (Partial.v733Kick, Cfg.fcvm, Run.wakeAllCall),
    (Partial.v733Kick, Cfg.fcvm, Run.wakeAllNodeCall),
    (Partial.v733Patched, Cfg.fcvm, Run.snapshotPatched),
    (Design.rules, writesBack, Run.writeBack),
    (Today.v733, Cfg.fcvm, Run.noWaker),
    (Order.kickFirst, Cfg.fcvm, Run.kickFirst),
    (Order.msrsFirst, timerNoApf, Run.msrsFirst),
    (Order.noLapic, Cfg.fcvm, Run.noLapic),
    (Partial.v733Kick, Cfg.fcvm, Run.pageReady),
    (Design.rules, Cfg.fcvm, Run.pageReady),
    (Design.rules, Cfg.fcvm, Run.snapshot),
    (Design.rules, Cfg.fcvm, Run.wakeAll),
    (Design.rules, Cfg.fcvm, Run.sleeperDue),
    (Today.v614, Cfg.fcvm, Run.pageReady) ]

end Wake
