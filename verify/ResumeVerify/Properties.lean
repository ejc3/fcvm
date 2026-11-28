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
     | .fresh host => !host
     | .sync e => decide (e.o < 0))

/-- R1: a tick is owed and the timer that was to deliver it is no longer armed. -/
def lostTick (s : State) : Bool :=
  running s && anyVcpu s fun _ c => c.owed && !c.tEn

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

/-- R2: an SGI is pending that no CPU sent. -/
def strayPending (s : State) : Bool :=
  running s && anyVcpu s fun _ c => c.sgi.latch && !c.call

/-- R2, for a guest hypervisor: a nested vCPU is loaded and the priority mask and group enable
the guest hypervisor gave it (`ICH_VMCR_EL2`) are gone: that nested vCPU takes no interrupt. -/
def lostNestedMask (s : State) : Bool :=
  running s && anyVcpu s fun _ c => c.l2 && !c.ichv

/-- R3: the vCPU thread cannot block and the guest CPU cannot be interrupted: idle,
`kvm_arch_vcpu_runnable` true, and nothing that can be signalled. A full host CPU, an idle
guest CPU with no tick: #2155's signature. -/
def spins (C : Cfg) (s : State) : Bool :=
  running s &&
    anyVcpu s fun v c =>
      c.thr == .guest && !c.stopped && c.g == .idle && kvmPending C s.vm v &&
        !deliverable C s.vm v .tmr && !deliverable C s.vm v .sgi && !deliverable C s.vm v .spi

/-- The timer's condition holds and its interrupt cannot become pending for KVM whatever its
line does: disabled, active, or masked. -/
def timerStuck (m : VM) (c : Vcpu) : Bool :=
  timerDue m c && !(m.gicEn && c.pmr && c.tmr.en && !c.tmr.act)

/-- R3, the host's own loop: a vCPU thread that is blocked, or stopped, with an enabled and
expired timer that cannot wake it. `kvm_timer_blocking` arms the background timer with the
earliest expiry, which is zero for an expired timer (arch_timer.c:293-313, 595-615);
`kvm_bg_timer_expire` wakes the thread (316-338); `kvm_arch_vcpu_runnable` is still false, the
thread schedules out, and the put arms the timer with zero again. A full host CPU with the
thread never in the guest, and a guest CPU with no tick. -/
def timerLoops (C : Cfg) (s : State) : Bool :=
  running s &&
    anyVcpu s fun v c =>
      c.thr != .user &&
        ((c.thr == .blocked && !c.stopped && timerStuck s.vm c && !kvmPending C s.vm v) ||
         (c.stopped && timerDue s.vm c))

/-- R4: the guest's counter is not the counter it should read. -/
def clockOff (s : State) : Bool :=
  running s && s.vm.ctr != .sync { t := 0, o := 0 }

/-- R4, in a VM that was restored and not paused since: the clock a restore gives a clone,
apart from what a pause in place does. -/
def clockOffClone (s : State) : Bool := clockOff s && s.vm.clone

/-- R5: a request of the VMM on a paused source changed a vCPU's interrupt, timer or power
state, or took something from the device's interrupt (`undisturbed`, `Spec.lean`). -/
def sourceDisturbed (s : State) : Bool := s.disturbed

/-- Not a requirement: a request of the VMM on a paused source left the device's interrupt
pending with no event behind it. A virtio driver's handler finds nothing and returns. -/
def sourceKicked (s : State) : Bool := s.kicked

/-- R6: a CPU the guest has on is stopped, or one it has off is not. -/
def powerWrong (s : State) : Bool :=
  running s && anyVcpu s fun _ c => c.stopped != c.gOff

def checks (C : Cfg) : List (String × (State → Bool)) :=
  [("R1 lostIpi", lostIpi), ("R1 lostDeviceIrq", lostDeviceIrq C), ("R1 lostTick", lostTick),
   ("R1 timerCannotFire", timerCannotFire),
   ("R2 stuckActive", stuckActive), ("R2 stuckPriority", stuckPriority), ("R2 lostEnable", lostEnable),
   ("R2 strayEnable", strayEnable), ("R2 strayPending", strayPending),
   ("R2 lostNestedMask", lostNestedMask),
   ("R3 spins", spins C), ("R3 timerLoops", timerLoops C), ("R4 clockOff", clockOff),
   ("R4 clockOffClone", clockOffClone),
   ("R5 sourceDisturbed", sourceDisturbed), ("R6 powerWrong", powerWrong)]

/-- What the search also reports and no requirement forbids. -/
def notes : List (String × (State → Bool)) := [("-- sourceKicked", sourceKicked)]

/-- Every requirement at once. -/
def healthy (C : Cfg) (s : State) : Bool := (checks C).all fun (_, f) => !f s

end Resume
