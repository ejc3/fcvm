/*
 * Verbatim line ranges from Linux v6.18.60 arch/arm64/kvm/nested.c:
 * https://raw.githubusercontent.com/gregkh/linux/v6.18.60/arch/arm64/kvm/nested.c
 * Complete source SHA256: 725634c89893a145c50c1aa1fee97e76d53eed9f65c428041b5ca5ddb5ddb93f
 * Lines 1-134, 733-755, 1132-1204, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64/nv2-targeted-shadow-s2-unmap.patch to these files,
 * then compiles the functions it names from them into a C harness.
 */
// SPDX-License-Identifier: GPL-2.0-only
/*
 * Copyright (C) 2017 - Columbia University and Linaro Ltd.
 * Author: Jintack Lim <jintack.lim@linaro.org>
 */

#include <linux/bitfield.h>
#include <linux/kvm.h>
#include <linux/kvm_host.h>

#include <asm/fixmap.h>
#include <asm/kvm_arm.h>
#include <asm/kvm_emulate.h>
#include <asm/kvm_mmu.h>
#include <asm/kvm_nested.h>
#include <asm/sysreg.h>

#include "sys_regs.h"

struct vncr_tlb {
	/* The guest's VNCR_EL2 */
	u64			gva;
	struct s1_walk_info	wi;
	struct s1_walk_result	wr;

	u64			hpa;
	bool			hpa_writable;

	/* -1 when not mapped on a CPU */
	atomic_t		cpu;

	/*
	 * true if the TLB is valid. Can only be changed with the
	 * mmu_lock held.
	 */
	bool			valid;
};

/*
 * Ratio of live shadow S2 MMU per vcpu. This is a trade-off between
 * memory usage and potential number of different sets of S2 PTs in
 * the guests. Running out of S2 MMUs only affects performance (we
 * will invalidate them more often).
 */
#define S2_MMU_PER_VCPU		2

int kvm_init_nested(struct kvm *kvm)
{
	kvm->arch.nested_mmus = kvmalloc_objs(struct kvm_s2_mmu *,
					      KVM_MAX_VCPUS * S2_MMU_PER_VCPU,
					      GFP_KERNEL_ACCOUNT);
	kvm->arch.nested_mmus_size = 0;

	return kvm->arch.nested_mmus ? 0 : -ENOMEM;
}

void kvm_destroy_nested(struct kvm *kvm)
{
	for (int i = 0; i < kvm->arch.nested_mmus_size; i+= S2_MMU_PER_VCPU)
		kvfree(kvm->arch.nested_mmus[i]);

	kvm->arch.nested_mmus_size = 0;
	kvfree(kvm->arch.nested_mmus);
}

static int init_nested_s2_mmu(struct kvm *kvm, struct kvm_s2_mmu *mmu)
{
	/*
	 * We only initialise the IPA range on the canonical MMU, which
	 * defines the contract between KVM and userspace on where the
	 * "hardware" is in the IPA space. This affects the validity of MMIO
	 * exits forwarded to userspace, for example.
	 *
	 * For nested S2s, we use the PARange as exposed to the guest, as it
	 * is allowed to use it at will to expose whatever memory map it
	 * wants to its own guests as it would be on real HW.
	 */
	return kvm_init_stage2_mmu(kvm, mmu, kvm_get_pa_bits(kvm));
}

int kvm_vcpu_init_nested(struct kvm_vcpu *vcpu)
{
	struct kvm *kvm = vcpu->kvm;
	int num_mmus;

	lockdep_assert_held(&kvm->arch.config_lock);

	if (test_bit(KVM_ARM_VCPU_HAS_EL2_E2H0, kvm->arch.vcpu_features) &&
	    !cpus_have_final_cap(ARM64_HAS_HCR_NV1))
		return -EINVAL;

	if (!vcpu->arch.ctxt.vncr_array)
		vcpu->arch.ctxt.vncr_array = (u64 *)__get_free_page(GFP_KERNEL_ACCOUNT |
								    __GFP_ZERO);

	if (!vcpu->arch.ctxt.vncr_array)
		return -ENOMEM;

	num_mmus = atomic_read(&kvm->online_vcpus) * S2_MMU_PER_VCPU;

	if (num_mmus > kvm->arch.nested_mmus_size) {
		struct kvm_s2_mmu *tmp;
		int i, ret = 0;

		tmp = kvzalloc_objs(*tmp, S2_MMU_PER_VCPU, GFP_KERNEL_ACCOUNT);
		if (!tmp)
			ret = -ENOMEM;

		for (i = 0; !ret && i < S2_MMU_PER_VCPU; i++) {
			ret = init_nested_s2_mmu(kvm, &tmp[i]);
			if (ret)
				break;
		}

		if (ret) {
			while (--i >= 0)
				kvm_free_stage2_pgd(&tmp[i]);

			kvfree(tmp);
			free_page((unsigned long)vcpu->arch.ctxt.vncr_array);
			vcpu->arch.ctxt.vncr_array = NULL;
			return ret;
		}

		guard(write_lock)(&kvm->mmu_lock);

		for (i = 0; i < S2_MMU_PER_VCPU; i++)
			kvm->arch.nested_mmus[i + kvm->arch.nested_mmus_size] = &tmp[i];

		kvm->arch.nested_mmus_size += S2_MMU_PER_VCPU;
	}

	return 0;
}

out:
	atomic_inc(&s2_mmu->refcnt);

	/*
	 * Set the vCPU request to perform an unmap, even if the pending unmap
	 * originates from another vCPU. This guarantees that the MMU has been
	 * completely unmapped before any vCPU actually uses it, and allows
	 * multiple vCPUs to lend a hand with completing the unmap.
	 */
	if (s2_mmu->pending_unmap)
		kvm_make_request(KVM_REQ_NESTED_S2_UNMAP, vcpu);

	return s2_mmu;
}

void kvm_init_nested_s2_mmu(struct kvm_s2_mmu *mmu)
{
	/* CnP being set denotes an invalid entry */
	mmu->tlb_vttbr = VTTBR_CNP_BIT;
	mmu->nested_stage2_enabled = false;
	atomic_set(&mmu->refcnt, 0);
}


	invalidate_vncr_va(vcpu->kvm, &scope);
}

static void kvm_invalidate_vncr_ipa_all(struct kvm *kvm)
{
	struct kvm_pgtable *pgt = kvm->arch.mmu.pgt;

	lockdep_assert_held_write(&kvm->mmu_lock);

	/* if the mmu lock was dropped, pgt teardown may have raced. */
	if (pgt)
		kvm_invalidate_vncr_ipa(kvm, 0, BIT(pgt->ia_bits));
}

void kvm_nested_s2_wp(struct kvm *kvm)
{
	int i;

	lockdep_assert_held_write(&kvm->mmu_lock);

	for (i = 0; i < kvm->arch.nested_mmus_size; i++) {
		struct kvm_s2_mmu *mmu = kvm->arch.nested_mmus[i];

		if (kvm_s2_mmu_valid(mmu))
			kvm_stage2_wp_range(mmu, 0, kvm_phys_size(mmu));
	}

	kvm_invalidate_vncr_ipa_all(kvm);
}

void kvm_nested_s2_unmap(struct kvm *kvm, bool may_block)
{
	int i;

	lockdep_assert_held_write(&kvm->mmu_lock);

	for (i = 0; i < kvm->arch.nested_mmus_size; i++) {
		struct kvm_s2_mmu *mmu = kvm->arch.nested_mmus[i];

		if (kvm_s2_mmu_valid(mmu))
			kvm_stage2_unmap_range(mmu, 0, kvm_phys_size(mmu), may_block);
	}

	kvm_invalidate_vncr_ipa_all(kvm);
}

void kvm_nested_s2_flush(struct kvm *kvm)
{
	int i;

	lockdep_assert_held_write(&kvm->mmu_lock);

	for (i = 0; i < kvm->arch.nested_mmus_size; i++) {
		struct kvm_s2_mmu *mmu = kvm->arch.nested_mmus[i];

		if (kvm_s2_mmu_valid(mmu))
			kvm_stage2_flush_range(mmu, 0, kvm_phys_size(mmu));
	}
}

void kvm_arch_flush_shadow_all(struct kvm *kvm)
{
	for (int i = 0; i < kvm->arch.nested_mmus_size; i++) {
		struct kvm_s2_mmu *mmu = kvm->arch.nested_mmus[i];

		if (!WARN_ON(atomic_read(&mmu->refcnt)))
			kvm_free_stage2_pgd(mmu);
	}
	kvm_uninit_stage2_mmu(kvm);
}

/*
 * Dealing with VNCR_EL2 exposed by the *guest* is a complicated matter:
