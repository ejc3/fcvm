import WakeVerify.Spec

/-!
# The properties

Each is a `Prop` about one state, decidable, so the search can test it on every reachable state and
the kernel can check a counterexample (`Breaks.lean`). `Proofs.lean` proves them for every
reachable state of the design.

A vCPU **sleeps in `kvm_wait`** when it has executed the HLT there and KVM holds it halted. Its
wake-up is **due** when the unlock slow path has released the lock byte, because that path then
sends the kick (pvq.h:529, 539). For a queue head that is `lock->locked != _Q_SLOW_VAL`; a queue
node is in the same position once `pv_kick_node` has hashed it (pvq.h:377-388).
-/

namespace Wake

/-- vCPU `i` has executed the HLT in `kvm_wait` and KVM holds it halted. -/
def State.sleeps (s : State) (i : Vid) : Bool :=
  let v := s.get i
  v.h.mp == .halted && (v.pc == .hdWoke || v.pc == .ndWoke)

/-- vCPU `i` sleeps in `kvm_wait` and the lock byte it waits on has been released. -/
def State.due (s : State) (i : Vid) : Bool :=
  let v := s.get i
  v.h.mp == .halted && s.lock != .slow && (v.pc == .hdWoke || (v.pc == .ndWoke && v.hashed))

/-- Nothing the model follows will wake vCPU `i`: no kick recorded or about to be sent, no
completion on its way, no interrupt it can take, no timer it can take. -/
def State.nothingPending (s : State) (i : Vid) : Bool :=
  let v := s.get i
  !v.h.pv && (s.get i.other).pc != .ulKick && v.apf == .none
    && !(v.ifOn && (v.ipi || v.apfIrq || v.tmr != .off))

/-- The kick vCPU `i` is due is gone. -/
def State.lostKick (s : State) (i : Vid) : Bool :=
  s.due i && s.nothingPending i

/-- The hang: a lost kick with interrupts off, so only an NMI or INIT wakes the vCPU. -/
def State.lostKickHard (s : State) (i : Vid) : Bool :=
  s.lostKick i && !(s.get i).ifOn

/-- A lost kick with interrupts on: the vCPU sleeps until some later interrupt. -/
def State.lostKickSoft (s : State) (i : Vid) : Bool :=
  s.lostKick i && (s.get i).ifOn

/-- vCPU `i` waits in a cross-CPU call whose target sleeps in `kvm_wait` with interrupts off and
no kick recorded. The target cannot take the call's interrupt, and the caller, which is the only
other vCPU, is not in the lock's unlock path to kick it. Stated without `lostKick`: it does not
ask whether the lock byte has been released. -/
def State.stuckCall (s : State) (i : Vid) : Bool :=
  let t := s.get i.other
  (s.get i).pc == .ccWait && t.csd && s.sleeps i.other && !t.ifOn && !t.h.pv

/-- A cross-CPU call is queued for vCPU `i` and its interrupt is not pending. -/
def State.lostIpi (s : State) (i : Vid) : Bool :=
  (s.get i).csd && !(s.get i).ipi

/-- vCPU `i` is in a synthetic halt and no completion will end it. -/
def State.orphanHalt (s : State) (i : Vid) : Bool :=
  (s.get i).h.apfHalted && (s.get i).apf == .none

/-- vCPU `i` sleeps in the idle loop for a timer that no longer exists, with nothing else pending
that would wake it. -/
def State.lostTimer (s : State) (i : Vid) : Bool :=
  let v := s.get i
  v.pc == .idWokeT && v.h.mp == .halted && v.tmr == .off
    && !v.h.pv && v.apf == .none && !v.ipi && !v.apfIrq

/-- vCPU `i` sleeps in `kvm_wait` and nothing stands to wake it: no kick is recorded or about to
be sent, the other vCPU does not own the lock (an owner's unlock sends the kick when the byte is
`_Q_SLOW_VAL`), and, for a queue node that `pv_kick_node` has not hashed, the other vCPU is not
the queue head that will hand the queue over to it. Unlike `lostKick` this does not wait for the
lock byte to be released. -/
def State.orphan (s : State) (i : Vid) : Bool :=
  let v := s.get i
  let o := s.get i.other
  s.sleeps i && !v.h.pv && o.pc != .ulKick && !o.pc.holds
    && !(v.pc == .ndWoke && !v.hashed && o.pc.isHead)

/-- A queue node sleeps although the queue head was handed to it and `pv_kick_node` has run: a
break in the guest's own protocol. -/
def State.strandedNode (s : State) (i : Vid) : Bool :=
  let v := s.get i
  v.h.mp == .halted && v.pc == .ndWoke && !v.hashed && v.mcs
    && (s.get i.other).pc != .hoKick && s.nothingPending i

/-- **P1, NoLostKick.** No vCPU sleeps in `kvm_wait`, its wake-up due, with nothing pending that
can wake it. -/
def NoLostKick (s : State) : Prop :=
  ∀ i, s.lostKick i = false

/-- P1 for the hang alone: interrupts off. -/
def NoHang (s : State) : Prop :=
  ∀ i, s.lostKickHard i = false

/-- **P2, NoStuckCall.** No vCPU waits in a cross-CPU call for a vCPU that sleeps in `kvm_wait`
with interrupts off and no kick recorded, and no queued call is without its interrupt. -/
def NoStuckCall (s : State) : Prop :=
  ∀ i, s.stuckCall i = false ∧ s.lostIpi i = false

/-- **P3, MutualExclusion.** A wake-up for any reason (a kick, a completion, an NMI, an interrupt
that was not for it) never lets a vCPU into the critical section beside the other: every HLT is in
a loop that reads the lock again. `hlt_rechecks` in `Proofs.lean` is the same statement about the
guest paths alone. -/
def MutualExclusion (s : State) : Prop :=
  ¬ ((s.get .a).pc.holds = true ∧ (s.get .b).pc.holds = true)

/-- **P1 before the release, HasWaker.** Every vCPU asleep in `kvm_wait` has a kick recorded or
about to be sent, or has the other vCPU ahead of it in the lock's path: as the owner, whose unlock
will find `_Q_SLOW_VAL` and kick (`unlock_slow_kicks` in `Proofs.lean`), or as the queue head that
hands over to an unhashed node. This is a statement about one state. That the other vCPU then
takes its steps is not part of it. -/
def HasWaker (s : State) : Prop :=
  ∀ i, s.orphan i = false

/-- The two vCPUs are never both asleep in `kvm_wait` with no kick recorded for either. -/
def NoMutualSleep (s : State) : Prop :=
  ¬ (s.sleeps .a = true ∧ s.sleeps .b = true ∧ s.a.h.pv = false ∧ s.b.h.pv = false)

/-- A synthetic halt always has its completion coming. -/
def NoOrphanHalt (s : State) : Prop :=
  ∀ i, s.orphanHalt i = false

/-- A vCPU that sleeps for its timer still has one. -/
def NoLostTimer (s : State) : Prop :=
  ∀ i, s.lostTimer i = false

/-- The guest's queue protocol never strands a node. -/
def NoStrandedNode (s : State) : Prop :=
  ∀ i, s.strandedNode i = false

instance : DecidablePred NoLostKick := fun s =>
  if h : s.lostKick .a = false ∧ s.lostKick .b = false then
    isTrue (fun i => by cases i; exact h.1; exact h.2)
  else isFalse (fun k => h ⟨k .a, k .b⟩)

instance : DecidablePred NoHang := fun s =>
  if h : s.lostKickHard .a = false ∧ s.lostKickHard .b = false then
    isTrue (fun i => by cases i; exact h.1; exact h.2)
  else isFalse (fun k => h ⟨k .a, k .b⟩)

instance : DecidablePred NoStuckCall := fun s =>
  if h : (s.stuckCall .a = false ∧ s.lostIpi .a = false) ∧
         (s.stuckCall .b = false ∧ s.lostIpi .b = false) then
    isTrue (fun i => by cases i; exact h.1; exact h.2)
  else isFalse (fun k => h ⟨k .a, k .b⟩)

instance : DecidablePred MutualExclusion := fun s => by
  unfold MutualExclusion; exact inferInstance

instance : DecidablePred HasWaker := fun s =>
  if h : s.orphan .a = false ∧ s.orphan .b = false then
    isTrue (fun i => by cases i; exact h.1; exact h.2)
  else isFalse (fun k => h ⟨k .a, k .b⟩)

instance : DecidablePred NoMutualSleep := fun s => by
  unfold NoMutualSleep; exact inferInstance

instance : DecidablePred NoOrphanHalt := fun s =>
  if h : s.orphanHalt .a = false ∧ s.orphanHalt .b = false then
    isTrue (fun i => by cases i; exact h.1; exact h.2)
  else isFalse (fun k => h ⟨k .a, k .b⟩)

instance : DecidablePred NoLostTimer := fun s =>
  if h : s.lostTimer .a = false ∧ s.lostTimer .b = false then
    isTrue (fun i => by cases i; exact h.1; exact h.2)
  else isFalse (fun k => h ⟨k .a, k .b⟩)

instance : DecidablePred NoStrandedNode := fun s =>
  if h : s.strandedNode .a = false ∧ s.strandedNode .b = false then
    isTrue (fun i => by cases i; exact h.1; exact h.2)
  else isFalse (fun k => h ⟨k .a, k .b⟩)

/-- The run is possible and the property holds after it. -/
def after (r : Option State) (P : State → Prop) : Prop :=
  match r with
  | some s => P s
  | none => False

instance (r : Option State) (P : State → Prop) [DecidablePred P] : Decidable (after r P) :=
  match r with
  | some s => inferInstanceAs (Decidable (P s))
  | none => isFalse (fun h => h)

/-- The run is possible and the property fails after it. -/
def breaks (r : Option State) (P : State → Prop) : Prop :=
  match r with
  | some s => ¬ P s
  | none => False

instance (r : Option State) (P : State → Prop) [DecidablePred P] : Decidable (breaks r P) :=
  match r with
  | some s => inferInstanceAs (Decidable (¬ P s))
  | none => isFalse (fun h => h)

end Wake
