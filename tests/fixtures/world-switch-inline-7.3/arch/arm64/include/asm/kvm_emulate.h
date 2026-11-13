/*
 * Verbatim line range from Linux v7.3.3 arch/arm64/include/asm/kvm_emulate.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/include/asm/kvm_emulate.h
 * Complete source SHA256: 92f7093348c599dee6b7901c24b6685fd6ff9d2caaf447f397b73477cf1c8944
 * Lines 255-268.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/world-switch-helpers-inline.patch to this file,
 * then checks the definitions it names.
 */
static inline bool is_nested_ctxt(struct kvm_vcpu *vcpu)
{
	return vcpu_has_nv(vcpu) && !is_hyp_ctxt(vcpu);
}

static inline bool vserror_state_is_nested(struct kvm_vcpu *vcpu)
{
	if (!is_nested_ctxt(vcpu))
		return false;

	return vcpu_el2_amo_is_set(vcpu) ||
	       (__vcpu_sys_reg(vcpu, HCRX_EL2) & HCRX_EL2_TMEA);
}

