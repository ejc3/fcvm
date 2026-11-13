/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Runs the 7.3 GICv3 world switch against a model of one CPU's virtual CPU
 * interface: ICH_HCR_EL2, ICH_VMCR_EL2, the list registers, and ICH_MISR_EL2
 * computed from them as the GICv3 architecture defines it. The functions are
 * the release's own: vcpu load and put, guest entry and exit from
 * arch/arm64/kvm/hyp/vgic-v3-sr.c, and kvm_vgic_flush_hwstate() and
 * kvm_vgic_sync_hwstate() from arch/arm64/kvm/vgic/vgic.c, which the run loop
 * calls around every entry. The test compiles them from the excerpts, with or
 * without kernel/patches-arm64-host/vgic-v3-keep-hcr-on-quiet-exits.patch, into
 * extracted.c, which this file includes after the stubs below. The vgic work
 * those two call for a non-empty ap_list (filling LRs, folding them back,
 * pruning the list) is stubbed and counted.
 *
 *   loop     1000 entries and exits through the run loop's two calls with an
 *            empty ap_list, then 10 with one interrupt queued: counts the locks
 *            taken, the list walks, the LR folds and the GIC system register
 *            accesses.
 *   matrix   every combination of the maintenance interrupt enables, the
 *            guest's group enables before and after a run, and one list
 *            register in each state: after an exit the host must never hold a
 *            register state that raises a maintenance interrupt, and after an
 *            entry ICH_HCR_EL2 must hold what the vgic computed for the guest.
 *   putload  the same checks across vcpu put and load, and across another
 *            writer of ICH_HCR_EL2 between a put and the next load.
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef uint64_t u64;
typedef uint32_t u32;

#define BIT(n) (1UL << (n))
#define VGIC_V3_MAX_LRS 16

#define ICH_HCR_EL2_En BIT(0)
#define ICH_HCR_EL2_UIE BIT(1)
#define ICH_HCR_EL2_LRENPIE BIT(2)
#define ICH_HCR_EL2_NPIE BIT(3)
#define ICH_HCR_EL2_VGrp0EIE BIT(4)
#define ICH_HCR_EL2_VGrp0DIE BIT(5)
#define ICH_HCR_EL2_VGrp1EIE BIT(6)
#define ICH_HCR_EL2_VGrp1DIE BIT(7)
#define ICH_HCR_EL2_vSGIEOICount BIT(8)
#define ICH_HCR_EL2_TDIR BIT(14)
#define ICH_HCR_EL2_EOIcount (0x1fUL << 27)
#define ICH_VMCR_EL2_VENG0_MASK BIT(0)
#define ICH_VMCR_EL2_VENG1_MASK BIT(1)
#define ICH_LR_EOI (1ULL << 41)
#define ICH_LR_HW (1ULL << 61)
#define ICH_LR_STATE (3ULL << 62)
#define ICH_LR_PENDING_BIT (1ULL << 62)
#define ICH_LR_ACTIVE_BIT (1ULL << 63)
#define ICC_SRE_EL1_SRE (1U << 0)
#define ICC_SRE_EL2_ENABLE (1 << 3)

struct its_vpe {
	void *its_vm;
};

/* One CPU's GIC system registers, and how often the code under test touches them. */
enum gicreg { ICH_HCR_EL2, ICH_VMCR_EL2, ICH_MISR_EL2, ICH_ELRSR_EL2, ICC_SRE_EL1, ICC_SRE_EL2, NR_GICREGS };
static u64 hw[NR_GICREGS];
static u64 hw_lr[VGIC_V3_MAX_LRS];
static unsigned long reads[NR_GICREGS], writes[NR_GICREGS];

static u64 model_misr(void)
{
	u64 hcr = hw[ICH_HCR_EL2], vmcr = hw[ICH_VMCR_EL2], misr = 0;
	int valid = 0, pending = 0, eoi = 0;

	for (int i = 0; i < VGIC_V3_MAX_LRS; i++) {
		valid += !!(hw_lr[i] & ICH_LR_STATE);
		pending += !!(hw_lr[i] & ICH_LR_PENDING_BIT);
		eoi += !(hw_lr[i] & ICH_LR_STATE) && (hw_lr[i] & ICH_LR_EOI) && !(hw_lr[i] & ICH_LR_HW);
	}
	if (eoi)
		misr |= BIT(0);
	if ((hcr & ICH_HCR_EL2_UIE) && valid <= 1)
		misr |= BIT(1);
	if ((hcr & ICH_HCR_EL2_LRENPIE) && (hcr & ICH_HCR_EL2_EOIcount))
		misr |= BIT(2);
	if ((hcr & ICH_HCR_EL2_NPIE) && !pending)
		misr |= BIT(3);
	if ((hcr & ICH_HCR_EL2_VGrp0EIE) && (vmcr & ICH_VMCR_EL2_VENG0_MASK))
		misr |= BIT(4);
	if ((hcr & ICH_HCR_EL2_VGrp0DIE) && !(vmcr & ICH_VMCR_EL2_VENG0_MASK))
		misr |= BIT(5);
	if ((hcr & ICH_HCR_EL2_VGrp1EIE) && (vmcr & ICH_VMCR_EL2_VENG1_MASK))
		misr |= BIT(6);
	if ((hcr & ICH_HCR_EL2_VGrp1DIE) && !(vmcr & ICH_VMCR_EL2_VENG1_MASK))
		misr |= BIT(7);
	return misr;
}

/* The CPU interface signals a maintenance interrupt while En is set and a condition holds. */
static bool mi_asserted(void)
{
	return (hw[ICH_HCR_EL2] & ICH_HCR_EL2_En) && model_misr();
}

static u64 gic_read(enum gicreg r)
{
	reads[r]++;
	if (r == ICH_MISR_EL2)
		return model_misr();
	if (r == ICH_ELRSR_EL2) {
		u64 elrsr = 0;
		for (int i = 0; i < VGIC_V3_MAX_LRS; i++)
			if (!(hw_lr[i] & ICH_LR_STATE))
				elrsr |= BIT(i);
		return elrsr;
	}
	return hw[r];
}

static void gic_write(enum gicreg r, u64 v)
{
	writes[r]++;
	hw[r] = v;
}

#define read_gicreg(r) gic_read(r)
#define write_gicreg(v, r) gic_write(r, (v))
#define dsb(x) do { } while (0)
#define isb() do { } while (0)
#define static_branch_unlikely(k) (*(k))

static bool vgic_v3_cpuif_trap, vgic_v3_has_v2_compat;
static bool vhe = true;
static bool has_vhe(void) { return vhe; }
static u64 vgic_ich_hcr_trap_bits(void) { return 0; }
static u64 __gic_v3_get_lr(unsigned int i) { return hw_lr[i]; }
static void __gic_v3_set_lr(u64 v, int i) { hw_lr[i] = v; }

/* What arch/arm64/kvm/vgic/vgic.c's flush and sync use, for one vcpu on GICv3 with VHE. */
struct list_head {
	struct list_head *next, *prev;
};
static bool list_empty(const struct list_head *head) { return head->next == head; }

typedef struct {
	int held;
} raw_spinlock_t;
static unsigned long locks_taken;
static int raw_lock(raw_spinlock_t *l) { locks_taken++; l->held = 1; return 1; }
static int raw_unlock(raw_spinlock_t *l) { l->held = 0; return 0; }
#define scoped_guard(type, lock) for (int __held = raw_lock(lock); __held; __held = raw_unlock(lock))

struct vgic_v3_cpu_if;
#include "extracted-types.c"

struct vgic_v2_cpu_if {
	unsigned int used_lrs;
};
struct vgic_cpu {
	struct list_head ap_list_head;
	raw_spinlock_t ap_list_lock;
	struct vgic_v3_cpu_if vgic_v3;
	struct vgic_v2_cpu_if vgic_v2;
};
struct kvm {
	int unused;
};
struct kvm_vcpu {
	struct kvm *kvm;
	struct {
		struct vgic_cpu vgic_cpu;
	} arch;
};
struct ap_list_summary {
	unsigned int nr_pend, nr_act, nr_sgi;
};

enum vgic_type { VGIC_V2, VGIC_V3 };
static struct {
	enum vgic_type type;
	bool gicv3_cpuif;
	bool has_gicv4;
} kvm_vgic_global_state = { .type = VGIC_V3, .gicv3_cpuif = true };
static void *last_lr_irq __attribute__((unused));
#define host_data_ptr(field) (&field)
#define DEBUG_SPINLOCK_BUG_ON(x) do { } while (0)
#define KVM_REQ_GUEST_HYP_IRQ_PENDING 0

static bool vgic_is_v3(struct kvm *kvm) { return true; }
static bool vgic_is_v5(struct kvm *kvm) { return false; }
static bool vgic_state_is_nested(struct kvm_vcpu *vcpu) { return false; }
static bool vcpu_has_nv(struct kvm_vcpu *vcpu) { return false; }
static bool can_access_vgic_from_kernel(void) { return true; }
static bool vgic_supports_direct_irqs(struct kvm *kvm) { return false; }
static bool kvm_vgic_vcpu_pending_irq(struct kvm_vcpu *vcpu) { return false; }
static void kvm_make_request(int req, struct kvm_vcpu *vcpu) { }
static void vgic_v3_sync_nested(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v3_flush_nested(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v3_nested_update_mi(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v4_commit(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v5_save_state(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v5_restore_state(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v5_flush_ppi_state(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v2_save_state(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v2_restore_state(struct kvm_vcpu *vcpu) { abort(); }
static void vgic_v2_configure_hcr(struct kvm_vcpu *vcpu, struct ap_list_summary *als) { abort(); }

/* The vgic's choice of group enable interrupts for a VMCR (vgic_v3_configure_hcr()). */
static u64 group_mi_bits(u64 vmcr)
{
	return ((vmcr & ICH_VMCR_EL2_VENG0_MASK) ? ICH_HCR_EL2_VGrp0DIE : ICH_HCR_EL2_VGrp0EIE) |
	       ((vmcr & ICH_VMCR_EL2_VENG1_MASK) ? ICH_HCR_EL2_VGrp1DIE : ICH_HCR_EL2_VGrp1EIE);
}

static unsigned long lr_fills, folds, prunes;

/* vgic_v3_configure_hcr() for a summary with nothing outside the LRs. */
static void vgic_v3_configure_hcr(struct kvm_vcpu *vcpu, struct ap_list_summary *als)
{
	struct vgic_v3_cpu_if *cpuif = &vcpu->arch.vgic_cpu.vgic_v3;

	cpuif->vgic_hcr = ICH_HCR_EL2_En | group_mi_bits(cpuif->vgic_vmcr);
	if (!als->nr_sgi)
		cpuif->vgic_hcr |= ICH_HCR_EL2_vSGIEOICount;
}

/* Walks the ap_list and fills one LR per queued interrupt (here: at most one). */
static void vgic_flush_lr_state(struct kvm_vcpu *vcpu)
{
	struct vgic_v3_cpu_if *cpuif = &vcpu->arch.vgic_cpu.vgic_v3;
	struct ap_list_summary als = { .nr_pend = !list_empty(&vcpu->arch.vgic_cpu.ap_list_head) };

	lr_fills++;
	cpuif->used_lrs = als.nr_pend;
	cpuif->vgic_lr[0] = als.nr_pend ? ICH_LR_PENDING_BIT | 27 : 0;
	vgic_v3_configure_hcr(vcpu, &als);
}

static void vgic_fold_state(struct kvm_vcpu *vcpu)
{
	folds++;
	vcpu->arch.vgic_cpu.vgic_v3.used_lrs = 0;
}

static void vgic_prune_ap_list(struct kvm_vcpu *vcpu)
{
	scoped_guard(raw_spinlock, &vcpu->arch.vgic_cpu.ap_list_lock)
		prunes++;
}

#include "extracted.c"

static int failures;
#define CHECK(cond, ...)                                                       \
	do {                                                                   \
		if (!(cond)) {                                                 \
			fprintf(stderr, __VA_ARGS__);                          \
			fputc('\n', stderr);                                   \
			failures++;                                            \
		}                                                              \
	} while (0)

/*
 * Whether ICH_HCR_EL2 holds what the vgic computed for the guest. EOIcount is
 * read only while LRENPIE is set, so a count the guest left while it was clear
 * is not compared.
 */
static bool hcr_is_programmed(struct vgic_v3_cpu_if *cpu_if)
{
	u64 want = compute_ich_hcr(cpu_if), diff = hw[ICH_HCR_EL2] ^ want;

	if (!(want & ICH_HCR_EL2_LRENPIE))
		diff &= ~ICH_HCR_EL2_EOIcount;
	return !diff;
}

static void reset(struct vgic_v3_cpu_if *cpu_if)
{
	memset(hw, 0, sizeof(hw));
	memset(hw_lr, 0, sizeof(hw_lr));
	memset(reads, 0, sizeof(reads));
	memset(writes, 0, sizeof(writes));
	locks_taken = lr_fills = folds = prunes = 0;
	memset(cpu_if, 0, sizeof(*cpu_if));
	cpu_if->vgic_sre = ICC_SRE_EL1_SRE;
}

/*
 * A Linux guest with group 1 enabled and nothing in flight: the state of
 * almost every exit. Up to 6.18 such an exit took no lock, folded no LR and
 * touched no GIC register. 7.3 fills, folds and prunes the empty ap_list
 * under its lock, clears ICH_HCR_EL2 and reads ICH_MISR_EL2 on exit, and
 * writes ICH_HCR_EL2 again on entry: on a c9g.metal-48xl host an HVC exit
 * took 396 ns on 6.18.60 and 453 ns on 7.3.3 (fcvm #659).
 */
static void loop(void)
{
	struct kvm vm = { 0 };
	struct kvm_vcpu vcpu = { .kvm = &vm };
	struct vgic_cpu *vgic_cpu = &vcpu.arch.vgic_cpu;
	struct vgic_v3_cpu_if *cpu_if = &vgic_cpu->vgic_v3;
	struct list_head queued;
	const int exits = 1000, busy = 10;

	reset(cpu_if);
	vgic_cpu->ap_list_head.next = vgic_cpu->ap_list_head.prev = &vgic_cpu->ap_list_head;
	cpu_if->vgic_vmcr = ICH_VMCR_EL2_VENG1_MASK;
	hw[ICH_VMCR_EL2] = cpu_if->vgic_vmcr;
	__vgic_v3_activate_traps(cpu_if);
	for (int i = 0; i < exits; i++) {
		kvm_vgic_flush_hwstate(&vcpu);
		CHECK(hcr_is_programmed(cpu_if),
		      "loop: entry %d runs the guest with ICH_HCR_EL2 %#llx, not %#llx", i,
		      (unsigned long long)hw[ICH_HCR_EL2], (unsigned long long)compute_ich_hcr(cpu_if));
		kvm_vgic_sync_hwstate(&vcpu);
		CHECK(!mi_asserted(), "loop: exit %d leaves a maintenance interrupt asserted", i);
	}
	unsigned long gic = writes[ICH_HCR_EL2] + reads[ICH_HCR_EL2] + reads[ICH_MISR_EL2];
	CHECK(locks_taken == 0 && lr_fills == 0 && prunes == 0 && folds == 0 && gic <= 1,
	      "loop: %d exits with an empty ap_list took the ap_list_lock %lu times, walked it "
	      "%lu times to fill LRs and %lu times to prune it, folded the LRs %lu times, wrote "
	      "ICH_HCR_EL2 %lu times and read ICH_MISR_EL2 %lu times; an empty list needs no "
	      "lock, no walk and no fold, and ICH_HCR_EL2 needs one write, on the first entry",
	      exits, locks_taken, lr_fills, prunes, folds, writes[ICH_HCR_EL2],
	      reads[ICH_MISR_EL2]);

	/* An interrupt queued: every entry fills the LRs, every exit folds and prunes. */
	queued.next = queued.prev = &vgic_cpu->ap_list_head;
	vgic_cpu->ap_list_head.next = vgic_cpu->ap_list_head.prev = &queued;
	locks_taken = lr_fills = folds = prunes = 0;
	for (int i = 0; i < busy; i++) {
		kvm_vgic_flush_hwstate(&vcpu);
		CHECK(hcr_is_programmed(cpu_if) && hw_lr[0] == (ICH_LR_PENDING_BIT | 27),
		      "loop: busy entry %d runs the guest without its LR (ICH_HCR_EL2 %#llx, LR0 %#llx)",
		      i, (unsigned long long)hw[ICH_HCR_EL2], (unsigned long long)hw_lr[0]);
		kvm_vgic_sync_hwstate(&vcpu);
		CHECK(!mi_asserted(), "loop: busy exit %d leaves a maintenance interrupt asserted", i);
	}
	CHECK(lr_fills == busy && folds == busy && prunes == busy,
	      "loop: %d exits with an interrupt queued filled the LRs %lu times, folded them %lu "
	      "times and pruned %lu times", busy, lr_fills, folds, prunes);

	__vgic_v3_deactivate_traps(cpu_if);
	CHECK(hw[ICH_HCR_EL2] == 0, "loop: put leaves ICH_HCR_EL2 %#llx", (unsigned long long)hw[ICH_HCR_EL2]);
}

static const u64 lr_cases[] = {
	0,
	ICH_LR_PENDING_BIT | 27,
	ICH_LR_ACTIVE_BIT | 27,
	ICH_LR_EOI | ICH_LR_PENDING_BIT | 27,
};

/* What the guest does to one list register while it runs: nothing, or completes it. */
static u64 guest_lr(u64 lr, bool completes)
{
	return completes ? lr & ~ICH_LR_STATE : lr;
}

static void one_cycle(const char *name, u64 mi_bits, u64 vmcr_before, u64 vmcr_after,
		      u64 lr, bool completes, u64 eoicount)
{
	struct vgic_v3_cpu_if cpu_if;

	reset(&cpu_if);
	cpu_if.vgic_vmcr = vmcr_before;
	hw[ICH_VMCR_EL2] = vmcr_before;
	__vgic_v3_activate_traps(&cpu_if);

	/* Two entries and exits: the second starts from whatever the first left. */
	for (int round = 0; round < 2; round++) {
		cpu_if.vgic_hcr = ICH_HCR_EL2_En | mi_bits;
		cpu_if.used_lrs = lr ? 1 : 0;
		cpu_if.vgic_lr[0] = lr;
		__vgic_v3_restore_state(&cpu_if);
		CHECK(hcr_is_programmed(&cpu_if),
		      "%s: hcr %#llx vmcr %#llx->%#llx lr %#llx round %d: entry leaves ICH_HCR_EL2 %#llx",
		      name, (unsigned long long)mi_bits, (unsigned long long)vmcr_before,
		      (unsigned long long)vmcr_after, (unsigned long long)lr, round,
		      (unsigned long long)hw[ICH_HCR_EL2]);
		/* The guest runs: it may change its group enables, complete the LR, EOI. */
		hw[ICH_VMCR_EL2] = vmcr_after;
		hw_lr[0] = guest_lr(hw_lr[0], completes);
		hw[ICH_HCR_EL2] |= eoicount << 27;
		__vgic_v3_save_state(&cpu_if);
		CHECK(!mi_asserted(),
		      "%s: hcr %#llx vmcr %#llx->%#llx lr %#llx%s eoicount %llu round %d: the host "
		      "runs with ICH_HCR_EL2 %#llx raising maintenance interrupt %#llx",
		      name, (unsigned long long)mi_bits, (unsigned long long)vmcr_before,
		      (unsigned long long)vmcr_after, (unsigned long long)lr,
		      completes ? " completed" : "", (unsigned long long)eoicount, round,
		      (unsigned long long)hw[ICH_HCR_EL2], (unsigned long long)model_misr());
		CHECK(cpu_if.vgic_vmcr == vmcr_after, "%s: exit did not save the guest's VMCR", name);
		vmcr_before = vmcr_after;
	}

	__vgic_v3_deactivate_traps(&cpu_if);
	CHECK(hw[ICH_HCR_EL2] == 0, "%s: hcr %#llx: put leaves ICH_HCR_EL2 %#llx", name,
	      (unsigned long long)mi_bits, (unsigned long long)hw[ICH_HCR_EL2]);
}

static void matrix(void)
{
	const u64 enables[] = { ICH_HCR_EL2_UIE, ICH_HCR_EL2_LRENPIE, ICH_HCR_EL2_NPIE,
				ICH_HCR_EL2_VGrp0EIE, ICH_HCR_EL2_VGrp0DIE,
				ICH_HCR_EL2_VGrp1EIE, ICH_HCR_EL2_VGrp1DIE };

	for (unsigned int set = 0; set < 1u << 7; set++) {
		u64 mi_bits = ICH_HCR_EL2_vSGIEOICount;
		for (int b = 0; b < 7; b++)
			if (set & (1u << b))
				mi_bits |= enables[b];
		for (u64 before = 0; before < 4; before++)
			for (u64 after = 0; after < 4; after++)
				for (unsigned int l = 0; l < sizeof(lr_cases) / sizeof(lr_cases[0]); l++)
					for (int completes = 0; completes < 2; completes++)
						for (u64 eoi = 0; eoi < 2; eoi++)
							one_cycle("matrix", mi_bits, before, after,
								  lr_cases[l], completes, eoi);
	}
}

/*
 * A put clears what an exit left programmed, and a load followed by an entry
 * programs the guest's value even when another writer (the nested vgic, or
 * another vcpu) changed ICH_HCR_EL2 while this vcpu was not loaded.
 */
static void putload(void)
{
	struct vgic_v3_cpu_if cpu_if;
	u64 vmcr = ICH_VMCR_EL2_VENG1_MASK;

	reset(&cpu_if);
	cpu_if.vgic_vmcr = vmcr;
	cpu_if.vgic_hcr = ICH_HCR_EL2_En | group_mi_bits(vmcr);
	hw[ICH_VMCR_EL2] = vmcr;
	for (int round = 0; round < 3; round++) {
		__vgic_v3_activate_traps(&cpu_if);
		__vgic_v3_restore_state(&cpu_if);
		CHECK(hcr_is_programmed(&cpu_if),
		      "putload: round %d: entry after load leaves ICH_HCR_EL2 %#llx", round,
		      (unsigned long long)hw[ICH_HCR_EL2]);
		__vgic_v3_save_state(&cpu_if);
		__vgic_v3_deactivate_traps(&cpu_if);
		CHECK(hw[ICH_HCR_EL2] == 0, "putload: round %d: put leaves ICH_HCR_EL2 %#llx", round,
		      (unsigned long long)hw[ICH_HCR_EL2]);
		/* Someone else programs the CPU interface before this vcpu is loaded again. */
		hw[ICH_HCR_EL2] = ICH_HCR_EL2_En | ICH_HCR_EL2_NPIE | ICH_HCR_EL2_TDIR;
	}
}

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: %s loop|matrix|putload\n", argv[0]);
		return 2;
	}
	if (!strcmp(argv[1], "loop"))
		loop();
	else if (!strcmp(argv[1], "matrix"))
		matrix();
	else if (!strcmp(argv[1], "putload"))
		putload();
	else
		return 2;
	if (failures) {
		fprintf(stderr, "%s: %d failed checks\n", argv[1], failures);
		return 1;
	}
	return 0;
}
