/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Runs the 6.18 shadow stage-2 unmap paths with
 * kernel/patches-arm64/nv2-targeted-shadow-s2-unmap.patch applied, or without
 * it. The test compiles the kernel functions it names from the patched
 * excerpts into extracted.c, which this file includes after the stubs below.
 * Page tables are stubbed: kvm_pgtable_stage2_unmap() only logs the range it
 * was asked to unmap. The interval tree is a list with the kernel's API and
 * overlap semantics. It does not model hardware TLBs or cross-CPU ordering;
 * the one interleaving it runs (teardown_race) is forced with semaphores.
 */
#include <assert.h>
#include <errno.h>
#include <limits.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef uint64_t u64;
typedef uint32_t u32;
typedef uint16_t u16;
typedef u64 phys_addr_t;
typedef u64 gpa_t;
typedef u64 gfn_t;
typedef int atomic_t;

#define PAGE_SHIFT 12
#define PAGE_SIZE (1UL << PAGE_SHIFT)
#define PAGE_MASK (~(PAGE_SIZE - 1))
#define SZ_4K 0x1000UL
#define SZ_2M 0x200000UL
#define SZ_1G 0x40000000UL
#define IA_BITS 40
#define KVM_MAX_VCPUS 512
#define VTTBR_CNP_BIT 1UL
#define GFP_KERNEL_ACCOUNT 0
#define __GFP_ZERO 0
#define KVM_ARM_EAGER_SPLIT_CHUNK_SIZE_DEFAULT 0
#define KVM_PGTABLE_MIN_BLOCK_LEVEL 1

#define BIT(n) (1UL << (n))
#define GENMASK_ULL(h, l) (((~0ULL) << (l)) & (~0ULL >> (63 - (h))))
#define ALIGN_DOWN(x, a) ((x) & ~((__typeof__(x))(a) - 1))
#define BUILD_BUG_ON(cond) _Static_assert(!(cond), #cond)
#define container_of(ptr, type, member) ((type *)((char *)(ptr) - offsetof(type, member)))
#define swap(a, b) do { __typeof__(a) __t = (a); (a) = (b); (b) = __t; } while (0)
#define size_mul(a, b) ((a) * (b))
#define unlikely(x) (x)
#define likely(x) (x)

static int warnings;
#define WARN_ON(cond) ({ bool __c = !!(cond); if (__c) { warnings++; fprintf(stderr, "WARN_ON(%s) at %s:%d\n", #cond, __func__, __LINE__); } __c; })
#define WARN_ON_ONCE(cond) WARN_ON(cond)
#define kvm_err(...) fprintf(stderr, __VA_ARGS__)

#define atomic_read(v) (*(v))
#define atomic_set(v, i) (*(v) = (i))
#define test_bit(nr, addr) false
#define cpus_have_final_cap(cap) true
#define is_protected_kvm_enabled() false
#define KVM_PGT_FN(fn) fn
#define kvm_granule_size(level) SZ_1G
#define kvm_get_pa_bits(kvm) IA_BITS
#define kvm_phys_size(mmu) (1ULL << IA_BITS)
#define __pa(x) ((phys_addr_t)(uintptr_t)(x))
#define for_each_possible_cpu(cpu) for ((cpu) = 0; (cpu) < 1; (cpu)++)
#define per_cpu_ptr(ptr, cpu) (&(ptr)[cpu])
#define cond_resched() ((void)0)

/* Allocations: every allocator here counts, so a scenario can require that a
 * full teardown frees everything it allocated. */
static long live_allocations;

static void *counted_alloc(size_t size)
{
	void *p = calloc(1, size ? size : 1);

	assert(p);
	__atomic_add_fetch(&live_allocations, 1, __ATOMIC_SEQ_CST);
	return p;
}

static void counted_free(const void *p)
{
	if (!p)
		return;
	__atomic_sub_fetch(&live_allocations, 1, __ATOMIC_SEQ_CST);
	free((void *)p);
}

#define kmalloc(size, gfp) counted_alloc(size)
#define kzalloc(size, gfp) counted_alloc(size)
#define kvcalloc(n, size, gfp) counted_alloc((n) * (size))
#define kfree(p) counted_free(p)
#define alloc_percpu(type) ((type *)counted_alloc(sizeof(type)))
#define free_percpu(p) counted_free(p)
#define __get_free_page(gfp) ((unsigned long)counted_alloc(PAGE_SIZE))
#define free_page(addr) counted_free((void *)(addr))

/* Interval tree with the kernel's API: [start, last] closed ranges, iteration
 * over every node that overlaps the query. */
struct interval_tree_node {
	unsigned long start, last, __subtree_last;
	struct interval_tree_node *harness_next;
};
struct rb_root_cached {
	struct interval_tree_node *harness_first;
};
#define RB_ROOT_CACHED ((struct rb_root_cached){ NULL })

static bool overlaps(struct interval_tree_node *n, unsigned long start, unsigned long last)
{
	return n->start <= last && n->last >= start;
}

static void interval_tree_insert(struct interval_tree_node *node, struct rb_root_cached *root)
{
	node->harness_next = root->harness_first;
	root->harness_first = node;
}

static void interval_tree_remove(struct interval_tree_node *node, struct rb_root_cached *root)
{
	struct interval_tree_node **pp = &root->harness_first;

	while (*pp && *pp != node)
		pp = &(*pp)->harness_next;
	if (!*pp) {
		fprintf(stderr, "interval_tree_remove: node [%#lx, %#lx] is not in this tree\n",
			node->start, node->last);
		abort();
	}
	*pp = node->harness_next;
}

static struct interval_tree_node *interval_tree_iter_first(struct rb_root_cached *root,
							   unsigned long start, unsigned long last)
{
	for (struct interval_tree_node *n = root->harness_first; n; n = n->harness_next)
		if (overlaps(n, start, last))
			return n;
	return NULL;
}

static struct interval_tree_node *interval_tree_iter_next(struct interval_tree_node *node,
							  unsigned long start, unsigned long last)
{
	for (struct interval_tree_node *n = node->harness_next; n; n = n->harness_next)
		if (overlaps(n, start, last))
			return n;
	return NULL;
}

static int tree_count(struct rb_root_cached *root)
{
	int n = 0;

	for (struct interval_tree_node *p = root->harness_first; p; p = p->harness_next)
		n++;
	return n;
}

/* Locks: a real reader/writer lock, so the teardown race runs two threads. */
typedef struct {
	pthread_mutex_t m;
	pthread_cond_t c;
	int writers, readers;
} rwlock_t;
typedef struct { int held; } spinlock_t;

static void rwlock_init(rwlock_t *l)
{
	pthread_mutex_init(&l->m, NULL);
	pthread_cond_init(&l->c, NULL);
	l->writers = l->readers = 0;
}

static void write_lock(rwlock_t *l)
{
	pthread_mutex_lock(&l->m);
	while (l->writers || l->readers)
		pthread_cond_wait(&l->c, &l->m);
	l->writers = 1;
	pthread_mutex_unlock(&l->m);
}

static void write_unlock(rwlock_t *l)
{
	pthread_mutex_lock(&l->m);
	assert(l->writers == 1);
	l->writers = 0;
	pthread_cond_broadcast(&l->c);
	pthread_mutex_unlock(&l->m);
}

static void read_lock(rwlock_t *l)
{
	pthread_mutex_lock(&l->m);
	while (l->writers)
		pthread_cond_wait(&l->c, &l->m);
	l->readers++;
	pthread_mutex_unlock(&l->m);
}

static void read_unlock(rwlock_t *l)
{
	pthread_mutex_lock(&l->m);
	assert(l->readers > 0);
	l->readers--;
	pthread_cond_broadcast(&l->c);
	pthread_mutex_unlock(&l->m);
}

#define lockdep_assert_held_write(l) assert((l)->writers == 1)
#define lockdep_assert_held_read(l) assert((l)->readers > 0)
#define spin_lock_init(l) ((l)->held = 0)

struct harness_guard_spinlock { spinlock_t *l; };
static struct harness_guard_spinlock harness_lock_spinlock(spinlock_t *l)
{
	assert(!l->held);
	l->held = 1;
	return (struct harness_guard_spinlock){ l };
}
static void harness_unlock_spinlock(struct harness_guard_spinlock *g)
{
	g->l->held = 0;
}
#define HARNESS_CAT2(a, b) a##b
#define HARNESS_CAT(a, b) HARNESS_CAT2(a, b)
#define guard(kind) \
	struct harness_guard_##kind __attribute__((cleanup(harness_unlock_##kind))) \
	HARNESS_CAT(harness_guard_, __COUNTER__) = harness_lock_##kind

/* The 6.18 structures, cut down to the fields the compiled functions use. */
struct kvm_mmu_memory_cache { int gfp_zero; };
struct kvm_s2_mmu;
struct kvm_pgtable {
	u32 ia_bits;
	struct kvm_s2_mmu *mmu;
	void *pgd;
};
struct kvm_arch;
struct kvm_s2_mmu {
	phys_addr_t pgd_phys;
	struct kvm_pgtable *pgt;
	int *last_vcpu_ran;
	struct kvm_mmu_memory_cache split_page_cache;
	u64 split_page_chunk_size;
	struct kvm_arch *arch;
	u64 tlb_vttbr;
	bool nested_stage2_enabled;
	bool pending_unmap;
	struct rb_root_cached guest_s2_mappings;
	atomic_t refcnt;
};
struct kvm_arch {
	struct kvm_s2_mmu mmu;
	unsigned long vcpu_features[1];
	struct kvm_s2_mmu *nested_mmus;
	size_t nested_mmus_size;
	int nested_mmus_next;
	spinlock_t guest_s2_tracking_lock;
	atomic_t vncr_map_count;
};
struct kvm {
	struct kvm_arch arch;
	rwlock_t mmu_lock;
	atomic_t online_vcpus;
};
struct kvm_vcpu {
	struct kvm *kvm;
	struct { struct { u64 *vncr_array; } ctxt; } arch;
};
struct kvm_gfn_range {
	gfn_t start, end;
	bool may_block;
};
struct kvm_memory_slot {
	gfn_t base_gfn;
	unsigned long npages;
};

#define kvm_s2_mmu_to_kvm(mmu) container_of((mmu)->arch, struct kvm, arch)

static inline bool kvm_is_nested_s2_mmu(struct kvm *kvm, struct kvm_s2_mmu *mmu)
{
	return &kvm->arch.mmu != mmu;
}

static inline bool kvm_s2_mmu_valid(struct kvm_s2_mmu *mmu)
{
	return !(mmu->tlb_vttbr & VTTBR_CNP_BIT);
}

static void kvm_mmu_free_memory_cache(struct kvm_mmu_memory_cache *mc) { (void)mc; }
static int kvm_init_ipa_range(struct kvm_s2_mmu *mmu, unsigned long type) { return 0; }
static int kvm_s2_mm_ops;

static int kvm_pgtable_stage2_init(struct kvm_pgtable *pgt, struct kvm_s2_mmu *mmu, void *ops)
{
	pgt->ia_bits = IA_BITS;
	pgt->mmu = mmu;
	pgt->pgd = pgt;
	return 0;
}

static void kvm_pgtable_stage2_destroy(struct kvm_pgtable *pgt) { (void)pgt; }

/* Every unmap a scenario asked a page table for. */
struct unmap_call {
	struct kvm_pgtable *pgt;
	u64 addr, size;
};
static struct unmap_call unmaps[4096];
static int nr_unmaps;

static int kvm_pgtable_stage2_unmap(struct kvm_pgtable *pgt, u64 addr, u64 size)
{
	assert(nr_unmaps < (int)(sizeof(unmaps) / sizeof(unmaps[0])));
	unmaps[nr_unmaps++] = (struct unmap_call){ pgt, addr, size };
	return 0;
}

static void kvm_invalidate_vncr_ipa(struct kvm *kvm, u64 start, u64 end)
{
	(void)kvm; (void)start; (void)end;
}

/* The forced interleaving for teardown_race: the unmapping thread drops the
 * mmu_lock to reschedule, kvm_arch_flush_shadow_all() runs on a second thread
 * until it has freed nested_mmus, and the unmapping thread resumes there. */
static int race_state; /* 0 off, 1 armed, 2 waiting for the array free */
static void *race_array;
static sem_t go_flush, array_freed, unmapper_done;

static int cond_resched_rwlock_write(rwlock_t *l)
{
	write_unlock(l);
	if (race_state == 1) {
		race_state = 2;
		sem_post(&go_flush);
		sem_wait(&array_freed);
	}
	write_lock(l);
	return 1;
}

static void kvfree(const void *p)
{
	counted_free(p);
	if (race_state == 2 && p == race_array) {
		race_state = 3;
		sem_post(&array_freed);
		sem_wait(&unmapper_done);
	}
}

/* The functions under test, declared here because extracted.c holds them in
 * source order and some call ones defined later or in the other file. */
struct kvm_guest_s2_mapping;
void kvm_init_nested(struct kvm *kvm);
int kvm_vcpu_init_nested(struct kvm_vcpu *vcpu);
void kvm_init_nested_s2_mmu(struct kvm_s2_mmu *mmu);
int kvm_init_stage2_mmu(struct kvm *kvm, struct kvm_s2_mmu *mmu, unsigned long type);
void kvm_uninit_stage2_mmu(struct kvm *kvm);
void kvm_free_stage2_pgd(struct kvm_s2_mmu *mmu);
void kvm_stage2_unmap_range(struct kvm_s2_mmu *mmu, phys_addr_t start, u64 size, bool may_block);
bool kvm_unmap_gfn_range(struct kvm *kvm, struct kvm_gfn_range *range);
void kvm_arch_flush_shadow_memslot(struct kvm *kvm, struct kvm_memory_slot *slot);
void kvm_nested_s2_unmap(struct kvm *kvm, bool may_block);
void kvm_arch_flush_shadow_all(struct kvm *kvm);
void kvm_record_guest_s2_mapping(struct kvm_s2_mmu *mmu, gpa_t canonical_ipa,
				 gpa_t nested_ipa, size_t map_size,
				 struct kvm_guest_s2_mapping *mapping);
void kvm_remove_guest_s2_mappings(struct kvm_s2_mmu *mmu, gpa_t nipa, size_t size);
void kvm_destroy_guest_s2_mappings(struct rb_root_cached *tree);
void kvm_nested_unmap_cipa_range(struct kvm *kvm, gpa_t cipa, size_t unmap_size, bool may_block);

#include "extracted.c"

/* ------------------------------------------------------------------------ */

static struct kvm_vcpu vcpus[4];

static struct kvm *make_vm(int nr_vcpus)
{
	struct kvm *kvm = calloc(1, sizeof(*kvm));

	assert(kvm);
	rwlock_init(&kvm->mmu_lock);
	kvm_init_nested(kvm);
	assert(kvm_init_stage2_mmu(kvm, &kvm->arch.mmu, IA_BITS) == 0);
	for (int i = 0; i < nr_vcpus; i++) {
		kvm->online_vcpus = i + 1;
		vcpus[i] = (struct kvm_vcpu){ .kvm = kvm };
		assert(kvm_vcpu_init_nested(&vcpus[i]) == 0);
	}
	/* As get_s2_mmu_nested() leaves an MMU it handed to a vcpu. */
	for (size_t i = 0; i < kvm->arch.nested_mmus_size; i++)
		kvm->arch.nested_mmus[i].tlb_vttbr = (i + 1) << 48;
	return kvm;
}

static void destroy_vm(struct kvm *kvm, int nr_vcpus)
{
	kvm_arch_flush_shadow_all(kvm);
	for (int i = 0; i < nr_vcpus; i++)
		free_page(vcpus[i].arch.ctxt.vncr_array);
	free(kvm);
}

static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { failures++; fprintf(stderr, __VA_ARGS__); fputc('\n', stderr); } } while (0)

static void record(struct kvm *kvm, int mmu_idx, gpa_t cipa, gpa_t nipa, size_t size)
{
#ifdef HAVE_GUEST_S2_TRACKING
	struct kvm_guest_s2_mapping *m = kmalloc(sizeof(*m), GFP_KERNEL_ACCOUNT);

	/* user_mem_abort() records under kvm_fault_lock(), the read side. */
	read_lock(&kvm->mmu_lock);
	kvm_record_guest_s2_mapping(&kvm->arch.nested_mmus[mmu_idx], cipa, nipa, size, m);
	read_unlock(&kvm->mmu_lock);
#else
	/* Linux 6.18.50 keeps no record of a shadow mapping. */
	(void)kvm; (void)mmu_idx; (void)cipa; (void)nipa; (void)size;
#endif
}

static int mmu_of(struct kvm *kvm, struct kvm_pgtable *pgt)
{
	if (pgt == kvm->arch.mmu.pgt)
		return -1;
	for (size_t i = 0; i < kvm->arch.nested_mmus_size; i++)
		if (kvm->arch.nested_mmus[i].pgt == pgt)
			return (int)i;
	return -2;
}

/* Invalidate canonical [cipa, cipa + size) the way the MMU notifier does, and
 * require that exactly the listed nested ranges were unmapped. */
struct expect { int mmu; u64 addr, size; };

static void invalidate_and_expect(const char *what, struct kvm *kvm, gpa_t cipa, u64 size,
				  const struct expect *want, int nr_want)
{
	struct kvm_gfn_range range = {
		.start = cipa >> PAGE_SHIFT,
		.end = (cipa + size) >> PAGE_SHIFT,
		.may_block = true,
	};
	int nested = 0;

	nr_unmaps = 0;
	write_lock(&kvm->mmu_lock);
	kvm_unmap_gfn_range(kvm, &range);
	write_unlock(&kvm->mmu_lock);

	for (int i = 0; i < nr_unmaps; i++) {
		int mmu = mmu_of(kvm, unmaps[i].pgt);
		bool expected = false;

		if (mmu == -1)
			continue;
		nested++;
		for (int j = 0; j < nr_want; j++)
			if (want[j].mmu == mmu && want[j].addr == unmaps[i].addr &&
			    want[j].size == unmaps[i].size)
				expected = true;
		if (!expected) {
			CHECK(false, "%s: invalidating canonical [%#lx, +%#lx) unmapped nested MMU %d "
			      "[%#lx, +%#lx), which is not a shadow mapping of that range",
			      what, (unsigned long)cipa, (unsigned long)size, mmu,
			      (unsigned long)unmaps[i].addr, (unsigned long)unmaps[i].size);
			return;
		}
	}
	CHECK(nested == nr_want, "%s: %d nested unmaps, want %d", what, nested, nr_want);
}

static void check_teardown(const char *what)
{
	CHECK(live_allocations == 0, "%s: %ld allocations live after the VM was torn down",
	      what, live_allocations);
	CHECK(warnings == 0, "%s: %d WARN_ON fired", what, warnings);
}

#define A_CIPA 0x40000000UL
#define A_NIPA 0x80000000UL
#define B_CIPA 0x40200000UL
#define B_NIPA 0x90000000UL

/* An invalidation unmaps the shadow mappings of the invalidated canonical
 * range and leaves every other one alone. */
static void scenario_targeted(void)
{
	struct kvm *kvm = make_vm(1);

	record(kvm, 0, A_CIPA, A_NIPA, SZ_4K);
	record(kvm, 1, B_CIPA, B_NIPA, SZ_2M);

	invalidate_and_expect("page A", kvm, A_CIPA, SZ_4K,
			      (struct expect[]){ { 0, A_NIPA, SZ_4K } }, 1);
	invalidate_and_expect("one page of block B", kvm, B_CIPA + SZ_4K, SZ_4K,
			      (struct expect[]){ { 1, B_NIPA, SZ_2M } }, 1);
	invalidate_and_expect("page A again", kvm, A_CIPA, SZ_4K, NULL, 0);
#ifdef HAVE_GUEST_S2_TRACKING
	CHECK(tree_count(&kvm->arch.mmu.guest_s2_mappings) == 0,
	      "unmapped records stay in the canonical tree");
#endif
	destroy_vm(kvm, 1);
	check_teardown("targeted");
}

/* A memslot flush unmaps the shadow mappings of that slot only. */
static void scenario_memslot(void)
{
	struct kvm *kvm = make_vm(1);
	struct kvm_memory_slot slot = { .base_gfn = A_CIPA >> PAGE_SHIFT, .npages = 1 };
	int nested = 0;

	record(kvm, 0, A_CIPA, A_NIPA, SZ_4K);
	record(kvm, 1, B_CIPA, B_NIPA, SZ_2M);
	nr_unmaps = 0;
	kvm_arch_flush_shadow_memslot(kvm, &slot);
	for (int i = 0; i < nr_unmaps; i++) {
		int mmu = mmu_of(kvm, unmaps[i].pgt);

		if (mmu == -1)
			continue;
		nested++;
		CHECK(mmu == 0 && unmaps[i].addr == A_NIPA && unmaps[i].size == SZ_4K,
		      "flushing the memslot of canonical page A unmapped nested MMU %d [%#lx, +%#lx)",
		      mmu, (unsigned long)unmaps[i].addr, (unsigned long)unmaps[i].size);
		if (failures)
			break;
	}
	CHECK(failures || nested == 1, "memslot flush: %d nested unmaps, want 1", nested);
	destroy_vm(kvm, 1);
	check_teardown("memslot");
}

#ifdef HAVE_GUEST_S2_TRACKING
/* An unmap of the nested range (a guest TLBI, an MMU recycled for another
 * VMID) drops the records it fully covers, and keeps one it covers in part. */
static void scenario_tlbi_removal(void)
{
	struct kvm *kvm = make_vm(1);
	struct kvm_s2_mmu *mmu1 = &kvm->arch.nested_mmus[1];

	record(kvm, 1, B_CIPA, B_NIPA, SZ_2M);
	write_lock(&kvm->mmu_lock);
	kvm_stage2_unmap_range(mmu1, B_NIPA + SZ_4K, SZ_4K, true);
	write_unlock(&kvm->mmu_lock);
	CHECK(tree_count(&mmu1->guest_s2_mappings) == 1,
	      "a 4K unmap inside a 2M record dropped the record");
	invalidate_and_expect("block B after a partial unmap", kvm, B_CIPA, SZ_4K,
			      (struct expect[]){ { 1, B_NIPA, SZ_2M } }, 1);

	record(kvm, 1, B_CIPA, B_NIPA, SZ_2M);
	write_lock(&kvm->mmu_lock);
	kvm_stage2_unmap_range(mmu1, B_NIPA, SZ_2M, true);
	write_unlock(&kvm->mmu_lock);
	CHECK(tree_count(&mmu1->guest_s2_mappings) == 0 &&
	      tree_count(&kvm->arch.mmu.guest_s2_mappings) == 0,
	      "unmapping the whole nested range left its record");
	invalidate_and_expect("block B after a full unmap", kvm, B_CIPA, SZ_4K, NULL, 0);
	destroy_vm(kvm, 1);
	check_teardown("tlbi_removal");
}

/* kvm_vcpu_init_nested() moves nested_mmus when a vcpu is added. A record
 * made before the move still names its MMU afterwards. */
static void scenario_grow(void)
{
	struct kvm *kvm = make_vm(1);
	struct kvm_s2_mmu *before = kvm->arch.nested_mmus;

	record(kvm, 1, B_CIPA, B_NIPA, SZ_2M);
	kvm->online_vcpus = 2;
	vcpus[1] = (struct kvm_vcpu){ .kvm = kvm };
	assert(kvm_vcpu_init_nested(&vcpus[1]) == 0);
	assert(kvm->arch.nested_mmus != before && kvm->arch.nested_mmus_size == 4);

	invalidate_and_expect("block B after the array moved", kvm, B_CIPA, SZ_4K,
			      (struct expect[]){ { 1, B_NIPA, SZ_2M } }, 1);
	CHECK(tree_count(&kvm->arch.nested_mmus[1].guest_s2_mappings) == 0,
	      "the record was not removed from nested MMU 1's tree after the move");
	destroy_vm(kvm, 2);
	check_teardown("grow");
}

static void *flush_thread(void *kvm)
{
	sem_wait(&go_flush);
	kvm_arch_flush_shadow_all(kvm);
	return NULL;
}

/* kvm_arch_flush_shadow_all() runs while an invalidation has dropped the
 * mmu_lock to reschedule, and frees nested_mmus before the unmap resumes. */
static void scenario_teardown_race(void)
{
	struct kvm *kvm = make_vm(1);
	pthread_t flusher;

	record(kvm, 0, A_CIPA, A_NIPA, SZ_4K);
	record(kvm, 1, B_CIPA, B_NIPA, SZ_2M);
	sem_init(&go_flush, 0, 0);
	sem_init(&array_freed, 0, 0);
	sem_init(&unmapper_done, 0, 0);
	race_array = kvm->arch.nested_mmus;
	race_state = 1;
	pthread_create(&flusher, NULL, flush_thread, kvm);

	write_lock(&kvm->mmu_lock);
	kvm_nested_unmap_cipa_range(kvm, A_CIPA, B_CIPA + SZ_2M - A_CIPA, true);
	write_unlock(&kvm->mmu_lock);
	CHECK(race_state == 3, "the unmap never rescheduled, so the race did not run");
	sem_post(&unmapper_done);
	pthread_join(flusher, NULL);

	free_page(vcpus[0].arch.ctxt.vncr_array);
	free(kvm);
	check_teardown("teardown_race");
}
#endif

int main(int argc, char **argv)
{
	const char *name = argc > 1 ? argv[1] : "";

	if (!strcmp(name, "targeted"))
		scenario_targeted();
	else if (!strcmp(name, "memslot"))
		scenario_memslot();
#ifdef HAVE_GUEST_S2_TRACKING
	else if (!strcmp(name, "tlbi_removal"))
		scenario_tlbi_removal();
	else if (!strcmp(name, "grow"))
		scenario_grow();
	else if (!strcmp(name, "teardown_race"))
		scenario_teardown_race();
#endif
	else {
		fprintf(stderr, "unknown or unavailable scenario '%s'\n", name);
		return 2;
	}
	return failures ? 1 : 0;
}
