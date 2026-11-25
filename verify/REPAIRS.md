# Repairs: what each break needs changed, and where

`WakeVerify/Design.lean` is the repaired system: host kernel v7.3.3 with the patch of #2130
(REPAIR 1) and the Firecracker fork with the restore kick of #2131 (REPAIR 2). `Proofs.lean` proves
every property of `Properties.lean` for it. Writing the proof, and running the search on the
combinations in between, turned up two things the two fixes did not say about each other, and one
rule the VMM has to keep. Nothing here changes a kernel patch, the fork or fcvm: those changes
belong to #2130 and #2131, and this file is what the model says about them.

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

## 2. The restore gives every runnable vCPU a kick (#2131, Firecracker fork)

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

## 3. The VMM never writes `mp_state` to a live VM (found by the proof)

**What the proof found.** The design's theorems would not close for a VMM that reads `mp_state` of
a paused VM and writes it back. The state that blocks the proof is a runnable vCPU at its HLT in
`kvm_wait` with a kick recorded: `KVM_SET_MP_STATE` with RUNNABLE clears the kick on v6.15 and
later, with or without REPAIR 1 (`breaks_writeBack`). So the theorems carry the hypothesis
`Sound.noSet`, and the configuration that drops it is the last row of the table in BREAKS.md.

**Why it matters.** It is true of Firecracker today, where `KVM_SET_MP_STATE` is issued only on the
vCPUs of a VM being built from a snapshot. It is not written down anywhere, and a later change that
restores state in place (a snapshot loaded into the running VM, a register write-back after a
debugger stop) would bring the hang back with every test of today still passing.

**Proposed wording** for the comment above `set_mp_state` in the fork's `restore_state`:
"`KVM_SET_MP_STATE` to RUNNABLE clears a pending paravirtual kick on Linux v6.15 and later. It is
safe here because the vCPU is new and `kick_runnable_vcpus` runs after it. Never call it on a vCPU
of a VM that has run without giving that vCPU a kick afterwards."

## 4. Hosts fcvm does not build the kernel for (not a change to the model)

On a host at v6.15 or later without REPAIR 1, findings 1 and 3 are both open and nothing in the VMM
closes them: the completion that clears the kick runs inside KVM. Two things would:

- **Upstream.** Linux master has the code of v7.3.3 here. The #2130 patch, with its reproduction,
  is the change to send.
- **The guest.** A guest booted with `nopvspin` has no `kvm_wait()`, so no kick to lose (#2130's
  measurements: 0 of 200 restores stuck with it, against 15 of 900 without). fcvm chooses the guest's
  command line, so it could add `nopvspin` when the host kernel is v6.15 or later and is not one
  fcvm built, at the cost of spinning where a waiter would have halted. This is not modelled (a
  lock that only spins has no HLT, and the model's lock paths would simply not reach one), and the
  cost on an oversubscribed host is not measured.

## What was not repaired

Nothing was found in the guest's own protocol (`NoStrandedNode` and `MutualExclusion` hold in every
rule book), in the pause and resume of a source VM, or in how a pending interrupt or timer crosses
a restore in Firecracker's order.
