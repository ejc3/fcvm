/*
 * Verbatim line ranges from Linux v6.18.50 arch/arm64/include/asm/kvm_nested.h:
 * https://raw.githubusercontent.com/gregkh/linux/v6.18.50/arch/arm64/include/asm/kvm_nested.h
 * Complete source SHA256: ddd388bd8a4e981ea207c7c85e107a983f553a3336840e311238980cd37fe9d8
 * Lines 60-140, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64/nv2-targeted-shadow-s2-unmap.patch to these files,
 * then compiles the functions it names from them into a C harness.
 */
static inline u64 translate_ttbr0_el2_to_ttbr0_el1(u64 ttbr0)
{
	/* Clear the ASID field */
	return ttbr0 & ~GENMASK_ULL(63, 48);
}

extern bool forward_smc_trap(struct kvm_vcpu *vcpu);
extern bool forward_debug_exception(struct kvm_vcpu *vcpu);
extern void kvm_init_nested(struct kvm *kvm);
extern int kvm_vcpu_init_nested(struct kvm_vcpu *vcpu);
extern void kvm_init_nested_s2_mmu(struct kvm_s2_mmu *mmu);
extern struct kvm_s2_mmu *lookup_s2_mmu(struct kvm_vcpu *vcpu);

union tlbi_info;

extern void kvm_s2_mmu_iterate_by_vmid(struct kvm *kvm, u16 vmid,
				       const union tlbi_info *info,
				       void (*)(struct kvm_s2_mmu *,
						const union tlbi_info *));
extern void kvm_vcpu_load_hw_mmu(struct kvm_vcpu *vcpu);
extern void kvm_vcpu_put_hw_mmu(struct kvm_vcpu *vcpu);

extern void check_nested_vcpu_requests(struct kvm_vcpu *vcpu);
extern void kvm_nested_flush_hwstate(struct kvm_vcpu *vcpu);
extern void kvm_nested_sync_hwstate(struct kvm_vcpu *vcpu);

extern void kvm_nested_setup_mdcr_el2(struct kvm_vcpu *vcpu);

struct kvm_s2_trans {
	phys_addr_t output;
	unsigned long block_size;
	bool writable;
	bool readable;
	int level;
	u32 esr;
	u64 desc;
};

static inline phys_addr_t kvm_s2_trans_output(struct kvm_s2_trans *trans)
{
	return trans->output;
}

static inline unsigned long kvm_s2_trans_size(struct kvm_s2_trans *trans)
{
	return trans->block_size;
}

static inline u32 kvm_s2_trans_esr(struct kvm_s2_trans *trans)
{
	return trans->esr;
}

static inline bool kvm_s2_trans_readable(struct kvm_s2_trans *trans)
{
	return trans->readable;
}

static inline bool kvm_s2_trans_writable(struct kvm_s2_trans *trans)
{
	return trans->writable;
}

static inline bool kvm_s2_trans_executable(struct kvm_s2_trans *trans)
{
	return !(trans->desc & BIT(54));
}

extern int kvm_walk_nested_s2(struct kvm_vcpu *vcpu, phys_addr_t gipa,
			      struct kvm_s2_trans *result);
extern int kvm_s2_handle_perm_fault(struct kvm_vcpu *vcpu,
				    struct kvm_s2_trans *trans);
extern int kvm_inject_s2_fault(struct kvm_vcpu *vcpu, u64 esr_el2);
extern void kvm_nested_s2_wp(struct kvm *kvm);
extern void kvm_nested_s2_unmap(struct kvm *kvm, bool may_block);
extern void kvm_nested_s2_flush(struct kvm *kvm);

unsigned long compute_tlb_inval_range(struct kvm_s2_mmu *mmu, u64 val);

static inline bool kvm_supported_tlbi_s1e1_op(struct kvm_vcpu *vpcu, u32 instr)
{
