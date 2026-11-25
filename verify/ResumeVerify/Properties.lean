import ResumeVerify.Spec

/-!
# The properties, decidable on a state

Each is the negation of a requirement of the README: a state in which the requirement is
broken. All are read once the guest runs (`ph = running`), which is where a guest can be hurt.
-/

namespace Resume

def running (s : State) : Bool := s.ph == .running

def anyVcpu (s : State) (p : Vid → Vcpu → Bool) : Bool := p false s.vm.c0 || p true s.vm.c1

/-- R1: a cross-call is outstanding for a CPU whose SGI is neither pending nor active: the
sender waits for ever. -/
def lostIpi (s : State) : Bool :=
  running s && anyVcpu s fun _ c => c.call && !c.sgi.latch && !c.sgi.act

/-- R1: the device has an event and its interrupt is neither pending nor active. -/
def lostDeviceIrq (C : Cfg) (s : State) : Bool :=
  running s && s.vm.dev && !pend C.spiLevel s.vm.spi && !s.vm.spi.act

/-- R1: an armed timer that cannot fire: the counter it compares against was never restored,
or went back by a whole `CNTVOFF_EL2`. -/
def timerCannotFire (s : State) : Bool :=
  running s && (anyVcpu s fun _ c => c.tEn && !c.stopped) &&
    (match s.vm.ctr with
     | .fresh => true
     | .sync e => decide (e.o < 0))

/-- R2: an interrupt is active on a CPU that is not in its handler: nothing will deactivate it,
and it is never delivered again. -/
def stuckActive (s : State) : Bool :=
  running s &&
    ((anyVcpu s fun _ c => (c.tmr.act && c.g != .hdl .tmr) || (c.sgi.act && c.g != .hdl .sgi)) ||
     (s.vm.spi.act && s.vm.c0.g != .hdl .spi))

/-- R2: the active-priority bit is set on a CPU that is in no handler: nothing will drop it,
and no interrupt of the one priority is signalled again. -/
def stuckPriority (s : State) : Bool :=
  running s && anyVcpu s fun _ c => c.ap && (c.g == .run || c.g == .idle)

/-- R2: an enable the guest set is gone: an interrupt's, the group's, the distributor's, or the
priority mask is shut. -/
def lostEnable (s : State) : Bool :=
  running s &&
    (!s.vm.gicEn || !s.vm.spi.en ||
     anyVcpu s fun _ c => !c.gOff && (!c.tmr.en || !c.sgi.en || !c.pmr || !c.grp))

/-- R2: an interrupt the guest disabled is enabled: here, one of a CPU it took offline. -/
def strayEnable (s : State) : Bool :=
  running s && anyVcpu s fun _ c => c.gOff && (c.tmr.en || c.sgi.en)

/-- R2: an SGI is pending that no CPU sent, or a pending latch sits on the level-triggered
timer interrupt. -/
def strayPending (s : State) : Bool :=
  running s && anyVcpu s fun _ c => (c.sgi.latch && !c.call) || c.tmr.latch

/-- R3: the vCPU thread cannot block and the guest CPU cannot be interrupted: idle,
`kvm_arch_vcpu_runnable` true, and nothing that can be signalled. A full host CPU, an idle
guest CPU with no tick: #2155's signature. -/
def spins (C : Cfg) (s : State) : Bool :=
  running s &&
    anyVcpu s fun v c =>
      c.thr == .guest && !c.stopped && c.g == .idle && kvmPending C s.vm v &&
        !deliverable C s.vm v .tmr && !deliverable C s.vm v .sgi && !deliverable C s.vm v .spi

/-- R4: the guest's counter is not the counter it should read. -/
def clockOff (s : State) : Bool :=
  running s && s.vm.ctr != .sync { t := 0, o := 0 }

/-- R5: a resume in place found a vCPU's interrupt, timer or power state changed. -/
def sourceDisturbed (s : State) : Bool := s.disturbed

/-- R6: a CPU the guest has on is stopped, or one it has off is not. -/
def powerWrong (s : State) : Bool :=
  running s && anyVcpu s fun _ c => c.stopped != c.gOff

def checks (C : Cfg) : List (String × (State → Bool)) :=
  [("R1 lostIpi", lostIpi), ("R1 lostDeviceIrq", lostDeviceIrq C), ("R1 timerCannotFire", timerCannotFire),
   ("R2 stuckActive", stuckActive), ("R2 stuckPriority", stuckPriority), ("R2 lostEnable", lostEnable),
   ("R2 strayEnable", strayEnable), ("R2 strayPending", strayPending), ("R3 spins", spins C), ("R4 clockOff", clockOff),
   ("R5 sourceDisturbed", sourceDisturbed), ("R6 powerWrong", powerWrong)]

/-- Every requirement at once. -/
def healthy (C : Cfg) (s : State) : Bool := (checks C).all fun (_, f) => !f s

end Resume
