/* SPDX-License-Identifier: GPL-2.0-only */
/* Single-CPU state regression. Page walks and allocation are stubbed; the
 * production translation/reset/put functions execute from patched nested.c.
 * This does not emulate hardware TLBs or prove cross-CPU invalidation. */
#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

typedef uint64_t u64;
struct s1_walk_info { int regime; bool as_el0, pan; };
struct s1_walk_result { u64 pa; bool pw; };
struct vncr_tlb {
    u64 gva;
    struct s1_walk_info wi;
    struct s1_walk_result wr;
    u64 hpa;
    bool hpa_writable;
    int cpu;
    bool valid;
};
struct kvm_s2_mmu { int refcnt; };
struct kvm {
    struct { int vncr_map_count; struct kvm_s2_mmu mmu; } arch;
    int mmu_lock, srcu;
    unsigned long mmu_invalidate_seq;
};
struct kvm_vcpu {
    struct { struct vncr_tlb *vncr_tlb; struct kvm_s2_mmu *hw_mmu; } arch;
    struct kvm *kvm;
    bool scheduled_out;
};
struct kvm_memory_slot { int flags; };
struct page { int unused; };
static struct kvm_memory_slot slot;
static bool mapped, pte_present;
static int clears, translation_result;

#define scoped_guard(kind, lock) for (int once = 1; once; once = 0)
#define guard(kind) (void)
#define BUG_ON(condition) assert(!(condition))
#define host_data_test_flag(flag) mapped
#define host_data_clear_flag(flag) (mapped = false)
#define smp_processor_id() 3
#define is_hyp_ctxt(vcpu) false
#define vncr_fixmap(cpu) (cpu)
#define atomic_dec(value) (--*(value))
#define vcpu_get_flag(vcpu, flag) false
#define kvm_is_nested_s2_mmu(kvm, mmu) false
#define get_s2_mmu_nested(vcpu) (&(vcpu)->kvm->arch.mmu)
#define __vcpu_sys_reg(vcpu, reg) 0
#define HCR_NV 1
#define kvm_make_request(request, vcpu) ((void)0)
#define TR_EL20 0
#define read_vncr_el2(vcpu) 0x1000ULL
#define kvm_is_write_fault(vcpu) false
#define smp_rmb() ((void)0)
#define PAGE_SHIFT 12
#define PAGE_SIZE (1UL << PAGE_SHIFT)
#define gfn_to_memslot(kvm, gfn) (&slot)
#define fail_s1_walk(result, fault, level) ((void)0)
#define kvm_slot_has_gmem(slot) false
#define FOLL_WRITE 1
#define __kvm_faultin_pfn(slot, gfn, flags, writable, page) \
    (*(writable) = true, *(page) = NULL, 1ULL)
#define is_error_noslot_pfn(pfn) false
#define kvm_gmem_get_pfn(kvm, slot, gfn, pfn, page, order) \
    (*(pfn) = 1, *(page) = NULL, 0)
#define kvm_prepare_memory_fault_exit(...) ((void)0)
#define KVM_MEM_READONLY 1
#define pfn_is_map_memory(pfn) true
#define kvm_release_faultin_page(...) ((void)0)
#define mmu_invalidate_retry(kvm, seq) false
#define mark_page_dirty(kvm, gfn) ((void)0)

static void clear_fixmap(int cpu)
{
    assert(cpu == smp_processor_id());
    pte_present = false;
    clears++;
}

static int __kvm_translate_va(struct kvm_vcpu *vcpu,
                              struct s1_walk_info *wi,
                              struct s1_walk_result *wr, u64 va)
{
    wr->pa = 0x1000;
    wr->pw = true;
    return translation_result;
}

#include "arch/arm64/kvm/nested.c"

int main(int argc, char **argv)
{
    assert(argc == 2);
    int scenario = atoi(argv[1]);
    assert(scenario >= 0 && scenario <= 2);
    bool initially_mapped = scenario != 0;
    mapped = pte_present = initially_mapped;
    translation_result = scenario == 2 ? -EFAULT : 0;
    struct vncr_tlb vt = { .cpu = initially_mapped ? 3 : -1, .valid = true };
    /* One other CPU's mapping must not be decremented by this vCPU's reset. */
    struct kvm kvm = { .arch.vncr_map_count = 1 + initially_mapped };
    struct kvm_vcpu vcpu = { .arch.vncr_tlb = &vt, .kvm = &kvm };
    bool is_gmem = false;

    assert(kvm_translate_vncr(&vcpu, &is_gmem) == translation_result);
    fprintf(stderr, "scenario=%d cpu=%d mapped=%d count=%d pte=%d clears=%d valid=%d\n",
            scenario, vt.cpu, mapped, kvm.arch.vncr_map_count, pte_present, clears, vt.valid);
    assert(!mapped && vt.cpu == -1 && kvm.arch.vncr_map_count == 1 && !pte_present);
    assert(clears == initially_mapped);
    assert(vt.valid == (translation_result == 0));

    /* A successful retranslation used to leave mapped=true and cpu=-1 here,
     * causing the actual put function's CPU ownership BUG_ON to fire. */
    kvm_vcpu_put_hw_mmu(&vcpu);
    assert(!mapped && vt.cpu == -1 && kvm.arch.vncr_map_count == 1 && !pte_present);
    assert(clears == initially_mapped);
    return 0;
}
