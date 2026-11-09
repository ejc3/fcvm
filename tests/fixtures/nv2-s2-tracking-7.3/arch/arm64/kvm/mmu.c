/*
 * Verbatim line ranges from Linux v7.3.3 arch/arm64/kvm/mmu.c:
 * https://raw.githubusercontent.com/gregkh/linux/v7.3.3/arch/arm64/kvm/mmu.c
 * Complete source SHA256: 26b498fd11ef50a6837cc95d5a395b2c516e8ff088911f62c23df84eb2488cec
 * Lines 7-12, 40-95, 322-356, 975-1062, 1131-1158, 1600-1605, 1613-1618, 1633-1638, 1666-1676, 1997-2011, 2017-2022, 2035-2040, 2057-2078, 2410-2423, 2694-2705, each range separated by one blank line.
 * tests/test_kernel_patch_invariants.rs applies
 * kernel/patches-arm64-host/nv2-targeted-shadow-s2-unmap.patch to these files,
 * then compiles the functions it names from them into a C harness.
 */
#include <linux/acpi.h>
#include <linux/mman.h>
#include <linux/kvm_host.h>
#include <linux/io.h>
#include <linux/hugetlb.h>
#include <linux/sched/signal.h>

static phys_addr_t __stage2_range_addr_end(phys_addr_t addr, phys_addr_t end,
					   phys_addr_t size)
{
	phys_addr_t boundary = ALIGN_DOWN(addr + size, size);

	return (boundary - 1 < end - 1) ? boundary : end;
}

static phys_addr_t stage2_range_addr_end(phys_addr_t addr, phys_addr_t end)
{
	phys_addr_t size = kvm_granule_size(KVM_PGTABLE_MIN_BLOCK_LEVEL);

	return __stage2_range_addr_end(addr, end, size);
}

/*
 * Release kvm_mmu_lock periodically if the memory region is large. Otherwise,
 * we may see kernel panics with CONFIG_DETECT_HUNG_TASK,
 * CONFIG_LOCKUP_DETECTOR, CONFIG_LOCKDEP. Additionally, holding the lock too
 * long will also starve other vCPUs. We have to also make sure that the page
 * tables are not freed while we released the lock.
 */
static int stage2_apply_range(struct kvm_s2_mmu *mmu, phys_addr_t start,
			      phys_addr_t end,
			      int (*fn)(struct kvm_pgtable *, u64, u64),
			      bool resched)
{
	struct kvm *kvm = kvm_s2_mmu_to_kvm(mmu);
	bool lock_dropped = false;
	phys_addr_t addr = start;
	int ret;
	u64 next;

	do {
		struct kvm_pgtable *pgt = mmu->pgt;
		/*
		 * We may be raced on PGT teardown when we release the
		 * kvm->mmu_lock. That's fine as the PGT is legitimately no
		 * longer present.
		 */
		if (!pgt)
			return lock_dropped ? 0 : -EINVAL;

		next = stage2_range_addr_end(addr, end);
		ret = fn(pgt, addr, next - addr);
		if (ret)
			break;

		if (resched && next != end) {
			cond_resched_rwlock_write(&kvm->mmu_lock);
			lock_dropped = true;
		}
	} while (addr = next, addr != end);

	return ret;
}

 * we then fully enforce cacheability of RAM, no matter what the guest
 * does.
 */
/**
 * __unmap_stage2_range -- Clear stage2 page table entries to unmap a range
 * @mmu:   The KVM stage-2 MMU pointer
 * @start: The intermediate physical base address of the range to unmap
 * @size:  The size of the area to unmap
 * @may_block: Whether or not we are permitted to block
 *
 * Clear a range of stage-2 mappings, lowering the various ref-counts.  Must
 * be called while holding mmu_lock (unless for freeing the stage2 pgd before
 * destroying the VM), otherwise another faulting VCPU may come in and mess
 * with things behind our backs.
 */
static void __unmap_stage2_range(struct kvm_s2_mmu *mmu, phys_addr_t start, u64 size,
				 bool may_block)
{
	struct kvm *kvm = kvm_s2_mmu_to_kvm(mmu);
	phys_addr_t end = start + size;

	lockdep_assert_held_write(&kvm->mmu_lock);
	WARN_ON(size & ~PAGE_MASK);
	WARN_ON(stage2_apply_range(mmu, start, end, KVM_PGT_FN(kvm_pgtable_stage2_unmap),
				   may_block));
}

void kvm_stage2_unmap_range(struct kvm_s2_mmu *mmu, phys_addr_t start,
			    u64 size, bool may_block)
{
	if (kvm_vm_is_protected(kvm_s2_mmu_to_kvm(mmu)))
		return;

	__unmap_stage2_range(mmu, start, size, may_block);
}

/**
 * kvm_init_stage2_mmu - Initialise a S2 MMU structure
 * @kvm:	The pointer to the KVM structure
 * @mmu:	The pointer to the s2 MMU structure
 * @type:	The machine type of the virtual machine
 *
 * Allocates only the stage-2 HW PGD level table(s).
 * Note we don't need locking here as this is only called in two cases:
 *
 * - when the VM is created, which can't race against anything
 *
 * - when secondary kvm_s2_mmu structures are initialised for NV
 *   guests, and the caller must hold kvm->lock as this is called on a
 *   per-vcpu basis.
 */
int kvm_init_stage2_mmu(struct kvm *kvm, struct kvm_s2_mmu *mmu, unsigned long type)
{
	int cpu, err;
	struct kvm_pgtable *pgt;

	/*
	 * If we already have our page tables in place, and that the
	 * MMU context is the canonical one, we have a bug somewhere,
	 * as this is only supposed to ever happen once per VM.
	 *
	 * Otherwise, we're building nested page tables, and that's
	 * probably because userspace called KVM_ARM_VCPU_INIT more
	 * than once on the same vcpu. Since that's actually legal,
	 * don't kick a fuss and leave gracefully.
	 */
	if (mmu->pgt != NULL) {
		if (kvm_is_nested_s2_mmu(kvm, mmu))
			return 0;

		kvm_err("kvm_arch already initialized?\n");
		return -EINVAL;
	}

	err = kvm_init_ipa_range(mmu, type);
	if (err)
		return err;

	pgt = kzalloc_obj(*pgt, GFP_KERNEL_ACCOUNT);
	if (!pgt)
		return -ENOMEM;

	mmu->arch = &kvm->arch;
	err = KVM_PGT_FN(kvm_pgtable_stage2_init)(pgt, mmu, &kvm_s2_mm_ops);
	if (err)
		goto out_free_pgtable;

	mmu->pgt = pgt;
	if (is_protected_kvm_enabled())
		return 0;

	mmu->last_vcpu_ran = alloc_percpu(typeof(*mmu->last_vcpu_ran));
	if (!mmu->last_vcpu_ran) {
		err = -ENOMEM;
		goto out_destroy_pgtable;
	}

	for_each_possible_cpu(cpu)
		*per_cpu_ptr(mmu->last_vcpu_ran, cpu) = -1;

	 /* The eager page splitting is disabled by default */
	mmu->split_page_chunk_size = KVM_ARM_EAGER_SPLIT_CHUNK_SIZE_DEFAULT;
	mmu->split_page_cache.gfp_zero = __GFP_ZERO;

	mmu->pgd_phys = __pa(pgt->pgd);

	if (kvm_is_nested_s2_mmu(kvm, mmu))
		kvm_init_nested_s2_mmu(mmu);

	return 0;

out_destroy_pgtable:
	kvm_stage2_destroy(pgt);
	mmu->pgt = NULL;
out_free_pgtable:
	kfree(pgt);
	return err;
}

void kvm_uninit_stage2_mmu(struct kvm *kvm)
{
	kvm_free_stage2_pgd(&kvm->arch.mmu);
	kvm_mmu_free_memory_cache(&kvm->arch.mmu.split_page_cache);
}

	srcu_read_unlock(&kvm->srcu, idx);
}

void kvm_free_stage2_pgd(struct kvm_s2_mmu *mmu)
{
	struct kvm *kvm = kvm_s2_mmu_to_kvm(mmu);
	struct kvm_pgtable *pgt = NULL;

	write_lock(&kvm->mmu_lock);
	pgt = mmu->pgt;
	if (pgt) {
		mmu->pgd_phys = 0;
		mmu->pgt = NULL;
		free_percpu(mmu->last_vcpu_ran);
	}

	if (kvm_is_nested_s2_mmu(kvm, mmu))
		kvm_init_nested_s2_mmu(mmu);

	write_unlock(&kvm->mmu_lock);

	if (pgt) {
		kvm_stage2_destroy(pgt);
		kfree(pgt);
	}
}

static void hyp_mc_free_fn(void *addr, void *mc)

	enum kvm_pgtable_walk_flags flags = KVM_PGTABLE_WALK_SHARED;
	enum kvm_pgtable_prot prot = KVM_PGTABLE_PROT_R;
	struct kvm_pgtable *pgt = s2fd->vcpu->arch.hw_mmu->pgt;
	unsigned long mmu_seq;
	struct page *page;
	struct kvm *kvm = s2fd->vcpu->kvm;

		ret = topup_mmu_memcache(s2fd->vcpu, memcache);
		if (ret)
			return ret;
	}

	if (s2fd->nested)

	if (ret) {
		kvm_prepare_memory_fault_exit(s2fd->vcpu, s2fd->fault_ipa, PAGE_SIZE,
					      write_fault, exec_fault, false);
		return ret;
	}


		ret = KVM_PGT_FN(kvm_pgtable_stage2_map)(pgt, s2fd->fault_ipa, PAGE_SIZE,
							 __pfn_to_phys(pfn), prot,
							 memcache, flags);
	}

out_unlock:
	kvm_release_faultin_page(kvm, page, !!ret, prot & KVM_PGTABLE_PROT_W);
	kvm_fault_unlock(kvm);

	if ((prot & KVM_PGTABLE_PROT_W) && !ret)
		mark_page_dirty_in_slot(kvm, s2fd->memslot, gfn);

			    void *memcache)
{
	enum kvm_pgtable_walk_flags flags = KVM_PGTABLE_WALK_SHARED;
	bool writable = prot & KVM_PGTABLE_PROT_W;
	struct kvm *kvm = s2fd->vcpu->kvm;
	struct kvm_pgtable *pgt;
	long perm_fault_granule;
	long mapping_size;
	kvm_pfn_t pfn;
	gfn_t gfn;
	int ret;

	kvm_fault_lock(kvm);
	pgt = s2fd->vcpu->arch.hw_mmu->pgt;
	ret = -EAGAIN;

	mapping_size = s2vi->vma_pagesize;
	pfn = s2vi->pfn;
	gfn = s2vi->gfn;

	/*
	 * If we are not forced to use page mapping, check if we are

				goto out_unlock;
			}
		}
	}

	if (!perm_fault_granule && !s2vi->map_non_cacheable && kvm_has_mte(kvm))

		ret = KVM_PGT_FN(kvm_pgtable_stage2_map)(pgt, gfn_to_gpa(gfn), mapping_size,
							 __pfn_to_phys(pfn), prot,
							 memcache, flags);
	}

out_unlock:
	kvm_release_faultin_page(kvm, s2vi->page, !!ret, writable);
	kvm_fault_unlock(kvm);

	/*
	 * Mark the page dirty only if the fault is handled successfully,
	 * making sure we adjust the canonical IPA if the mapping size has
	 * been updated (via a THP upgrade, for example).
	 */
	if (writable && !ret) {
		phys_addr_t ipa = gfn_to_gpa(get_canonical_gfn(s2fd, s2vi));
		ipa &= ~(mapping_size - 1);
		mark_page_dirty_in_slot(kvm, s2fd->memslot, gpa_to_gfn(ipa));
	}

	if (ret != -EAGAIN)
		return ret;


bool kvm_unmap_gfn_range(struct kvm *kvm, struct kvm_gfn_range *range)
{
	if (!kvm->arch.mmu.pgt || kvm_vm_is_protected(kvm))
		return false;

	__unmap_stage2_range(&kvm->arch.mmu, range->start << PAGE_SHIFT,
			     (range->end - range->start) << PAGE_SHIFT,
			     range->may_block);

	kvm_nested_s2_unmap(kvm, range->may_block);
	return false;
}


void kvm_arch_flush_shadow_memslot(struct kvm *kvm,
				   struct kvm_memory_slot *slot)
{
	gpa_t gpa = slot->base_gfn << PAGE_SHIFT;
	phys_addr_t size = slot->npages << PAGE_SHIFT;

	write_lock(&kvm->mmu_lock);
	kvm_stage2_unmap_range(&kvm->arch.mmu, gpa, size, true);
	kvm_nested_s2_unmap(kvm, true);
	write_unlock(&kvm->mmu_lock);
}

