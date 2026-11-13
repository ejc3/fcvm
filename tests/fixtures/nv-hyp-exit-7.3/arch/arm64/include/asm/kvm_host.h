/*
 * Verbatim line ranges from Linux v7.3.3 arch/arm64/include/asm/kvm_host.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/include/asm/kvm_host.h
 * Complete source SHA256: ac044465ffccb1446e843e0edfb22626ce8c0c73f34339be978e1163217f65bc
 * Lines 650-672, 1165-1200, 1520-1534, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/nv-guest-hypervisor-exits-without-calls.patch to this file,
 * then checks or compiles the functions it names from it.
 */
struct resx {
	u64	res0;
	u64	res1;
};

struct kvm_sysreg_masks {
	struct resx mask[NR_SYS_REGS - __SANITISED_REG_START__];
};

static inline struct resx __kvm_get_sysreg_resx(struct kvm_arch *arch,
						enum vcpu_sysreg sr)
{
	struct kvm_sysreg_masks *masks;

	masks = arch->sysreg_masks;
	if (likely(masks &&
		   sr >= __SANITISED_REG_START__ && sr < NR_SYS_REGS))
		return masks->mask[sr - __SANITISED_REG_START__];

	return (struct resx){};
}

#define kvm_get_sysreg_resx(k, sr) __kvm_get_sysreg_resx(&(k)->arch, (sr))

 * memory backed version of a register, and not the one most recently
 * accessed by a running VCPU.  For example, for userspace access or
 * for system registers that are never context switched, but only
 * emulated.
 *
 * Don't bother with VNCR-based accesses in the nVHE code, it has no
 * business dealing with NV.
 */
static inline u64 *___ctxt_sys_reg(const struct kvm_cpu_context *ctxt, int r)
{
#if !defined (__KVM_NVHE_HYPERVISOR__)
	if (unlikely(cpus_have_final_cap(ARM64_HAS_NESTED_VIRT) &&
		     r >= __VNCR_START__ && ctxt->vncr_array))
		return &ctxt->vncr_array[r - __VNCR_START__];
#endif
	return (u64 *)&ctxt->sys_regs[r];
}

#define __ctxt_sys_reg(c,r)						\
	({								\
		BUILD_BUG_ON(__builtin_constant_p(r) &&			\
			     (r) >= NR_SYS_REGS);			\
		___ctxt_sys_reg(c, r);					\
	})

#define ctxt_sys_reg(c,r)	(*__ctxt_sys_reg(c,r))

u64 kvm_vcpu_apply_reg_masks(const struct kvm_vcpu *, enum vcpu_sysreg, u64);

#define __vcpu_assign_sys_reg(v, r, val)				\
	do {								\
		const struct kvm_cpu_context *ctxt = &(v)->arch.ctxt;	\
		u64 __v = (val);					\
		if (vcpu_has_nv((v)) && (r) >= __SANITISED_REG_START__)	\
			__v = kvm_vcpu_apply_reg_masks((v), (r), __v);	\
									\

#define kvm_supports_32bit_el0()				\
	(system_supports_32bit_el0() &&				\
	 !static_branch_unlikely(&arm64_mismatched_32bit_el0))

#define kvm_vm_has_ran_once(kvm)					\
	(test_bit(KVM_ARCH_FLAG_HAS_RAN_ONCE, &(kvm)->arch.flags))

static inline bool __vcpu_has_feature(const struct kvm_arch *ka, int feature)
{
	return test_bit(feature, ka->vcpu_features);
}

#define kvm_vcpu_has_feature(k, f)	__vcpu_has_feature(&(k)->arch, (f))
#define vcpu_has_feature(v, f)	__vcpu_has_feature(&(v)->kvm->arch, (f))

