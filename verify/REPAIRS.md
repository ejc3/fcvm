# Repairs: what each break needs changed, and where

`WakeVerify/Design.lean` is the repaired system: host kernel v7.3.3 with the patch of #2130
(REPAIR 1) and the Firecracker fork with the restore kick that fixes #2131 (REPAIR 2, #2135).
`Proofs.lean` proves every property of `Properties.lean` for it. Writing the proof, and running
the search on the combinations in between, turned up two things the two fixes did not say about
each other, and one rule the VMM has to keep (3, the one hypothesis the proof could not drop).

Both repairs are merged (#2130 and #2146 for the kernels, #2135 for the fork), by their own pull
requests: nothing here changes a kernel patch, the fork or fcvm. This file is what the model said
about each, what of that shipped, and what is still open: parts 2 and 3 of repair 3, and the
upstream fix (4).

## 1. A completion leaves the kick alone (#2130 host kernel, #2146 guest kernels; merged)

**The change.** `kvm_arch_async_page_present()` assigns `mp_state` instead of calling
`kvm_set_mp_state()`
(`kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-page-ready.patch`).

**In the model.** `V733Patched.present`, and the line `KeepsKick.present_keeps` that the proofs
rest on. v7.3.3 as released fails that line (`v733_not_keepsKick`); v6.14 has it.

**What the model adds.** The patch is sufficient for #2125 at both wait sites, with and without the
guest's async page fault interface, and it is the only change the kernel needs:
`v733Patched_sound_without_snapshots` proves that with it, and no snapshot, no kick is lost. The
other callers of `kvm_set_mp_state()` (nested VM-exit, Xen poll, INIT and SIPI, the SEV paths, the
end of `vcpu_block`, `__kvm_emulate_halt`) are in the model and need no change. `make
verify-harness` runs the patched function itself and it agrees with the model on every input.

**Where it is now.** #2146 carries the same change in the x86 guest kernels that run KVM
themselves (nested, phone, the mountinfo pair), so a VM restored inside such a guest keeps its
kick too. `kernel/README.md` says that the restore kick needs this patch. The harness here runs
the patched function for v7.3.3 only; the guest kernels' copies are covered by
`a_page_ready_completion_keeps_a_pending_pv_kick` (`tests/test_kernel_patch_invariants.rs`), not
by this directory.

**Proposed wording**, still open, for `kernel/README.md` where it says which callers of the
helper are left alone: "The other callers were checked with a model of the wake-up path
(`verify/`): each either leaves a halted state, where clearing the kick is the wake-up, or runs
where the vCPU is not waiting for a kick."

## 2. The restore gives every runnable vCPU a kick (#2131; fixed by #2135 in the Firecracker fork, merged)

**The change, as shipped.** `KvmVm::restore_kvm_state` (src/vmm/src/arch/x86_64/vm.rs:256-269 at
2a33289b55b7) restores every vCPU's state, then the VM's, and then calls `kick_runnable_vcpus`
(:299-326): `KVM_SIGNAL_MSI` with the remote-read delivery mode to each vCPU that was saved
RUNNABLE, in a guest that was offered `KVM_FEATURE_PV_UNHALT` (`pv_kick_target`, :109-128).

**In the model.** `firecrackerWithKick`, whose last ioctl kicks a vCPU saved RUNNABLE and no
other (`restoreOne`, `Ioctl.kick`), and `restore_inv`, the lemma that the restored VM satisfies
the invariant.

**What the model said, and what became of it.**

- *A vCPU saved HALTED needs no kick* (proved, `restore_inv`). If it had a kick pending it was
  saved RUNNABLE. If it had none and is owed one, the other vCPU is between its release of the lock
  byte and its kick, and sends the kick in the new VM. #2131 had proposed a kick for every vCPU.
  #2135 kicks the RUNNABLE ones only, and found on KVM that the others must not get one: with a
  kick recorded, `KVM_RUN` of an UNINITIALIZED vCPU returns `EAGAIN` at once, and of an
  INIT-received one spins in `vcpu_run()`. The model has no transition for either effect; its
  restore never kicked such a vCPU, so the design that is proved is the one that shipped.
- *The kick must come after `KVM_SET_MP_STATE`* on v6.15 and later (`breaks_kickFirst`). #2135
  sends it last, says so at the call, and its restore tests fail with the kick moved ahead of the
  vCPU restore.
- *The kick needs REPAIR 1* on v6.15 and later (found by the search, `breaks_wakeAll`, #2132).
  In a guest with async page faults on, the restore always leaves the wake-everyone completion on
  each vCPU's done list (`restore_queues_completion`). Without the patch, a vCPU that enters the
  guest with that completion and its page-ready slot free has no kick afterwards, whatever it had
  (`v733_entry_clears_kick`); with the patch the kick is kept (`v733Patched_entry_keeps_kick`).
  So on an unpatched v6.15+ host the restore kick is cleared for every vCPU whose slot was free in
  the snapshot. A vCPU whose slot was occupied keeps the kick, and did not need it: the completion
  stays on its done list and its HLT cannot block. On v6.14 the kick is enough by itself
  (`v614Kick_sound`). The doc comment of `kick_runnable_vcpus` now states this dependency and
  cites #2132.
- *The kick's test needs the async page fault MSRs, and a free slot.* A guest vCPU with
  `MSR_KVM_ASYNC_PF_EN` clear never queues the completion, so a test without them passes on an
  unpatched kernel. #2135 has the test this file proposed:
  `test_the_restore_kick_survives_the_page_ready_wakeup_of_the_restore` sets
  `MSR_KVM_ASYNC_PF_INT` and `MSR_KVM_ASYNC_PF_EN` (enabled, interrupt delivery) as Linux does,
  and asserts the wake-everyone token in the page-ready slot of the source and of the restored
  VM. On a distribution's 7.0.0 KVM it fails, the restored vCPU still in `KVM_RUN` at the 10 s
  bound, and with the one change of the #2130 patch loaded it passes (BREAKS.md, 3). That is #2132
  on hardware.

The kernel side of the same sequence is also run by `make verify-harness`:
`kvm_pv_enable_async_pf`, the kick, `kvm_check_async_pf_completion` and `__kvm_emulate_halt`, cut
from v7.3.3, leave the vCPU halted with `pv_unhalted` clear, and with the patch applied leave it
running.

## 3. The VMM never writes `mp_state` to a live VM (found by the proof; a requirement on the fork)

**What the proof found.** The design's theorems would not close for a VMM that reads `mp_state` of
a paused VM and writes it back. The state that blocks the proof is a runnable vCPU at its HLT in
`kvm_wait` with a kick recorded: `KVM_SET_MP_STATE` with RUNNABLE clears the kick on v6.15 and
later, with or without REPAIR 1 (`breaks_writeBack`). So the theorems carry the hypothesis
`Sound.noSet`, and the configuration that drops it is the last row of the table in BREAKS.md.

**The requirement.** The VMM never writes `mp_state` to a live VM: it issues `KVM_SET_MP_STATE`
only on the vCPUs of a VM that has not run. (The fork's comment also allows a write that is
followed by a kick. The model proves nothing about that case: its hypothesis is no write at all.)

**Why it matters.** It is true of the fork today: outside its tests, `KVM_SET_MP_STATE` is issued
on x86_64 in one place, `KvmVcpu::restore_state` (src/vmm/src/arch/x86_64/vcpu.rs:713-715 at
2a33289b55b7), and that function is called only from `KvmVm::restore_kvm_state` on the new vCPUs
of `build_microvm_from_snapshot` (arch/x86_64/vm.rs:264, builder.rs:499). Since #2135 a comment
at the call says so (part 1 below). Nothing enforces it. A later change that restores state in place (a snapshot loaded into the running VM, a register write-back
after a debugger stop, a "reset vCPU" API) would bring the hang back with every test of today
still passing.

**The repair, in the Firecracker fork** (three parts, smallest first):

1. *A comment at the call.* Done in #2135 (vcpu.rs:709-712): "KVM_SET_MP_STATE to runnable
   clears a pending paravirtual kick on Linux 6.15 and later. It is safe here because the vCPU is
   new and `KvmVm::kick_runnable_vcpus` runs after it. Never call it on a vCPU of a VM that has
   run without giving that vCPU a kick afterwards."
2. *A debug assertion that the vCPU has never run.* `KvmVcpu` gets a `has_run: bool` (or the
   existing run-state of its `Vcpu`), set on the first `KVM_RUN`, and `restore_state` starts with
   `debug_assert!(!self.has_run, "restore_state on a vCPU that has run: KVM_SET_MP_STATE would
   clear a pending paravirtual kick")`.
3. *A test that fails if a second writer appears.* A source-level test in the fork, in the style
   of fcvm's own order pins: `set_mp_state(` occurs once under `src/vmm/src/arch/x86_64` outside
   test modules, in `restore_state`; `restore_state` on a `KvmVcpu` is called from `build_microvm_from_snapshot`
   and nowhere else outside tests; and in `restore_kvm_state` the kick follows the last
   `restore_state`. With the assertion of part 2, a unit test that runs a vCPU once and then calls
   `restore_state` must panic in a debug build.

Part 1 shipped with the restore kick. Parts 2 and 3 are not written; they are what keep the
model's hypothesis true after the next refactor. Of part 3, #2135's tests already pin the last
clause: with the kick moved ahead of the vCPU restore, the restore tests fail.

## 4. Hosts fcvm does not build the kernel for

On a host at v6.15 or later without REPAIR 1, findings 1 and 3 are both open. Finding 1 cannot be
closed in the VMM: the completion that clears a guest's own kick runs inside KVM at any time.
For finding 3 alone #2135 observed a VMM-side route (one `KVM_RUN` with a signal pending after the
state restore, which delivers the restore's completion, and the kick after it) and did not adopt
it, because such a host loses the guest's own kicks to finding 1 anyway. That order is not in this
model.

**The remedy is the upstream fix.** Linux master has the code of v7.3.3 here. The #2130 patch, with
its reproduction and the trace of #2132, is the change to send, and until a host carries it (from
upstream, a stable backport, or fcvm's own kernel build) that host has both hangs.

No guest-side workaround is proposed. Booting guests with `nopvspin` removes `kvm_wait()` and with
it the kick, but the owner has declined guest-side workarounds for this defect, it gives up halting
waiters under oversubscription (the case PV spinlocks exist for), and its cost there is not
measured. It is not modelled either.

## What was not repaired

Nothing was found in the guest's own protocol (`NoStrandedNode` and `MutualExclusion` hold in every
rule book), in the pause and resume of a source VM, or in how a pending interrupt or timer crosses
a restore in Firecracker's order.
