import WakeVerify.Today

/-!
# The repaired system

`Design.rules` is what fcvm runs on x86_64 since both repairs merged: host kernel v7.3.3 with the
patch of #2130, and the Firecracker fork with the restore kick of #2135.

- **REPAIR 1** (#2130, `kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-
  page-ready.patch`; #2146 carries the same patch in the x86 guest kernels that run KVM):
  `kvm_arch_async_page_present()` assigns `mp_state` and leaves the kick alone.
- **REPAIR 2** (issue #2131, fixed by #2135; ejc3/firecracker at 2a33289b55b7, paths under
  src/vmm/src/): `KvmVm::restore_kvm_state` (arch/x86_64/vm.rs:256-269) restores every vCPU's
  state, then the VM's, and ends with `kick_runnable_vcpus` (:299-326), which sends
  `KVM_SIGNAL_MSI` with the remote-read delivery mode to each vCPU that `pv_kick_target`
  (:109-128) selects: one saved RUNNABLE, in a guest that was offered `KVM_FEATURE_PV_UNHALT`.
  A vCPU saved in any other state gets none. The model's guest always has paravirtual
  spinlocks, so the model's condition is the saved `mp_state` alone (`Ioctl.kick`).

The other rule books here are the combinations in between, for the table in `BREAKS.md`.
-/

namespace Wake

namespace V733Patched

/-- REPAIR 1: the tail of `kvm_arch_async_page_present` with the patch applied. -/
def present (h : Hv) : Hv :=
  { h with apfHalted := false, mp := .runnable }

def kernel : Kernel :=
  { V733.kernel with name := "v7.3.3 with the #2130 patch", present }

end V733Patched

/-- REPAIR 2: Firecracker's order with the kick after everything else. The fork sends the kicks
after every vCPU's state and the VM's; nothing in the model runs between one vCPU's restore and
the next, so the kick as the last ioctl of each vCPU is the same order. -/
def firecrackerWithKick : List Ioctl :=
  firecracker ++ [.kick]

namespace Design

/-- Host kernel v7.3.3 with the #2130 patch, Firecracker with the restore kick (#2135). -/
def rules : Rules := { kernel := V733Patched.kernel, restore := firecrackerWithKick }

end Design

/-! The combinations that are neither today's nor the design: one repair without the other.
`v733Kick` is what a host at v6.15 or later without the #2130 patch runs under the fork fcvm
pins now, and `v614Kick` is what a v6.14 host runs under it. -/

namespace Partial

/-- v6.14 with the restore kick. -/
def v614Kick : Rules := { kernel := V614.kernel, restore := firecrackerWithKick }

/-- v7.3.3 unpatched with the restore kick. -/
def v733Kick : Rules := { kernel := V733.kernel, restore := firecrackerWithKick }

/-- v7.3.3 with the #2130 patch, Firecracker before the restore kick. -/
def v733Patched : Rules := { kernel := V733Patched.kernel, restore := firecracker }

end Partial

/-! Restore orders that no VMM here uses, to see which orders matter. -/

namespace Order

/-- The kick before `KVM_SET_MP_STATE`, v7.3.3 with the #2130 patch. -/
def kickFirst : Rules :=
  { kernel := V733Patched.kernel, restore := [.kick, .setMpState, .setLapic, .setMsrs] }

/-- The kick before `KVM_SET_MP_STATE`, v6.14. -/
def kickFirst614 : Rules :=
  { kernel := V614.kernel, restore := [.kick, .setMpState, .setLapic, .setMsrs] }

/-- `KVM_SET_MSRS` before `KVM_SET_LAPIC`. -/
def msrsFirst : Rules :=
  { kernel := V733Patched.kernel, restore := [.setMpState, .setMsrs, .setLapic, .kick] }

/-- No `KVM_SET_LAPIC` at all. -/
def noLapic : Rules :=
  { kernel := V733Patched.kernel, restore := [.setMpState, .setMsrs, .kick] }

end Order

end Wake
