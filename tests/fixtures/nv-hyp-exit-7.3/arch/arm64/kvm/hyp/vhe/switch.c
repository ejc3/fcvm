/*
 * Verbatim line ranges from Linux v7.3.3 arch/arm64/kvm/hyp/vhe/switch.c:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/kvm/hyp/vhe/switch.c
 * Complete source SHA256: 5189d2c878c16cccf8ee8ca1356522b34b259e4070ad84eb92d367d62330f846
 * Lines 50-117, 556-591, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/nv-guest-hypervisor-exits-without-calls.patch to this file,
 * then compiles the functions it names from it into a C harness.
 */
#define NV_HCR_GUEST_EXCLUDE	(HCR_TGE | HCR_API | HCR_APK | HCR_FIEN)

static u64 __compute_hcr(struct kvm_vcpu *vcpu)
{
	u64 guest_hcr, hcr = vcpu->arch.hcr_el2;

	if (!vcpu_has_nv(vcpu))
		return hcr;

	/*
	 * We rely on the invariant that a vcpu entered from HYP
	 * context must also exit in the same context, as only an ERET
	 * instruction can kick us out of it, and we obviously trap
	 * that sucker. PSTATE.M will get fixed-up on exit.
	 */
	if (is_hyp_ctxt(vcpu)) {
		host_data_set_flag(VCPU_IN_HYP_CONTEXT);

		hcr |= HCR_NV | HCR_NV2 | HCR_AT | HCR_TTLB;

		if (!vcpu_el2_e2h_is_set(vcpu))
			hcr |= HCR_NV1;

		/* Publish the guest's view of HCR_EL2 to the HW */
		if (cpus_have_final_cap(ARM64_HAS_NV3) && vcpu_el2_e2h_is_set(vcpu))
			write_sysreg_s(__vcpu_sys_reg(vcpu, HCR_EL2), SYS_NVHCR_EL2);
		else
			__vcpu_assign_sys_reg(vcpu, NVHCR_EL2, __vcpu_sys_reg(vcpu, HCR_EL2));

		/*
		 * Nothing in HCR_EL2 should impact running in hypervisor
		 * context, apart from bits we have defined as RESx (E2H,
		 * HCD and co), or that cannot be set directly (the EXCLUDE
		 * bits). Given that we OR the guest's view with the host's,
		 * we can use the 0 value as the starting point, and only
		 * use the config-driven RES1 bits.
		 */
		guest_hcr = kvm_vcpu_apply_reg_masks(vcpu, HCR_EL2, 0);

		write_sysreg_s(vcpu->arch.ctxt.vncr_array, SYS_VNCR_EL2);
	} else {
		host_data_clear_flag(VCPU_IN_HYP_CONTEXT);

		guest_hcr = __vcpu_sys_reg(vcpu, HCR_EL2);
		if (guest_hcr & HCR_NV) {
			u64 va = __fix_to_virt(vncr_fixmap(smp_processor_id()));

			/* Inherit the low bits from the actual register */
			va |= __vcpu_sys_reg(vcpu, VNCR_EL2) & GENMASK(PAGE_SHIFT - 1, 0);
			write_sysreg_s(va, SYS_VNCR_EL2);

			/* Force NV2 in case the guest is forgetful... */
			guest_hcr |= HCR_NV2;
		}

		/*
		 * Exclude the guest's TWED configuration if it hasn't set TWE
		 * to avoid potentially delaying traps for the host.
		 */
		if (!(guest_hcr & HCR_TWE))
			guest_hcr &= ~(HCR_EL2_TWEDEn | HCR_EL2_TWEDEL);
	}

	BUG_ON(host_data_test_flag(VCPU_IN_HYP_CONTEXT) &&
	       host_data_test_flag(L1_VNCR_MAPPED));

	return hcr | (guest_hcr & ~NV_HCR_GUEST_EXCLUDE);
}

static void fixup_nv_guest_exit(struct kvm_vcpu *vcpu)
{
	/*
	 * If we were in HYP context on entry, adjust the PSTATE view
	 * so that the usual helpers work correctly. This enforces our
	 * invariant that the guest's HYP context status is preserved
	 * across a run.
	 */
	if (unlikely(host_data_test_flag(VCPU_IN_HYP_CONTEXT))) {
		u64 mode = *vcpu_cpsr(vcpu) & (PSR_MODE_MASK | PSR_MODE32_BIT);
		u64 hcr;

		switch (mode) {
		case PSR_MODE_EL1t:
			mode = PSR_MODE_EL2t;
			break;
		case PSR_MODE_EL1h:
			mode = PSR_MODE_EL2h;
			break;
		}

		*vcpu_cpsr(vcpu) &= ~(PSR_MODE_MASK | PSR_MODE32_BIT);
		*vcpu_cpsr(vcpu) |= mode;

		/* Publish the latest HCR_EL2 to the emulation */
		hcr = (cpus_have_final_cap(ARM64_HAS_NV3) &&
		       vcpu_el2_e2h_is_set(vcpu)) ?
			read_sysreg_s(SYS_NVHCR_EL2) :
			__vcpu_sys_reg(vcpu, NVHCR_EL2);

		__vcpu_assign_sys_reg(vcpu, HCR_EL2, hcr);
	}

	/* Apply extreme paranoia! */
	BUG_ON(!!host_data_test_flag(VCPU_IN_HYP_CONTEXT) != is_hyp_ctxt(vcpu));
}
