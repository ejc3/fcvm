import WakeVerify.Today

/-!
# The repaired system

`Design.rules` is what fcvm is to run on x86_64: host kernel v7.3.3 with the patch of #2130, and
the Firecracker fork with the restore kick of #2131.

- **REPAIR 1** (#2130, `kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-
  page-ready.patch`): `kvm_arch_async_page_present()` assigns `mp_state` and leaves the kick alone.
- **REPAIR 2** (#2131, Firecracker fork): after every vCPU's state is set, the restore gives each
  vCPU that was saved runnable a kick (`KVM_SIGNAL_MSI`, remote-read delivery mode).

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

/-- REPAIR 2: Firecracker's order with the kick after everything else. -/
def firecrackerWithKick : List Ioctl :=
  firecracker ++ [.kick]

namespace Design

/-- Host kernel v7.3.3 with the #2130 patch, Firecracker with the #2131 restore kick. -/
def rules : Rules := { kernel := V733Patched.kernel, restore := firecrackerWithKick }

end Design

/-! The combinations that are neither today's nor the design: one repair without the other. -/

namespace Partial

/-- v6.14 with the restore kick. -/
def v614Kick : Rules := { kernel := V614.kernel, restore := firecrackerWithKick }

/-- v7.3.3 unpatched with the restore kick. -/
def v733Kick : Rules := { kernel := V733.kernel, restore := firecrackerWithKick }

/-- v7.3.3 with the #2130 patch, Firecracker as pinned (no restore kick). -/
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
