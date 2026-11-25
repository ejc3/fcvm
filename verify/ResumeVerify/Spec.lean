/-!
# A guest vCPU's timer and interrupt state across a snapshot and a restore, on arm64 KVM

Plain data and one function per step. The kernel's rules (host KVM, Linux v7.3.3, paths under
arch/arm64/kvm/) are fixed here, each with the file and line it was read from. What a VMM does,
and in which order, is a `Rules` value: `Today.lean` has one per VMM as pinned, `Design.lean`
has the repaired ones. The README says what is left out and why.

One priority, as Linux uses for every ordinary interrupt (drivers/irqchip/irq-gic-v3.c,
`GICD_INT_DEF_PRI`), so the active priorities of a vCPU are one bit.
-/

namespace Resume

/-! ## The state -/

/-- The interrupts followed. `tmr` is the timer PPI the guest's tick uses: the EL1 virtual timer
of an ordinary guest, the EL2 timer of a guest hypervisor (NV2). `sgi` is one SGI, the
cross-call IPI. `spi` is one shared interrupt of a device, routed to vCPU 0. -/
inductive Irq where
  | tmr | sgi | spi
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- What KVM keeps for one interrupt (`struct vgic_irq`, include/kvm/arm_vgic.h): `enabled`,
`pending_latch`, `line_level`, `active`. -/
structure IrqSt where
  en : Bool
  latch : Bool
  line : Bool
  act : Bool
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Where the guest CPU is. `hdl i`: in the handler of `i`, between the acknowledge
(`ICC_IAR1_EL1` read, irq-gic-v3.c `gic_handle_irq`) and the EOI. This is guest registers and
memory, which every snapshot carries exactly. -/
inductive G where
  | run | idle | hdl (i : Irq)
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- Where the vCPU thread is: in the guest, blocked in `kvm_vcpu_block` for a WFI
(arm.c:1065-1089), or in userspace, out of `KVM_RUN`. -/
inductive Thr where
  | guest | blocked | user
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- An error of the guest's counter against the counter it should read: `t` in units of host
time that passed while the guest did not run, `o` in units of a guest hypervisor's
`CNTVOFF_EL2`. The two are kept apart so that one cannot cancel the other. -/
structure Err where
  t : Int
  o : Int
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- The counter the tick timer compares against: off by `e`, or `fresh`, the offset a new vCPU
was given (`kvm_timer_vcpu_init`, arch_timer.c:1105-1122), which no restore has replaced. -/
inductive Ctr where
  | sync (e : Err) | fresh
  deriving DecidableEq, Repr, Hashable, Inhabited

structure Vcpu where
  /-- guest: where it executes -/
  g : G
  /-- guest: a cross-call sent to this CPU has not been handled (kernel/smp.c, the sender waits
  in `csd_lock_wait`) -/
  call : Bool
  /-- guest: this CPU is offline (PSCI `CPU_OFF`) -/
  gOff : Bool
  thr : Thr
  /-- `mp_state` is `KVM_MP_STATE_STOPPED` (arm.c:779-840) -/
  stopped : Bool
  /-- `ICC_PMR_EL1` admits the one priority -/
  pmr : Bool
  /-- `ICC_IGRPEN1_EL1` -/
  grp : Bool
  /-- the active-priority bit of `ICC_AP1R0_EL1` -/
  ap : Bool
  tmr : IrqSt
  sgi : IrqSt
  /-- the tick timer is enabled and unmasked (`CNT*_CTL_EL0`) -/
  tEn : Bool
  /-- its compare value, in the guest's counter -/
  cv : Int
  /-- NV2: `CNTVOFF_EL2` is not zero (the guest hypervisor has a nested vCPU loaded) -/
  l2 : Bool
  /-- the timer PPI is mapped to the host's (`irq->hw`, set by `kvm_timer_enable` at the
  vCPU's first `KVM_RUN`, arch_timer.c:1587-1635) -/
  hw : Bool
  deriving DecidableEq, Repr, Hashable, Inhabited

structure VM where
  c0 : Vcpu
  c1 : Vcpu
  spi : IrqSt
  /-- the device behind `spi` has an event the guest has not handled (device state) -/
  dev : Bool
  /-- `GICD_CTLR` group enable -/
  gicEn : Bool
  ctr : Ctr
  /-- `KVM_ARCH_FLAG_VM_COUNTER_OFFSET`: `KVM_ARM_SET_COUNTER_OFFSET` was used on this VM -/
  offFlag : Bool
  /-- the time the guest has run: what its counter should read -/
  ideal : Nat
  /-- this VM was made by a restore (Cloud Hypervisor then knows its redistributors) -/
  restored : Bool
  /-- this VM was made by a restore and has not been paused since -/
  clone : Bool
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- A vCPU index. `false` is vCPU 0. -/
abbrev Vid := Bool

def VM.get (m : VM) (v : Vid) : Vcpu := if v then m.c1 else m.c0
def VM.set (m : VM) (v : Vid) (c : Vcpu) : VM := if v then { m with c1 := c } else { m with c0 := c }

/-- What one slot of an image holds for a vCPU. The redistributor and CPU-interface parts are
what the VMM read for that slot, from whichever vCPU its request named. -/
structure Slot where
  g : G
  call : Bool
  gOff : Bool
  stopped : Bool
  tEn : Bool
  cv : Int
  l2 : Bool
  /-- the counter read for this vCPU, as an error against `ideal` -/
  cnt : Ctr
  tmr : IrqSt
  sgi : IrqSt
  pmr : Bool
  grp : Bool
  ap : Bool
  deriving DecidableEq, Repr, Hashable, Inhabited

structure Image where
  s0 : Slot
  s1 : Slot
  spi : IrqSt
  dev : Bool
  gicEn : Bool
  ideal : Nat
  deriving DecidableEq, Repr, Hashable, Inhabited

def Image.slot (i : Image) (v : Vid) : Slot := if v then i.s1 else i.s0

/-- Where the VMM is with the VM. -/
inductive Ph where
  | running
  /-- vCPUs out of `KVM_RUN` by the VMM's pause -/
  | paused
  /-- a restore has built the VM and not started it (fcvm loads with `resume_vm: false`) -/
  | loaded
  deriving DecidableEq, Repr, Hashable, Inhabited

/-- What of a vCPU a pause and a save must leave as they found it (R5). -/
structure Keep where
  g : G
  call : Bool
  stopped : Bool
  pmr : Bool
  grp : Bool
  ap : Bool
  tmr : IrqSt
  sgi : IrqSt
  tEn : Bool
  cv : Int
  deriving DecidableEq, Repr, Hashable, Inhabited

def Vcpu.keep (c : Vcpu) : Keep :=
  { g := c.g, call := c.call, stopped := c.stopped, pmr := c.pmr, grp := c.grp, ap := c.ap,
    tmr := { c.tmr with line := false }, sgi := c.sgi, tEn := c.tEn, cv := c.cv }

structure State where
  vm : VM
  ph : Ph
  img : Option Image
  /-- host time since the pause or the load, while the guest does not run -/
  since : Nat
  /-- at the pause: both vCPUs (R5) -/
  kept : Option (Keep × Keep)
  /-- R5: a resume in place found a vCPU changed -/
  disturbed : Bool
  snaps : Nat
  restores : Nat
  waits : Nat
  deriving DecidableEq, Repr, Hashable, Inhabited

/-! ## What exists around the guest -/

structure Cfg where
  /-- the guest is a hypervisor (NV2): its tick is an EL2 timer, on the physical offset -/
  nv : Bool
  /-- a guest hypervisor may load a nested vCPU (`CNTVOFF_EL2` not zero) -/
  l2 : Bool
  /-- the device's interrupt is level-triggered, and the device holds its line -/
  spiLevel : Bool
  /-- there is a device raising `spi` -/
  device : Bool
  /-- guest CPU 1 is offline -/
  cpu1Off : Bool
  maxSnaps : Nat
  maxRestores : Nat
  /-- how far guest time runs -/
  maxIdeal : Nat
  /-- how many times host time passes while the guest does not run -/
  maxWaits : Nat
  /-- host time may pass between the pause and the save (the caller's two requests) -/
  waitBeforeSave : Bool
  /-- host time may pass after the save: the memory dump, and however long until a resume in
  place or a restore -/
  waitAfterSave : Bool
  /-- host time may pass between a restore's load and its resume -/
  waitLoaded : Bool
  deriving DecidableEq, Repr, Inhabited

/-! ## The rule books' type: what a VMM does -/

/-- One request of a restore, in the order the VMM makes them. -/
inductive Op where
  /-- `KVM_CREATE_VCPU` -/
  | create (v : Vid)
  /-- `KVM_ARM_VCPU_INIT`, with `KVM_ARM_VCPU_POWER_OFF` or not -/
  | init (v : Vid) (off : Bool)
  /-- `KVM_ARM_SET_COUNTER_OFFSET` from vCPU 0's saved counter -/
  | offset
  /-- `KVM_SET_ONE_REG` of `CNTPCT_EL0` -/
  | cntp (v : Vid)
  /-- `KVM_SET_ONE_REG` of `KVM_REG_ARM_TIMER_CNT` (`CNTVCT_EL0`) -/
  | cntv (v : Vid)
  /-- the other registers: the timer's control and compare value, `CNTVOFF_EL2`, and the
  guest's own registers -/
  | regs (v : Vid)
  /-- `KVM_SET_MP_STATE` -/
  | mp (v : Vid)
  /-- the vGIC device, initialised -/
  | gic
  /-- `KVM_DEV_ARM_VGIC_GRP_DIST_REGS` -/
  | dist
  /-- `KVM_DEV_ARM_VGIC_GRP_REDIST_REGS` of slot `v`, addressed to vCPU `v` -/
  | redist (v : Vid)
  /-- `KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS` of slot `v`, addressed to vCPU `v` -/
  | icc (v : Vid)
  /-- `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO` -/
  | level
  deriving DecidableEq, Repr, Hashable, Inhabited

structure Rules where
  name : String
  /-- the save addresses every redistributor and CPU interface as vCPU 0's until a restore has
  told the VMM which is which -/
  slotsFromRestore : Bool
  /-- the image's counter is the one at the pause, not the one read at the save -/
  cntAtPause : Bool
  /-- a resume moves the counter offset by the time paused (the guest's clock stood still) -/
  freeze : Bool
  /-- the resume of a restored VM takes out the time since its load (or, the same thing, the
  counter is written at that resume and not at the load) -/
  freezeLoaded : Bool
  /-- a restore sets the counter to the saved one plus the wall time since the pause -/
  advance : Bool
  /-- the save reads `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO` -/
  levelInfo : Bool
  /-- the restore writes the clear-registers (`ICENABLER`, `ICACTIVER`) with the complement of
  the saved bits, so that what the new VM has set and the image has not is cleared -/
  clearInverted : Bool
  restore : List Op
  deriving Repr, Inhabited

/-! ## The kernel's rules -/

def IrqSt.none : IrqSt := { en := false, latch := false, line := false, act := false }

/-- `irq_is_pending` (vgic.h): the latch, or the line of a level interrupt. `tmr` is a PPI and
`sgi` an SGI: PPIs are level and SGIs edge, and the guest cannot change that
(vgic-mmio.c:733-764, vgic-init.c:281-285). -/
def pend (level : Bool) (i : IrqSt) : Bool := i.latch || (level && i.line)

/-- `kvm_vgic_vcpu_pending_irq` (vgic.c:1242-1277): an interrupt of this vCPU is pending,
enabled, not active, and of a priority `ICC_PMR_EL1` admits. It reads neither the group enable
nor the active priorities. This, with `mp_state`, is `kvm_arch_vcpu_runnable` (arm.c:849-859),
which decides whether a WFI blocks (arm.c:1065-1089, handle_exit.c:130-178). -/
def kvmPending (C : Cfg) (m : VM) (v : Vid) : Bool :=
  let c := m.get v
  m.gicEn && c.pmr &&
    ((pend true c.tmr && c.tmr.en && !c.tmr.act) ||
     (pend false c.sgi && c.sgi.en && !c.sgi.act) ||
     (!v && pend C.spiLevel m.spi && m.spi.en && !m.spi.act))

/-- The interrupt `i` is signalled to the guest CPU `v`: what the virtual CPU interface does
with a pending list register. Beyond `kvmPending` it needs the group enabled and no active
priority at or above the interrupt's (GIC architecture, IHI 0069, 4.8: the running priority). -/
def deliverable (C : Cfg) (m : VM) (v : Vid) (i : Irq) : Bool :=
  let c := m.get v
  m.gicEn && c.pmr && c.grp && !c.ap &&
    (match i with
     | .tmr => pend true c.tmr && c.tmr.en && !c.tmr.act
     | .sgi => pend false c.sgi && c.sgi.en && !c.sgi.act
     | .spi => !v && pend C.spiLevel m.spi && m.spi.en && !m.spi.act)

/-- The tick timer's condition holds: enabled, unmasked and `CVAL <= counter`
(`kvm_timer_pending`, arch_timer.c:366-406). A fresh offset puts the counter near zero; an
error of a whole `CNTVOFF_EL2` decides by its sign. -/
def timerDue (m : VM) (c : Vcpu) : Bool :=
  c.tEn &&
    (match m.ctr with
     | .fresh => false
     | .sync e => if e.o < 0 then false else if e.o > 0 then true else decide (c.cv ≤ (m.ideal : Int) + e.t))

/-- What the guest reads as its counter, where it is defined. -/
def counter (m : VM) : Int :=
  match m.ctr with
  | .sync e => (m.ideal : Int) + e.t
  | .fresh => 0

/-! ## The guest's steps, and KVM's while the guest runs -/

inductive GAct where
  /-- program the tick timer one unit ahead (drivers/clocksource/arm_arch_timer.c, `set_next_event`) -/
  | arm
  /-- enter the idle loop (kernel/sched/idle.c) -/
  | toIdle
  /-- WFI with nothing pending for KVM: the vCPU thread blocks -/
  | wfi
  /-- acknowledge interrupt `i`: active, the latch of an edge interrupt consumed, the active
  priority raised (irq-gic-v3.c `gic_handle_irq`; `vgic_v3_fold_lr`, vgic-v3.c:74-136, is how
  KVM learns of it at the next exit) -/
  | ack (i : Irq)
  /-- the handler's work, then EOI: priority drop and deactivation (EOImode 0) -/
  | eoi
  /-- send the cross-call IPI to the other CPU (`ICC_SGI1R_EL1`, arch/arm64/kernel/smp.c) -/
  | send
  /-- NV2: load a nested vCPU, `CNTVOFF_EL2` becomes non-zero -/
  | l2
  deriving DecidableEq, Repr, Hashable, Inhabited

def guest (C : Cfg) (v : Vid) (a : GAct) (m : VM) : Option VM :=
  let c := m.get v
  if c.thr != .guest || c.stopped then none else
  match a with
  | .arm =>
    if c.g != .run || c.tEn then none else
      some (m.set v { c with tEn := true, cv := counter m + 1 })
  | .toIdle => if c.g != .run then none else some (m.set v { c with g := .idle })
  | .wfi =>
    if c.g != .idle || kvmPending C m v then none else some (m.set v { c with thr := .blocked })
  | .ack i =>
    if (c.g != .run && c.g != .idle) || !deliverable C m v i then none else
    match i with
    | .tmr => some (m.set v { c with g := .hdl .tmr, ap := true, tmr := { c.tmr with act := true, latch := false } })
    | .sgi => some (m.set v { c with g := .hdl .sgi, ap := true, sgi := { c.sgi with act := true, latch := false } })
    | .spi =>
      some ({ m with spi := { m.spi with act := true, latch := false } }.set v { c with g := .hdl .spi, ap := true })
  | .eoi =>
    match c.g with
    | .hdl .tmr =>
      -- the handler masks the timer, so its line falls (arm_arch_timer.c `timer_handler`)
      some (m.set v { c with g := .run, ap := false, tEn := false, tmr := { c.tmr with act := false, line := false } })
    | .hdl .sgi =>
      some (m.set v { c with g := .run, ap := false, call := false, sgi := { c.sgi with act := false } })
    | .hdl .spi =>
      some ({ m with dev := false, spi := { m.spi with act := false, line := false } }.set v { c with g := .run, ap := false })
    | _ => none
  | .send =>
    let w := m.get (!v)
    if c.g != .run || w.call || w.gOff then none else
      -- `vgic_v3_dispatch_sgi` (vgic-mmio-v3.c): the latch of the target's SGI
      some (m.set (!v) { w with call := true, sgi := { w.sgi with latch := true } })
  | .l2 => if !C.nv || !C.l2 || c.l2 || c.g != .run then none else some (m.set v { c with l2 := true })

/-- The timer fires: its output is a level into the vGIC (`kvm_timer_update_irq`,
arch_timer.c:457-483; `kvm_vgic_inject_irq`, vgic.c:533-572). For a blocked vCPU this is the
background timer (`kvm_bg_timer_expire`, arch_timer.c:316-338) and the load that follows. -/
def fire (m : VM) (v : Vid) : Option VM :=
  let c := m.get v
  if c.thr == .user || c.stopped || !timerDue m c || c.tmr.line then none else
    some (m.set v { c with tmr := { c.tmr with line := true } })

/-- A blocked vCPU runs again when `kvm_arch_vcpu_runnable` holds (`kvm_vcpu_block`). -/
def wake (C : Cfg) (m : VM) (v : Vid) : Option VM :=
  let c := m.get v
  if c.thr != .blocked || c.stopped || !kvmPending C m v then none else
    some (m.set v { c with thr := .guest })

/-- The device raises its interrupt: the latch of an edge interrupt, the line of a level one
(`vgic_validate_injection` and `kvm_vgic_inject_irq`, vgic.c:379-392, 533-572). -/
def devRaise (C : Cfg) (m : VM) : Option VM :=
  if !C.device || m.dev then none else
    some { m with dev := true,
                  spi := if C.spiLevel then { m.spi with line := true } else { m.spi with latch := true } }

/-! ## The VMM's steps -/

/-- The slot's source: which vCPU a request for slot `v` reads. -/
def source (R : Rules) (m : VM) (v : Vid) : Vid :=
  if R.slotsFromRestore && !m.restored then false else v

/-- The save. Guest registers and memory, `mp_state` and the timer registers are per-vCPU
ioctls on the vCPU's own descriptor. The redistributor and CPU-interface groups name their vCPU
by MPIDR in the request (vgic-kvm-device.c:484-509). A user read of `GICR_ISPENDR0` and
`GICD_ISPENDR` returns the latch alone (vgic-mmio.c:229-296), of `ISACTIVER` the active bit
(vgic-mmio.c:488-534); the line of a level interrupt is in `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO`
only (vgic-mmio.c:766-786). -/
def save (R : Rules) (s : State) : Image :=
  let m := s.vm
  let cnt : Ctr :=
    match m.ctr with
    | .sync e => .sync (if R.cntAtPause then { e with t := e.t - s.since } else e)
    | .fresh => .fresh
  let slot (v : Vid) : Slot :=
    let c := m.get v
    let r := m.get (source R m v)
    { g := c.g, call := c.call, gOff := c.gOff, stopped := c.stopped, tEn := c.tEn, cv := c.cv,
      l2 := c.l2, cnt := cnt,
      tmr := { r.tmr with line := R.levelInfo && r.tmr.line },
      sgi := { r.sgi with line := false },
      pmr := r.pmr, grp := r.grp, ap := r.ap }
  { s0 := slot false, s1 := slot true,
    spi := { m.spi with line := R.levelInfo && m.spi.line },
    dev := m.dev, gicEn := m.gicEn, ideal := m.ideal }

/-- A vCPU as `KVM_CREATE_VCPU` makes it: `vgic_v3_reset` zeroes the VMCR (vgic-v3.c:474-504),
so the priority mask is shut and the group off; SGIs enabled, PPIs not
(vgic-init.c:276-286). -/
def freshVcpu : Vcpu :=
  { g := .run, call := false, gOff := false, thr := .user, stopped := false,
    pmr := false, grp := false, ap := false,
    tmr := IrqSt.none, sgi := { IrqSt.none with en := true },
    tEn := false, cv := 0, l2 := false, hw := false }

/-- The VM a restore starts from: nothing created. -/
def emptyVM : VM :=
  { c0 := freshVcpu, c1 := freshVcpu, spi := IrqSt.none, dev := false, gicEn := false,
    ctr := .fresh, offFlag := false, ideal := 0, restored := true, clone := true }

/-- A clear-register written with `clr`, then its set-register with `set`: a bit that was set
and is not in `clr` stays (`vgic_uaccess_write_cenable` and `_senable`, vgic-mmio.c:189-228;
`vgic_mmio_uaccess_write_cactive` and `_sactive`, vgic-mmio.c:593-668). -/
def clearSet (cur clr set : Bool) : Bool := (cur && !clr) || set

/-- What the restore writes to a clear-register for a saved bit. -/
def clrOf (R : Rules) (saved : Bool) : Bool := if R.clearInverted then !saved else saved

/-- One request of a restore, by the kernel's rules. -/
def apply (R : Rules) (C : Cfg) (i : Image) (m : VM) : Op → VM
  | .create v =>
    -- `kvm_timer_vcpu_init` (arch_timer.c:1105-1122): unless the VM's offset was set by
    -- `KVM_ARM_SET_COUNTER_OFFSET`, a new vCPU resets the VM-wide offsets
    let m := m.set v freshVcpu
    if m.offFlag then m else { m with ctr := .fresh }
  | .init v off =>
    -- `kvm_arch_vcpu_ioctl_vcpu_init` (arm.c:1870-1905) and `kvm_timer_vcpu_reset`
    -- (arch_timer.c:1029-1073)
    let c := m.get v
    m.set v { c with stopped := off, tEn := false }
  | .offset =>
    -- `kvm_vm_ioctl_set_counter_offset` (arch_timer.c:1731-1764): both offsets, and the flag
    { m with offFlag := true, ctr := i.s0.cnt }
  | .cntp v =>
    -- `arch_timer_set_user` (sys_regs.c:1779-1802): the physical offset, VM-wide, unless the
    -- flag is set. The tick of an ordinary guest is on the virtual offset.
    if m.offFlag || !C.nv then m else { m with ctr := (i.slot v).cnt }
  | .cntv v =>
    -- the same function, for the virtual timer's context. Under NV2 that context's VM-wide
    -- offset is the PHYSICAL one and `CNTVOFF_EL2` is added on top (arch_timer.c:1048-1053,
    -- include/kvm/arm_arch_timer.h:171-196), and the write stores `now - value` in it without
    -- taking `CNTVOFF_EL2` out: the physical counter becomes the saved virtual counter.
    if m.offFlag then m else
    if !C.nv then { m with ctr := (i.slot v).cnt } else
      { m with ctr := match (i.slot v).cnt with
                      | .sync e => .sync (if (i.slot v).l2 then { e with o := e.o - 1 } else e)
                      | .fresh => .fresh }
  | .regs v =>
    let c := m.get v
    let s := i.slot v
    m.set v { c with g := s.g, call := s.call, gOff := s.gOff, tEn := s.tEn, cv := s.cv, l2 := s.l2 }
  | .mp v =>
    -- `kvm_arch_vcpu_ioctl_set_mpstate` (arm.c:816-840)
    let c := m.get v
    m.set v { c with stopped := (i.slot v).stopped }
  | .gic => { m with spi := IrqSt.none, gicEn := false }
  | .dist =>
    -- `GICD_CTLR`, then ICENABLER and ISENABLER, ISPENDR (the latch, exactly:
    -- `vgic_v3_uaccess_write_pending`, vgic-mmio-v3.c:383-394; ICPENDR is ignored,
    -- vgic-mmio-v3.c:649-652), ICACTIVER and ISACTIVER
    { m with gicEn := i.gicEn, dev := i.dev, ideal := i.ideal,
             spi := { m.spi with en := clearSet m.spi.en (clrOf R i.spi.en) i.spi.en,
                                 latch := i.spi.latch,
                                 act := clearSet m.spi.act (clrOf R i.spi.act) i.spi.act } }
  | .redist v =>
    let c := m.get v
    let s := i.slot v
    -- a user write of the active bit of a mapped interrupt is dropped
    -- (`vgic_hw_irq_change_active`, vgic-mmio.c:537-545)
    m.set v { c with tmr := { c.tmr with en := clearSet c.tmr.en (clrOf R s.tmr.en) s.tmr.en,
                                         latch := s.tmr.latch,
                                         act := if c.hw then c.tmr.act
                                                else clearSet c.tmr.act (clrOf R s.tmr.act) s.tmr.act },
                     sgi := { c.sgi with en := clearSet c.sgi.en (clrOf R s.sgi.en) s.sgi.en,
                                         latch := s.sgi.latch,
                                         act := clearSet c.sgi.act (clrOf R s.sgi.act) s.sgi.act } }
  | .icc v =>
    -- `vgic_v3_cpu_sysregs_uaccess` (vgic-sys-reg-v3.c): PMR, IGRPEN1, AP1R0 as given
    let c := m.get v
    let s := i.slot v
    m.set v { c with pmr := s.pmr, grp := s.grp, ap := s.ap }
  | .level =>
    -- `vgic_write_irq_line_level_info` (vgic-mmio.c:788-820)
    let c0 := m.c0
    let c1 := m.c1
    { m with spi := { m.spi with line := i.spi.line },
             c0 := { c0 with tmr := { c0.tmr with line := i.s0.tmr.line } },
             c1 := { c1 with tmr := { c1.tmr with line := i.s1.tmr.line } } }

def build (R : Rules) (C : Cfg) (i : Image) : VM := R.restore.foldl (apply R C i) emptyVM

/-- A vCPU thread enters `KVM_RUN`: the first time maps the timer interrupt
(`kvm_timer_enable`), and every load recomputes the timer's level from its registers
(`kvm_timer_vcpu_load_gic`, arch_timer.c:686-707). -/
def enter (m : VM) (c : Vcpu) : Vcpu :=
  { c with thr := .guest, hw := true, tmr := { c.tmr with line := timerDue m c } }

def resumeVM (R : Rules) (s : State) : VM :=
  let m := s.vm
  let frozen := if s.ph == .paused then R.freeze else R.freezeLoaded
  let m := if frozen then
      { m with ctr := match m.ctr with
                      | .sync e => .sync { e with t := e.t - s.since }
                      | .fresh => .fresh }
    else m
  { m with c0 := enter m m.c0, c1 := enter m m.c1 }

def pauseVcpu (c : Vcpu) : Vcpu := { c with thr := .user }

/-! ## One step -/

inductive Label where
  | g (v : Vid) (a : GAct)
  | fire (v : Vid)
  | wake (v : Vid)
  | dev
  /-- guest time passes -/
  | tick
  /-- host time passes while the guest does not run -/
  | wait
  | pause
  | save
  | resume
  | restore
  deriving DecidableEq, Repr, Hashable, Inhabited

def step (R : Rules) (C : Cfg) (l : Label) (s : State) : Option State :=
  match l with
  | .g v a => if s.ph != .running then none else (guest C v a s.vm).map ({ s with vm := · })
  | .fire v => if s.ph != .running then none else (fire s.vm v).map ({ s with vm := · })
  | .wake v => if s.ph != .running then none else (wake C s.vm v).map ({ s with vm := · })
  | .dev =>
    -- a device may raise its interrupt while the vCPUs are paused and before the save
    if s.ph != .running && !(s.ph == .paused && s.img.isNone) then none else
      (devRaise C s.vm).map ({ s with vm := · })
  | .tick =>
    if s.ph != .running || s.vm.ideal ≥ C.maxIdeal then none else
      some { s with vm := { s.vm with ideal := s.vm.ideal + 1 } }
  | .wait =>
    -- KVM's offsets do not move: the guest's counter runs on with the host's
    if s.waits ≥ C.maxWaits ||
        !((s.ph == .paused && s.img.isNone && C.waitBeforeSave) ||
          (s.ph == .paused && s.img.isSome && C.waitAfterSave) ||
          (s.ph == .loaded && C.waitLoaded)) then none else
      some { s with waits := s.waits + 1, since := s.since + 1,
                    vm := { s.vm with ctr := match s.vm.ctr with
                                            | .sync e => .sync { e with t := e.t + 1 }
                                            | .fresh => .fresh } }
  | .pause =>
    if s.ph != .running || s.snaps ≥ C.maxSnaps then none else
      some { s with ph := .paused, since := 0, img := none,
                    kept := some (s.vm.c0.keep, s.vm.c1.keep),
                    vm := { s.vm with c0 := pauseVcpu s.vm.c0, c1 := pauseVcpu s.vm.c1, clone := false } }
  | .save =>
    if s.ph != .paused || s.img.isSome then none else
      some { s with img := some (save R s), snaps := s.snaps + 1 }
  | .resume =>
    match s.ph with
    | .running => none
    | .paused =>
      -- the image is dropped: a restore from it later builds the VM the restore explored
      -- from this same state builds
      some { s with ph := .running, since := 0, kept := none, img := none, vm := resumeVM R s,
                    disturbed := s.disturbed || s.kept != some (s.vm.c0.keep, s.vm.c1.keep) }
    | .loaded => some { s with ph := .running, since := 0, vm := resumeVM R s }
  | .restore =>
    match s.img with
    | none => none
    | some i =>
      if s.ph != .paused || s.restores ≥ C.maxRestores then none else
        let m := build R C i
        some { s with ph := .loaded, since := 0, kept := none, img := none,
                      restores := s.restores + 1,
                      vm := if R.advance then
                              { m with ctr := match m.ctr with
                                              | .sync e => .sync { e with t := e.t + s.since }
                                              | .fresh => .fresh }
                            else m }

/-- A guest that has booted: both CPUs running, the interrupts it uses enabled, the priority
mask open, nothing pending. A CPU the guest took offline has its timer interrupt and its SGI
disabled (observed on a Linux guest: #2155, comment 7174817511). -/
def bootVcpu (off : Bool) : Vcpu :=
  { g := .run, call := false, gOff := off, thr := .guest, stopped := off,
    pmr := true, grp := true, ap := false,
    tmr := { IrqSt.none with en := !off }, sgi := { IrqSt.none with en := !off },
    tEn := false, cv := 0, l2 := false, hw := true }

def init (C : Cfg) : State :=
  { vm := { c0 := bootVcpu false, c1 := bootVcpu C.cpu1Off,
            spi := { IrqSt.none with en := true }, dev := false, gicEn := true,
            ctr := .sync { t := 0, o := 0 }, offFlag := false, ideal := 0, restored := false,
            clone := false },
    ph := .running, img := none, since := 0, kept := none, disturbed := false,
    snaps := 0, restores := 0, waits := 0 }

def replay (R : Rules) (C : Cfg) : List Label → State → Option State
  | [], s => some s
  | l :: ls, s => (step R C l s).bind (replay R C ls)

inductive Reachable (R : Rules) (C : Cfg) : State → Prop where
  | init : Reachable R C (init C)
  | step {s t : State} (l : Label) : Reachable R C s → step R C l s = some t → Reachable R C t

end Resume
