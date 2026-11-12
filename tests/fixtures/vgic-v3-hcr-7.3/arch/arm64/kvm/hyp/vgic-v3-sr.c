/*
 * Verbatim line range from Linux v7.3.3 arch/arm64/kvm/hyp/vgic-v3-sr.c:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/kvm/hyp/vgic-v3-sr.c
 * Complete source SHA256: 8369587de161f54ad621313eea6f4073d3bbe1c621f1802cec2882836a5759bc
 * Lines 198-368.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/vgic-v3-keep-hcr-on-quiet-exits.patch
 * to this file, then compiles the functions it names from it into a C harness.
 */
	return val;
}

static u64 compute_ich_hcr(struct vgic_v3_cpu_if *cpu_if)
{
	return cpu_if->vgic_hcr | vgic_ich_hcr_trap_bits();
}

void __vgic_v3_save_state(struct vgic_v3_cpu_if *cpu_if)
{
	u64 used_lrs = cpu_if->used_lrs;

	/*
	 * Make sure stores to the GIC via the memory mapped interface
	 * are now visible to the system register interface when reading the
	 * LRs, and when reading back the VMCR on non-VHE systems.
	 */
	if (used_lrs || !has_vhe()) {
		if (!cpu_if->vgic_sre) {
			dsb(sy);
			isb();
		}
	}

	if (used_lrs) {
		int i;
		u32 elrsr;

		elrsr = read_gicreg(ICH_ELRSR_EL2);

		for (i = 0; i < used_lrs; i++) {
			if (elrsr & (1 << i))
				cpu_if->vgic_lr[i] &= ~ICH_LR_STATE;
			else
				cpu_if->vgic_lr[i] = __gic_v3_get_lr(i);

			__gic_v3_set_lr(0, i);
		}
	}

	cpu_if->vgic_vmcr = read_gicreg(ICH_VMCR_EL2);

	if (cpu_if->vgic_hcr & ICH_HCR_EL2_LRENPIE) {
		u64 val = read_gicreg(ICH_HCR_EL2);
		cpu_if->vgic_hcr &= ~ICH_HCR_EL2_EOIcount;
		cpu_if->vgic_hcr |= val & ICH_HCR_EL2_EOIcount;
	}

	write_gicreg(0, ICH_HCR_EL2);

	/*
	 * Hack alert: On NV, this results in a trap so that the above write
	 * actually takes effect... No synchronisation is necessary, as we
	 * only care about the effects when this traps.
	 */
	read_gicreg(ICH_MISR_EL2);
}

void __vgic_v3_restore_state(struct vgic_v3_cpu_if *cpu_if)
{
	u64 used_lrs = cpu_if->used_lrs;
	int i;

	write_gicreg(compute_ich_hcr(cpu_if), ICH_HCR_EL2);

	for (i = 0; i < used_lrs; i++)
		__gic_v3_set_lr(cpu_if->vgic_lr[i], i);

	/*
	 * Ensure that writes to the LRs, and on non-VHE systems ensure that
	 * the write to the VMCR in __vgic_v3_activate_traps(), will have
	 * reached the (re)distributors. This ensure the guest will read the
	 * correct values from the memory-mapped interface.
	 */
	if (used_lrs || !has_vhe()) {
		if (!cpu_if->vgic_sre) {
			isb();
			dsb(sy);
		}
	}
}

void __vgic_v3_activate_traps(struct vgic_v3_cpu_if *cpu_if)
{
	/*
	 * VFIQEn is RES1 if ICC_SRE_EL1.SRE is 1. This causes a
	 * Group0 interrupt (as generated in GICv2 mode) to be
	 * delivered as a FIQ to the guest, with potentially fatal
	 * consequences. So we must make sure that ICC_SRE_EL1 has
	 * been actually programmed with the value we want before
	 * starting to mess with the rest of the GIC, and VMCR_EL2 in
	 * particular.  This logic must be called before
	 * __vgic_v3_restore_state().
	 *
	 * However, if the vgic is disabled (ICH_HCR_EL2.EN==0), no GIC is
	 * provisioned at all. In order to prevent illegal accesses to the
	 * system registers to trap to EL1 (duh), force ICC_SRE_EL1.SRE to 1
	 * so that the trap bits can take effect. Yes, we *loves* the GIC.
	 */
	if (!(cpu_if->vgic_hcr & ICH_HCR_EL2_En)) {
		write_gicreg(ICC_SRE_EL1_SRE, ICC_SRE_EL1);
		isb();
	} else if (!cpu_if->vgic_sre) {
		write_gicreg(0, ICC_SRE_EL1);
		isb();
		write_gicreg(cpu_if->vgic_vmcr, ICH_VMCR_EL2);


		if (has_vhe()) {
			/*
			 * Ensure that the write to the VMCR will have reached
			 * the (re)distributors. This ensure the guest will
			 * read the correct values from the memory-mapped
			 * interface.
			 */
			isb();
			dsb(sy);
		}
	}

	/* Only disable SRE if the host implements the GICv2 interface */
	if (static_branch_unlikely(&vgic_v3_has_v2_compat)) {
		/*
		 * Prevent the guest from touching the ICC_SRE_EL1 system
		 * register. Note that this may not have any effect, as
		 * ICC_SRE_EL2.Enable being RAO/WI is a valid implementation.
		 */
		write_gicreg(read_gicreg(ICC_SRE_EL2) & ~ICC_SRE_EL2_ENABLE,
			     ICC_SRE_EL2);
	}

	/*
	 * If we need to trap system registers, we must write ICH_HCR_EL2
	 * anyway, even if no interrupts are being injected. Note that this
	 * also applies if we don't expect any system register access (no
	 * vgic at all). In any case, no need to provide MI configuration.
	 */
	if (static_branch_unlikely(&vgic_v3_cpuif_trap) ||
	    cpu_if->its_vpe.its_vm || !cpu_if->vgic_sre)
		write_gicreg(vgic_ich_hcr_trap_bits() | ICH_HCR_EL2_En, ICH_HCR_EL2);
}

void __vgic_v3_deactivate_traps(struct vgic_v3_cpu_if *cpu_if)
{
	u64 val;

	/* Only restore SRE if the host implements the GICv2 interface */
	if (static_branch_unlikely(&vgic_v3_has_v2_compat)) {
		val = read_gicreg(ICC_SRE_EL2);
		write_gicreg(val | ICC_SRE_EL2_ENABLE, ICC_SRE_EL2);

		if (!cpu_if->vgic_sre) {
			/* Make sure ENABLE is set at EL2 before setting SRE at EL1 */
			isb();
			write_gicreg(1, ICC_SRE_EL1);
		}
	}

	/*
	 * If we were trapping system registers, we enabled the VGIC even if
	 * no interrupts were being injected, and we disable it again here.
	 */
	if (static_branch_unlikely(&vgic_v3_cpuif_trap) ||
	    cpu_if->its_vpe.its_vm || !cpu_if->vgic_sre)
		write_gicreg(0, ICH_HCR_EL2);
}

void __vgic_v3_save_aprs(struct vgic_v3_cpu_if *cpu_if)
{
	u64 val;
	u32 nr_pre_bits;
