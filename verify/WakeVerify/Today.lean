import WakeVerify.Spec

/-!
# Today's rules: the code as it was before the repairs

Two host kernels and one VMM, each rule citing the file and line it models. "Today" is the day
the model was written. The kernels are Linux as released, which is still what a host runs when
fcvm does not build its kernel. The VMM is the fork before the restore kick.

- `V614`: Linux v6.14, the last release before `kvm_set_mp_state()`. Paths are under
  arch/x86/kvm/ of https://github.com/gregkh/linux at tag `v6.14`.
- `V733`: Linux v7.3.3, the release fcvm builds as its x86_64 host kernel, without fcvm's patches.
  Paths are under arch/x86/kvm/ at tag `v7.3.3`. Since v6.15 (e9cb61055fee, "KVM: x86: Clear
  pv_unhalted on all transitions to KVM_MP_STATE_RUNNABLE") every write of `mp_state` goes through
  `kvm_set_mp_state()` (x86.h:143-148), which also clears the kick when the new state is runnable.
- `firecracker`: the restore order of ejc3/firecracker at 3e203733cb91, the commit
  `rootfs-config.toml` pinned (`[firecracker]`) until #2135 moved the pin to a commit with the
  restore kick (`firecrackerWithKick` in `Design.lean`).
-/

namespace Wake

/-! ## Linux v6.14 -/

namespace V614

/-- `__kvm_emulate_halt` (x86.c:11290-11310): with an event pending the kick is consumed and the
vCPU stays as it is; otherwise it takes the halted state. `kvm_vcpu_has_events` counts the kick
(x86.c:11122-11123). -/
def halt (ev : Bool) (st : Mp) (h : Hv) : Hv :=
  if ev || h.pv then { h with pv := false } else { h with mp := st }

/-- The switch that ends `vcpu_block` (x86.c:11220-11235). -/
def unhalt (h : Hv) : Hv :=
  match h.mp with
  | .halted | .apResetHold => { mp := .runnable, pv := false, apfHalted := false }
  | .runnable => { h with apfHalted := false }
  | .initReceived | .uninitialized => h

/-- The tail of `kvm_arch_async_page_present` (x86.c:13476-13477): a plain assignment. -/
def present (h : Hv) : Hv :=
  { h with apfHalted := false, mp := .runnable }

/-- `APIC_DM_REMRD` in `__apic_accept_irq` (lapic.c:1351-1356). -/
def kick (h : Hv) : Hv :=
  { h with pv := true }

/-- `kvm_arch_vcpu_ioctl_get_mpstate` (x86.c:11774-11779): a halted vCPU with a kick pending is
reported runnable. -/
def getMpState (h : Hv) : Mp :=
  if (h.mp == .halted || h.mp == .apResetHold) && h.pv then .runnable else h.mp

/-- `kvm_arch_vcpu_ioctl_set_mpstate` (x86.c:11827): a plain assignment. -/
def setMpState (m : Mp) (h : Hv) : Hv :=
  { h with mp := m }

/-- `nested_vmx_vmexit` (vmx/nested.c:5073-5074), `nested_svm_vmexit` (svm/nested.c:996-997). -/
def nestedExit (h : Hv) : Hv :=
  { h with mp := .runnable }

/-- `kvm_xen_schedop_poll` (xen.c:1483, 1494). -/
def xenBlock (h : Hv) : Hv := { h with mp := .halted }
def xenWake (h : Hv) : Hv := { h with mp := .runnable }

/-- INIT at an application processor: `kvm_apic_accept_events` (lapic.c:3397-3403) with
`kvm_vcpu_reset` (x86.c:12440-12442). -/
def initAp (h : Hv) : Hv :=
  { h with mp := .initReceived, apfHalted := false }

/-- SIPI in wait-for-SIPI (lapic.c:3404-3413). -/
def sipi (h : Hv) : Hv :=
  { h with mp := .runnable }

/-- SEV-SNP AP creation (svm/sev.c:3856-3858, 3895-3897). -/
def snpReset (h : Hv) : Hv :=
  { h with pv := false, mp := .runnable }

def kernel : Kernel :=
  { name := "v6.14", halt, unhalt, present, kick, getMpState, setMpState, nestedExit,
    xenBlock, xenWake, initAp, sipi, snpReset }

end V614

/-! ## Linux v7.3.3 -/

namespace V733

/-- `kvm_set_mp_state` (x86.h:143-148). -/
def setMp (m : Mp) (h : Hv) : Hv :=
  { h with mp := m, pv := if m == .runnable then false else h.pv }

/-- `__kvm_emulate_halt` (x86.c:8693-8712). -/
def halt (ev : Bool) (st : Mp) (h : Hv) : Hv :=
  setMp (if ev || h.pv then .runnable else st) h

/-- The switch that ends `vcpu_block` (x86.c:8625-8638). -/
def unhalt (h : Hv) : Hv :=
  match h.mp with
  | .halted | .apResetHold => { setMp .runnable h with apfHalted := false }
  | .runnable => { h with apfHalted := false }
  | .initReceived | .uninitialized => h

/-- The tail of `kvm_arch_async_page_present` (x86.c:10603-10604): through the helper, so a
pending kick is cleared. -/
def present (h : Hv) : Hv :=
  setMp .runnable { h with apfHalted := false }

/-- `APIC_DM_REMRD` in `__apic_accept_irq` (lapic.c:1451-1456). -/
def kick (h : Hv) : Hv :=
  { h with pv := true }

/-- `kvm_arch_vcpu_ioctl_get_mpstate` (x86.c:9025-9030). -/
def getMpState (h : Hv) : Mp :=
  if (h.mp == .halted || h.mp == .apResetHold) && h.pv then .runnable else h.mp

/-- `kvm_arch_vcpu_ioctl_set_mpstate` (x86.c:9072). -/
def setMpState (m : Mp) (h : Hv) : Hv :=
  setMp m h

/-- `nested_vmx_vmexit` (vmx/nested.c:5187-5188), `nested_svm_vmexit` (svm/nested.c:1340-1341). -/
def nestedExit (h : Hv) : Hv :=
  setMp .runnable h

/-- `kvm_xen_schedop_poll` (xen.c:1581, 1592). -/
def xenBlock (h : Hv) : Hv := setMp .halted h
def xenWake (h : Hv) : Hv := setMp .runnable h

/-- INIT at an application processor: `kvm_apic_accept_events` (lapic.c:3617-3623) with
`kvm_vcpu_reset` (x86.c:9607-9609). -/
def initAp (h : Hv) : Hv :=
  { setMp .initReceived h with apfHalted := false }

/-- SIPI in wait-for-SIPI (lapic.c:3624-3633). -/
def sipi (h : Hv) : Hv :=
  setMp .runnable h

/-- SEV-SNP AP creation: `sev_snp_init_protected_guest_state` (svm/sev.c:4129-4146). -/
def snpReset (h : Hv) : Hv :=
  setMp .runnable (setMp .halted { h with pv := false })

def kernel : Kernel :=
  { name := "v7.3.3", halt, unhalt, present, kick, getMpState, setMpState, nestedExit,
    xenBlock, xenWake, initAp, sipi, snpReset }

end V733

/-! ## The VMM -/

/-- Firecracker's order for one vCPU, `KvmVcpu::restore_state`
(src/vmm/src/arch/x86_64/vcpu.rs:684-750 at 3e203733cb91): KVM_SET_MP_STATE (:709-711), the
registers, KVM_SET_LAPIC (:735-737), KVM_SET_MSRS (:738-743, MSR_IA32_TSC_DEADLINE in the last
chunk, :40-47 and :514-517), KVM_SET_VCPU_EVENTS. The ioctls between them that the model has no
state for (registers, XSAVE, debug registers, vCPU events) are not listed. Every vCPU is restored
before the VM's own state (src/vmm/src/builder.rs:475-494). -/
def firecracker : List Ioctl :=
  [.setMpState, .setLapic, .setMsrs]

namespace Today

/-- Host kernel v6.14 under Firecracker before the restore kick. -/
def v614 : Rules := { kernel := V614.kernel, restore := firecracker }

/-- Host kernel v7.3.3, unpatched, under Firecracker before the restore kick. -/
def v733 : Rules := { kernel := V733.kernel, restore := firecracker }

end Today

end Wake
