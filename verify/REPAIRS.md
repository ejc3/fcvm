# Repairs: what each break needs changed, and where

`WakeVerify/Design.lean` is the repaired system: host kernel v7.3.3 with the patch of #2130
(REPAIR 1) and the Firecracker fork with the restore kick of #2131 (REPAIR 2). `Proofs.lean` proves
every property of `Properties.lean` for it. Writing the proof, and running the search on the
combinations in between, turned up two things the two fixes did not say about each other, and one
rule the VMM has to keep (3, the one hypothesis the proof could not drop). Nothing here changes a
kernel patch, the fork or fcvm: those changes belong to #2130 and #2131, and this file is what the
model says about them.

## 1. A completion leaves the kick alone (#2130, host kernel, merged)

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

**Proposed wording** for `kernel/README.md`, where it says which callers of the helper are left
alone (the patch is merged): "The other callers were checked with a model of the wake-up path
(`verify/`): each either leaves a halted state, where clearing the kick is the wake-up, or runs
where the vCPU is not waiting for a kick. The patch is also what makes the restore kick of #2131
hold (#2132)."

## 2. The restore gives every runnable vCPU a kick (#2131, Firecracker fork; for the owner of that fix)

**The change.** After every vCPU's state and the VM's state are restored, `KVM_SIGNAL_MSI` with the
remote-read delivery mode to each vCPU that was saved RUNNABLE.

**In the model.** `firecrackerWithKick`, and `restore_inv`, the lemma that the restored VM
satisfies the invariant.

**What the model adds.**

- *A vCPU saved HALTED needs no kick* (proved, `restore_inv`). If it had a kick pending it was
  saved RUNNABLE. If it had none and is owed one, the other vCPU is between its release of the lock
  byte and its kick, and sends the kick in the new VM.
- *The kick must come after `KVM_SET_MP_STATE`* on v6.15 and later (`breaks_kickFirst`). The change
  already does this, and says so.
- *The kick needs REPAIR 1* on v6.15 and later (found by the search, `breaks_wakeAll`, #2132).
  Restoring `MSR_KVM_ASYNC_PF_EN` queues a wake-everyone completion, and without the patch the
  vCPU's first entry clears the kick through `kvm_arch_async_page_present()`. On v6.14 the kick is
  enough by itself (`v614Kick_sound`).
- *The kick's test needs the async page fault MSRs.* A guest vCPU with `MSR_KVM_ASYNC_PF_EN` clear
  never queues the completion, so the test passes on an unpatched kernel.

**Proposed wording** for the doc comment of `kick_runnable_vcpus` in the fork: "On Linux v6.15 and
later this needs the host kernel to keep a kick across an async page-ready completion (the fcvm
host kernel patch `kvm-x86-keep-a-pending-pv-kick-across-async-page-ready`): restoring
`MSR_KVM_ASYNC_PF_EN` queues a completion that the vCPU handles on its first entry, and an
unpatched `kvm_arch_async_page_present()` clears the kick there (fcvm #2132)."

**Proposed test** in the fork, beside the existing one: the same guest (interrupts off, a port
write, HLT), with `MSR_KVM_ASYNC_PF_INT` set to a vector and `MSR_KVM_ASYNC_PF_EN` set to a guest
address with the enable and interrupt-delivery bits, written with `KVM_SET_MSRS` before the kick.
Expected: the vCPU runs past its HLT on a kernel with REPAIR 1 and on v6.14, and blocks on v6.15
and later without it. On an unpatched host that is the reproduction of #2132.

## 3. The VMM never writes `mp_state` to a live VM (found by the proof; for the owner of the #2131 fix)

**What the proof found.** The design's theorems would not close for a VMM that reads `mp_state` of
a paused VM and writes it back. The state that blocks the proof is a runnable vCPU at its HLT in
`kvm_wait` with a kick recorded: `KVM_SET_MP_STATE` with RUNNABLE clears the kick on v6.15 and
later, with or without REPAIR 1 (`breaks_writeBack`). So the theorems carry the hypothesis
`Sound.noSet`, and the configuration that drops it is the last row of the table in BREAKS.md.

**Why it matters.** It is true of the fork today: `KVM_SET_MP_STATE` is issued in one place,
`KvmVcpu::restore_state` (src/vmm/src/arch/x86_64/vcpu.rs:709-711 at 3e203733cb91), and that
function is called only on the new vCPUs of `build_microvm_from_snapshot`
(src/vmm/src/builder.rs:475-480). It is written down nowhere, and nothing enforces it. A later
change that restores state in place (a snapshot loaded into the running VM, a register write-back
after a debugger stop, a "reset vCPU" API) would bring the hang back with every test of today
still passing.

**The repair, in the Firecracker fork** (three parts, smallest first):

1. *A comment at the call.* Proposed wording, above `set_mp_state` in `restore_state`:
   "`KVM_SET_MP_STATE` to RUNNABLE clears a pending paravirtual kick (`pv_unhalted`) on Linux v6.15
   and later. It is safe here because this vCPU is new and has never run, and
   `kick_runnable_vcpus` runs after it. Never call it on a vCPU that has run without giving that
   vCPU a kick afterwards: a waiter in the guest's `kvm_wait()` would halt with its wake-up gone
   (fcvm `verify/REPAIRS.md`, 3)."
2. *A debug assertion that the vCPU has never run.* `KvmVcpu` gets a `has_run: bool` (or the
   existing run-state of its `Vcpu`), set on the first `KVM_RUN`, and `restore_state` starts with
   `debug_assert!(!self.has_run, "restore_state on a vCPU that has run: KVM_SET_MP_STATE would
   clear a pending paravirtual kick")`.
3. *A test that fails if a second writer appears.* A source-level test in the fork, in the style
   of fcvm's own order pins: `set_mp_state(` occurs once in `src/vmm/src` outside test modules, in
   `restore_state`; `restore_state` on a `KvmVcpu` is called from `build_microvm_from_snapshot`
   and nowhere else outside tests; and in `restore_kvm_state` the kick follows the last
   `restore_state`. With the assertion of part 2, a unit test that runs a vCPU once and then calls
   `restore_state` must panic in a debug build.

Part 1 belongs in the same change as the restore kick. Parts 2 and 3 are what keep the model's
hypothesis true after the next refactor.

## 4. Hosts fcvm does not build the kernel for

On a host at v6.15 or later without REPAIR 1, findings 1 and 3 are both open and nothing in the VMM
closes them: the completion that clears the kick runs inside KVM.

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
