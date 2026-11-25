import ResumeVerify.Spec

/-!
# The VMMs as they are

One rule book per VMM as fcvm pins it, every rule with the file and line it was read from.
"Read" means read in the source at the named commit; nothing here was observed in a running
VM. Host KVM is Linux v7.3.3 for all of them (`Spec.lean`).
-/

namespace Resume

namespace Today

/-- Cloud Hypervisor, ejc3/cloud-hypervisor a354afd77 (branch fcvm-v52-sve-fix).

* **The save reads vCPU 0's redistributor and CPU interface for every vCPU of a VM that was
  booted, not restored.** The vGIC is created with `gicr_typers: vec![0; vcpu_count]`
  (hypervisor/src/kvm/aarch64/gic/mod.rs:274). `state()` passes that list to `get_redist_regs`
  and `get_icc_regs` (mod.rs:334-343), which put `typer & MPIDR_MASK` in each request
  (redist_regs.rs:114-122, icc_regs.rs:99-107). MPIDR 0 is vCPU 0 (v7.3.3
  vgic-kvm-device.c:484-509, arm.c:2931-2960). The one caller of `set_gicr_typers` is the
  restore (devices/src/gic.rs:66-71, from vmm/src/device_manager.rs:1809-1813), so a restored
  VM saves each vCPU's own.
* The image's counter is the one read at the save: `Vcpu::snapshot` calls `vcpu.state()`
  (vmm/src/cpu.rs:692-703), which reads every register of `KVM_GET_REG_LIST`
  (hypervisor/src/kvm/mod.rs:2921-2988). `pause` and `resume` leave the counter offset alone
  (vmm/src/cpu.rs:2623-2683, vmm/src/vm.rs:3256-3325): nothing in the tree issues
  `KVM_ARM_SET_COUNTER_OFFSET`.
* No request for `KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO` anywhere in the tree.
* The restore makes each vCPU and restores it before it makes the next (`create_vcpu`,
  vmm/src/cpu.rs:974-1027, called per vCPU from `create_boot_vcpus`, cpu.rs:1525; vCPUs before
  devices, vmm/src/vm.rs:974, 1059): `KVM_ARM_VCPU_INIT` with `POWER_OFF` for every vCPU but
  the first (hypervisor/src/kvm/mod.rs:2607-2610), the registers in `KVM_GET_REG_LIST` order,
  in which `CNTPCT_EL0` precedes `CNTVCT_EL0` (v7.3.3 sys_regs.c:3748-3751, 5720-5733), then
  `KVM_SET_MP_STATE` (mod.rs:3150-3180). Then the vGIC: `GICD_CTLR` and the distributor, the
  redistributors of all vCPUs, the CPU interfaces of all vCPUs (gic/mod.rs:402-413). -/
def cloudHypervisor : Rules :=
  { name := "cloud-hypervisor",
    slotsFromRestore := true, cntAtPause := false, freeze := false, freezeLoaded := false,
    levelInfo := false,
    restore := [.create false, .init false false, .cntp false, .cntv false, .regs false, .mp false,
                .create true, .init true true, .cntp true, .cntv true, .regs true, .mp true,
                .gic, .dist, .redist false, .redist true, .icc false, .icc true] }

/-- The order of a Firecracker restore (src/vmm/src/builder.rs `build_microvm_from_snapshot`):
all vCPUs made, with the vGIC after them (`Vm::create_vcpus`), then each vCPU's
`KVM_ARM_VCPU_INIT` from the saved features with `POWER_OFF` cleared
(src/vmm/src/arch/aarch64/vcpu.rs `save_state`, `restore_state`), its registers in the saved
order and `KVM_SET_MP_STATE`, then the vGIC: the distributor, then per vCPU its redistributor
and its CPU interface (src/vmm/src/arch/aarch64/gic/gicv3/regs/mod.rs `restore_state`). -/
def fcOrder (offset : Bool) : List Op :=
  [.create false, .create true, .gic] ++ (if offset then [.offset] else []) ++
  [.init false false, .cntp false, .cntv false, .regs false, .mp false,
   .init true false, .cntp true, .cntv true, .regs true, .mp true,
   .dist, .redist false, .icc false, .redist true, .icc true]

/-- fcvm's Firecracker fork before the fix of #630, ejc3/firecracker ba79650e (NV2 support, no
counter code): the registers are replayed as saved and nothing owns the counter offset. Each
vCPU's redistributor and CPU interface is addressed by that vCPU's saved MPIDR
(src/vmm/src/lib.rs `construct_kvm_mpidrs`, used by `save_state`). -/
def forkBefore630 : Rules :=
  { name := "fork-before-630",
    slotsFromRestore := false, cntAtPause := false, freeze := false, freezeLoaded := false,
    levelInfo := false, restore := fcOrder false }

/-- The fork as pinned for every profile but the nested arm64 one, ejc3/firecracker
2a33289b55b7 (rootfs-config.toml `[firecracker]`). It does not carry 0baf5b0b: for the
counter it is `forkBefore630`. Its guests are not hypervisors. -/
def forkDefault : Rules := { forkBefore630 with name := "fork-default" }

/-- The fork as pinned for the nested arm64 profile, ejc3/firecracker dbea5779fb37, which has
0baf5b0b and 865fcab0: `KVM_ARM_SET_COUNTER_OFFSET` at boot, before the register replay of a
restore from vCPU 0's saved `CNTPCT_EL0` (src/vmm/src/builder.rs:520-535), and on `resume_vm`
moved by the time since `pause_vm` (src/vmm/src/lib.rs:478-524). A restored VM starts with
`paused_at_counter: None`, so the time between its load and its resume is not taken out. -/
def forkNested : Rules :=
  { name := "fork-nested",
    slotsFromRestore := false, cntAtPause := false, freeze := true, freezeLoaded := false,
    levelInfo := false, restore := fcOrder true }

end Today

end Resume
