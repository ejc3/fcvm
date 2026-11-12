/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Runs the 7.3 HCR_EL2 hand-off around a guest hypervisor's run (fcvm #659).
 * Since 7.3 KVM keeps the guest's HCR_EL2 outside the VNCR page and, around
 * every run of a vCPU in hyp context, copies it into the page's NVHCR_EL2 slot
 * (__compute_hcr() on entry) and back (fixup_nv_guest_exit() on exit), where a
 * guest hypervisor reads and writes it without trapping. The functions are the
 * release's own, from arch/arm64/kvm/hyp/vhe/switch.c, compiled with or without
 * kernel/patches-arm64-host/nv-guest-hypervisor-exits-without-calls.patch into
 * extracted.c, which this file includes after the stubs below.
 *
 * kvm_vcpu_apply_reg_masks() applies a register's RES0 and RES1 masks (in the
 * release, a call on every use); each use is counted. The helpers the two
 * functions call (is_hyp_ctxt(), vcpu_el2_e2h_is_set()) read the raw storage
 * here, so the count names only the masking the two functions do themselves.
 *
 *   quiet    1000 runs in hyp context in which the guest leaves HCR_EL2 alone,
 *            on a host without FEAT_NV3: the only masking per run may be the
 *            constant one __compute_hcr() uses for the RES1 bits.
 *   write    the guest writes NVHCR_EL2 with RES0 bits set and RES1 bits
 *            clear between entry and exit: after the exit HCR_EL2 must hold
 *            the written value sanitised, the next entry must publish that
 *            value, and __compute_hcr() must return what the release returns.
 *   nv3      the same on a host with FEAT_NV3, for a VHE guest hypervisor
 *            (the hardware NVHCR_EL2) and a non-VHE one (the VNCR page, with
 *            NVHCR_EL2's own masks).
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef uint64_t u64;
typedef uint32_t u32;

#define BIT(n) (1UL << (n))
#define GENMASK(h, l) (((~0UL) << (l)) & (~0UL >> (63 - (h))))
#define PAGE_SHIFT 12
#define unlikely(x) (x)

#define HCR_TWE BIT(14)
#define HCR_TGE BIT(27)
#define HCR_E2H BIT(34)
#define HCR_FIEN BIT(47)
#define HCR_API BIT(41)
#define HCR_APK BIT(40)
#define HCR_AT BIT(44)
#define HCR_NV BIT(42)
#define HCR_NV1 BIT(43)
#define HCR_NV2 BIT(45)
#define HCR_TTLB BIT(25)
#define HCR_EL2_TWEDEn BIT(59)
#define HCR_EL2_TWEDEL GENMASK(63, 60)

#define PSR_MODE_EL1t 0x4
#define PSR_MODE_EL1h 0x5
#define PSR_MODE_EL2t 0x8
#define PSR_MODE_EL2h 0x9
#define PSR_MODE_MASK 0xf
#define PSR_MODE32_BIT 0x10

/* The sanitised registers, then the VNCR-backed ones, as in kvm_host.h. */
enum vcpu_sysreg {
	__INVALID_SYSREG__,
	__SANITISED_REG_START__,
	VNCR_EL2 = __SANITISED_REG_START__,
	HCR_EL2,
	__VNCR_START__,
	NVHCR_EL2 = __VNCR_START__,
	NR_SYS_REGS,
};

struct kvm_cpu_context {
	u64 sys_regs[NR_SYS_REGS];
	u64 *vncr_array;
	u64 pstate;
};

struct kvm {
	bool nv3;	/* kvm_has_nv3(): the host has FEAT_NV3 and the VM uses it */
};

struct kvm_vcpu {
	struct {
		u64 hcr_el2;
		struct kvm_cpu_context ctxt;
	} arch;
	struct kvm *kvm;
};

enum { ARM64_HAS_NV3 = 1, ARM64_HAS_OTHER };
enum { SYS_NVHCR_EL2 = 1, SYS_VNCR_EL2 };
enum { VCPU_IN_HYP_CONTEXT = 1, L1_VNCR_MAPPED = 2 };

static bool host_nv3;
static u64 hw_nvhcr, hw_vncr;
static unsigned int host_flags;
static long mask_calls;

/* The RES0 and RES1 bits of the model's HCR_EL2 (any fixed choice will do). */
#define HCR_RES0 (BIT(48) | BIT(49) | GENMASK(55, 52))
#define HCR_RES1 BIT(1)
/* Without FEAT_NV3, NVHCR_EL2 has no masks of its own (config.c). */
#define NVHCR_RES0 (BIT(48) | BIT(50))
#define NVHCR_RES1 0UL

static bool cpus_have_final_cap(int cap)
{
	return cap == ARM64_HAS_NV3 && host_nv3;
}

static u64 res0_of(const struct kvm_vcpu *vcpu, enum vcpu_sysreg sr)
{
	if (sr == HCR_EL2)
		return HCR_RES0;
	if (sr == NVHCR_EL2 && vcpu->kvm->nv3)
		return NVHCR_RES0;
	return 0;
}

static u64 res1_of(const struct kvm_vcpu *vcpu, enum vcpu_sysreg sr)
{
	if (sr == HCR_EL2)
		return HCR_RES1;
	if (sr == NVHCR_EL2 && vcpu->kvm->nv3)
		return NVHCR_RES1;
	return 0;
}

static u64 sanitise(const struct kvm_vcpu *vcpu, enum vcpu_sysreg sr, u64 v)
{
	return (v & ~res0_of(vcpu, sr)) | res1_of(vcpu, sr);
}

static u64 kvm_vcpu_apply_reg_masks(const struct kvm_vcpu *vcpu,
				    enum vcpu_sysreg sr, u64 v)
{
	mask_calls++;
	return sanitise(vcpu, sr, v);
}

static u64 *__ctxt_sys_reg(const struct kvm_cpu_context *ctxt, enum vcpu_sysreg r)
{
	if (r >= __VNCR_START__ && ctxt->vncr_array)
		return &ctxt->vncr_array[r - __VNCR_START__];
	return (u64 *)&ctxt->sys_regs[r];
}

#define ctxt_sys_reg(c, r) (*__ctxt_sys_reg(c, r))
#define vcpu_has_nv(v) (true)

#define __vcpu_assign_sys_reg(v, r, val)				\
	do {								\
		const struct kvm_cpu_context *ctxt = &(v)->arch.ctxt;	\
		u64 __v = (val);					\
		if (vcpu_has_nv((v)) && (r) >= __SANITISED_REG_START__)	\
			__v = kvm_vcpu_apply_reg_masks((v), (r), __v);	\
									\
		ctxt_sys_reg(ctxt, (r)) = __v;				\
	} while (0)

#define __vcpu_sys_reg(v, r)						\
	({								\
		const struct kvm_cpu_context *ctxt = &(v)->arch.ctxt;	\
		u64 __v = ctxt_sys_reg(ctxt, (r));			\
		if (vcpu_has_nv((v)) && (r) >= __SANITISED_REG_START__)	\
			__v = kvm_vcpu_apply_reg_masks((v), (r), __v);	\
		__v;							\
	})

static bool vcpu_el2_e2h_is_set(const struct kvm_vcpu *vcpu)
{
	return ctxt_sys_reg(&vcpu->arch.ctxt, HCR_EL2) & HCR_E2H;
}

static bool is_hyp_ctxt(const struct kvm_vcpu *vcpu)
{
	u64 hcr = ctxt_sys_reg(&vcpu->arch.ctxt, HCR_EL2);
	u64 mode = vcpu->arch.ctxt.pstate & PSR_MODE_MASK;

	return ((hcr & HCR_E2H) && (hcr & HCR_TGE)) ||
	       mode == PSR_MODE_EL2h || mode == PSR_MODE_EL2t;
}

#define vcpu_cpsr(v) (&(v)->arch.ctxt.pstate)
#define host_data_set_flag(f) (host_flags |= (f))
#define host_data_clear_flag(f) (host_flags &= ~(f))
#define host_data_test_flag(f) (!!(host_flags & (f)))
#define smp_processor_id() 0
#define vncr_fixmap(cpu) (cpu)
#define __fix_to_virt(x) (0xffff800000000000UL + ((u64)(x) << PAGE_SHIFT))
#define BUG_ON(c)							\
	do {								\
		if (c) {						\
			fprintf(stderr, "BUG_ON(%s) at line %d\n", #c, __LINE__); \
			exit(1);					\
		}							\
	} while (0)

static void write_sysreg_s_fn(u64 v, int reg)
{
	if (reg == SYS_NVHCR_EL2)
		hw_nvhcr = v;
	else
		hw_vncr = v;
}

static u64 read_sysreg_s_fn(int reg)
{
	return reg == SYS_NVHCR_EL2 ? hw_nvhcr : hw_vncr;
}

#define write_sysreg_s(v, r) write_sysreg_s_fn((u64)(v), (r))
#define read_sysreg_s(r) read_sysreg_s_fn(r)

#include "extracted.c"

static u64 vncr_page[NR_SYS_REGS];
static struct kvm vm;
static struct kvm_vcpu vcpu;

static void reset(bool nv3_host, bool vhe_guest)
{
	host_nv3 = nv3_host;
	vm.nv3 = nv3_host;
	memset(&vcpu, 0, sizeof(vcpu));
	memset(vncr_page, 0, sizeof(vncr_page));
	vcpu.kvm = &vm;
	vcpu.arch.ctxt.vncr_array = vncr_page;
	vcpu.arch.hcr_el2 = BIT(0) | BIT(3);
	/*
	 * KVM keeps HCR_EL2 sanitised: every writer masks it, and so does vCPU init.
	 * Bit 50 is valid in HCR_EL2 and RES0 in the model's NVHCR_EL2 with FEAT_NV3.
	 */
	ctxt_sys_reg(&vcpu.arch.ctxt, HCR_EL2) =
		sanitise(&vcpu, HCR_EL2, (vhe_guest ? HCR_E2H : 0) | HCR_TWE | BIT(30) |
			 BIT(50));
	vcpu.arch.ctxt.pstate = PSR_MODE_EL2h;
	host_flags = 0;
	hw_nvhcr = 0;
	mask_calls = 0;
}

static bool nv3_live(void)
{
	return host_nv3 && vcpu_el2_e2h_is_set(&vcpu);
}

/* What the 7.3.3 __compute_hcr() returns for a vCPU in hyp context. */
static u64 expected_hcr(void)
{
	u64 hcr = vcpu.arch.hcr_el2 | HCR_NV | HCR_NV2 | HCR_AT | HCR_TTLB;

	if (!vcpu_el2_e2h_is_set(&vcpu))
		hcr |= HCR_NV1;
	return hcr | (sanitise(&vcpu, HCR_EL2, 0) & ~NV_HCR_GUEST_EXCLUDE);
}

/* The guest's view of HCR_EL2 while it runs, as the 7.3.3 entry publishes it. */
static u64 published(void)
{
	return nv3_live() ? hw_nvhcr : ctxt_sys_reg(&vcpu.arch.ctxt, NVHCR_EL2);
}

static u64 expected_published(u64 canonical)
{
	return nv3_live() ? canonical : sanitise(&vcpu, NVHCR_EL2, canonical);
}

static void guest_writes(u64 v)
{
	if (nv3_live())
		hw_nvhcr = v;
	else
		ctxt_sys_reg(&vcpu.arch.ctxt, NVHCR_EL2) = v;
}

static int fail(const char *what, int round, u64 got, u64 want)
{
	fprintf(stderr, "%s, round %d: 0x%llx, expected 0x%llx\n", what, round,
		(unsigned long long)got, (unsigned long long)want);
	return 1;
}

/* One run: entry, the guest's writes (if any), the exit. */
static int run(int round, bool write, u64 value)
{
	u64 canonical = ctxt_sys_reg(&vcpu.arch.ctxt, HCR_EL2);
	u64 want = expected_hcr(), got = __compute_hcr(&vcpu);
	u64 before;

	if (got != want)
		return fail("__compute_hcr() returned", round, got, want);
	if (published() != expected_published(canonical))
		return fail("the entry published HCR_EL2 as", round, published(),
			    expected_published(canonical));
	if (hw_vncr != (u64)(uintptr_t)vncr_page)
		return fail("VNCR_EL2 pointed at", round, hw_vncr, (u64)(uintptr_t)vncr_page);

	before = published();
	if (write)
		guest_writes(value);
	/* The guest ran at EL1; the exit maps PSTATE back to EL2. */
	vcpu.arch.ctxt.pstate = PSR_MODE_EL1h;
	fixup_nv_guest_exit(&vcpu);
	if ((vcpu.arch.ctxt.pstate & PSR_MODE_MASK) != PSR_MODE_EL2h)
		return fail("PSTATE.M after the exit", round,
			    vcpu.arch.ctxt.pstate & PSR_MODE_MASK, PSR_MODE_EL2h);

	{
		u64 seen = write ? value : before;
		u64 from = nv3_live() ? seen : sanitise(&vcpu, NVHCR_EL2, seen);
		u64 expect = sanitise(&vcpu, HCR_EL2, from);
		u64 now = ctxt_sys_reg(&vcpu.arch.ctxt, HCR_EL2);

		if (now != expect)
			return fail("HCR_EL2 after the exit", round, now, expect);
	}
	return 0;
}

static int scenario_quiet(void)
{
	reset(false, true);
	for (int i = 0; i < 1000; i++)
		if (run(i, false, 0))
			return 1;
	/* One constant masking per run: guest_hcr = kvm_vcpu_apply_reg_masks(vcpu, HCR_EL2, 0). */
	if (mask_calls > 1000) {
		fprintf(stderr,
			"1000 quiet hyp-context runs masked HCR_EL2 or NVHCR_EL2 %ld times, "
			"expected at most 1000 (one constant masking per entry): the copies "
			"between HCR_EL2 and NVHCR_EL2 call kvm_vcpu_apply_reg_masks() on a "
			"host without FEAT_NV3\n", mask_calls);
		return 1;
	}
	printf("quiet: %ld masking calls in 1000 runs\n", mask_calls);
	return 0;
}

static const u64 writes[] = {
	0,
	HCR_E2H,
	HCR_E2H | HCR_TGE,
	HCR_E2H | HCR_RES0,
	HCR_E2H | HCR_RES0 | NVHCR_RES0 | BIT(30),
	HCR_E2H | BIT(5) | BIT(31) | HCR_TWE,
	~0UL,
	~HCR_RES1,
};

static int scenario_write(bool nv3_host, bool vhe_guest)
{
	int round = 0;

	reset(nv3_host, vhe_guest);
	for (unsigned int i = 0; i < sizeof(writes) / sizeof(writes[0]); i++) {
		/* Keep the guest's E2H as it was, so that one run tests one path. */
		u64 v = (writes[i] & ~HCR_E2H) | (vhe_guest ? HCR_E2H : 0);

		if (run(round++, true, v) || run(round++, false, 0))
			return 1;
	}
	printf("write (FEAT_NV3 %s, %s guest hypervisor): %d runs\n",
	       nv3_host ? "on" : "off", vhe_guest ? "VHE" : "non-VHE", round);
	return 0;
}

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: %s quiet|write|nv3\n", argv[0]);
		return 2;
	}
	if (!strcmp(argv[1], "quiet"))
		return scenario_quiet();
	if (!strcmp(argv[1], "write"))
		return scenario_write(false, true) || scenario_write(false, false);
	if (!strcmp(argv[1], "nv3"))
		return scenario_write(true, true) || scenario_write(true, false);
	fprintf(stderr, "unknown scenario %s\n", argv[1]);
	return 2;
}
