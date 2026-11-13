/*
 * Verbatim line ranges from Linux v7.3.3 arch/arm64/kvm/hyp/include/hyp/sysreg-sr.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/kvm/hyp/include/hyp/sysreg-sr.h
 * Complete source SHA256: 70f38f09833d451a375c02629af9594590dc72033506c9e41b708e2a0c121cb7
 * Lines 15-64, 195-207, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/world-switch-helpers-inline.patch to this file,
 * then checks the definitions it names.
 */
#include <asm/kvm_emulate.h>
#include <asm/kvm_hyp.h>
#include <asm/kvm_mmu.h>

static inline bool ctxt_has_s1poe(struct kvm_cpu_context *ctxt);

static inline struct kvm_vcpu *ctxt_to_vcpu(struct kvm_cpu_context *ctxt)
{
	struct kvm_vcpu *vcpu = ctxt->__hyp_running_vcpu;

	if (!vcpu)
		vcpu = container_of(ctxt, struct kvm_vcpu, arch.ctxt);

	return vcpu;
}

static inline bool ctxt_is_guest(struct kvm_cpu_context *ctxt)
{
	return host_data_ptr(host_ctxt) != ctxt;
}

static inline u64 *ctxt_mdscr_el1(struct kvm_cpu_context *ctxt)
{
	struct kvm_vcpu *vcpu = ctxt_to_vcpu(ctxt);

	if (ctxt_is_guest(ctxt) && kvm_host_owns_debug_regs(vcpu))
		return &vcpu->arch.external_mdscr_el1;

	return &ctxt_sys_reg(ctxt, MDSCR_EL1);
}

static inline u64 ctxt_midr_el1(struct kvm_cpu_context *ctxt)
{
	struct kvm *kvm = kern_hyp_va(ctxt_to_vcpu(ctxt)->kvm);

	if (!(ctxt_is_guest(ctxt) &&
	      test_bit(KVM_ARCH_FLAG_WRITABLE_IMP_ID_REGS, &kvm->arch.flags)))
		return read_cpuid_id();

	return kvm_read_vm_id_reg(kvm, SYS_MIDR_EL1);
}

static inline void __sysreg_save_common_state(struct kvm_cpu_context *ctxt)
{
	*ctxt_mdscr_el1(ctxt)	= read_sysreg(mdscr_el1);

	// POR_EL0 can affect uaccess, so must be saved/restored early.
	if (ctxt_has_s1poe(ctxt))
		ctxt_sys_reg(ctxt, POR_EL0)	= read_sysreg_s(SYS_POR_EL0);
}

		ctxt_sys_reg(ctxt, DISR_EL1) = read_sysreg_s(SYS_VDISR_EL2);
	else if (ctxt_has_ras(ctxt))
		ctxt_sys_reg(ctxt, VDISR_EL2) = read_sysreg_s(SYS_VDISR_EL2);
}

static inline void __sysreg_restore_common_state(struct kvm_cpu_context *ctxt)
{
	write_sysreg(*ctxt_mdscr_el1(ctxt),  mdscr_el1);

	// POR_EL0 can affect uaccess, so must be saved/restored early.
	if (ctxt_has_s1poe(ctxt))
		write_sysreg_s(ctxt_sys_reg(ctxt, POR_EL0),	SYS_POR_EL0);
}
