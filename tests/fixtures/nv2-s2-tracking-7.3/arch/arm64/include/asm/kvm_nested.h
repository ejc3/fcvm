/*
 * Verbatim line ranges from Linux v7.3.3 arch/arm64/include/asm/kvm_nested.h:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/include/asm/kvm_nested.h
 * Complete source SHA256: c15ab343081d711ecd1a3399ecd307553542c06f2ab18bb2ca4c46f9b74fc62e
 * Lines 78-83, 166-171, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/nv2-targeted-shadow-s2-unmap.patch to these files,
 * then compiles the functions it names from them into a C harness.
 */
				       const union tlbi_info *info,
				       void (*)(struct kvm_s2_mmu *,
						const union tlbi_info *));
extern void kvm_vcpu_load_hw_mmu(struct kvm_vcpu *vcpu);
extern void kvm_vcpu_put_hw_mmu(struct kvm_vcpu *vcpu);


				    struct kvm_s2_trans *trans);
extern int kvm_inject_s2_fault(struct kvm_vcpu *vcpu, u64 esr_el2);
extern void kvm_nested_s2_wp(struct kvm *kvm);
extern void kvm_nested_s2_unmap(struct kvm *kvm, bool may_block);
extern void kvm_nested_s2_flush(struct kvm *kvm);

