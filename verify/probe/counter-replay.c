// What a userspace write of the guest counters does on arm64 KVM, for a plain vCPU and for
// one with EL2 (KVM_ARM_VCPU_HAS_EL2), with and without KVM_ARM_SET_COUNTER_OFFSET.
//
// A VMM restores a snapshot by writing each saved register back. This program does the
// counter part of that on a new VM and reads the counters after a sleep, to settle by
// measurement which of two accounts of fcvm #630 the kernel implements (verify/ResumeVerify/
// README.md, "Least sure", 1):
//
//   - the model's reading of arch/arm64/kvm (arch_timer.c, sys_regs.c): a write of CNTVCT or
//     CNTPCT stores (host counter - value) in a VM-wide offset; nothing is kept in CNTVOFF_EL2;
//     so after the writes the counters continue from the values written;
//   - the project's record: for a vCPU with EL2 the adjustment is kept in CNTVOFF_EL2 storage,
//     and writing the saved CNTVOFF_EL2 afterwards clobbers it, so the counters read near the
//     host's.
//
// It runs no guest code: KVM_SET_ONE_REG and KVM_GET_ONE_REG on a vCPU that never enters
// KVM_RUN. Output is one line per case, with each counter classified against the value
// written, the sleep and the host's counter; run.sh compares it with expected.txt.
//
// Needs: an arm64 host with /dev/kvm. The EL2 cases need a host booted with
// kvm-arm.mode=nested on hardware with FEAT_NV2; without it the program exits 3.
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <linux/kvm.h>

#ifndef __aarch64__
#error "arm64 only"
#endif

#define SYSREG(op0, op1, crn, crm, op2) \
	(KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM64_SYSREG | \
	 ((uint64_t)(op0) << 14) | ((uint64_t)(op1) << 11) | ((uint64_t)(crn) << 7) | \
	 ((uint64_t)(crm) << 3) | (uint64_t)(op2))

#define REG_CNTPCT_EL0 SYSREG(3, 3, 14, 0, 1)
#define REG_CNTVOFF_EL2 SYSREG(3, 4, 14, 0, 3)
// KVM_REG_ARM_TIMER_CNT is the id the kernel maps to CNTVCT_EL0 (kvm_one_reg_to_id).

#ifndef KVM_ARM_VCPU_HAS_EL2
#define KVM_ARM_VCPU_HAS_EL2 7
#endif
#ifndef KVM_CAP_ARM_EL2
#define KVM_CAP_ARM_EL2 240
#endif

static uint64_t host_counter(void)
{
	uint64_t v;
	__asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(v));
	return v;
}

static uint64_t counter_freq(void)
{
	uint64_t v;
	__asm__ volatile("mrs %0, cntfrq_el0" : "=r"(v));
	return v;
}

static void die(const char *what)
{
	fprintf(stderr, "counter-replay: %s: %s\n", what, strerror(errno));
	exit(2);
}

static void set_reg(int vcpu, uint64_t id, uint64_t val, const char *name)
{
	struct kvm_one_reg r = { .id = id, .addr = (uint64_t)&val };
	if (ioctl(vcpu, KVM_SET_ONE_REG, &r) < 0)
		die(name);
}

static uint64_t get_reg(int vcpu, uint64_t id, const char *name)
{
	uint64_t val = 0;
	struct kvm_one_reg r = { .id = id, .addr = (uint64_t)&val };
	if (ioctl(vcpu, KVM_GET_ONE_REG, &r) < 0)
		die(name);
	return val;
}

struct vm {
	int vm, vcpu;
};

static struct vm new_vm(int kvm, int el2)
{
	struct vm v;
	struct kvm_vcpu_init init;

	v.vm = ioctl(kvm, KVM_CREATE_VM, 0);
	if (v.vm < 0)
		die("KVM_CREATE_VM");
	v.vcpu = ioctl(v.vm, KVM_CREATE_VCPU, 0);
	if (v.vcpu < 0)
		die("KVM_CREATE_VCPU");
	memset(&init, 0, sizeof(init));
	if (ioctl(v.vm, KVM_ARM_PREFERRED_TARGET, &init) < 0)
		die("KVM_ARM_PREFERRED_TARGET");
	init.features[0] |= 1u << KVM_ARM_VCPU_PSCI_0_2;
	if (el2)
		init.features[0] |= 1u << KVM_ARM_VCPU_HAS_EL2;
	if (ioctl(v.vcpu, KVM_ARM_VCPU_INIT, &init) < 0)
		die(el2 ? "KVM_ARM_VCPU_INIT with HAS_EL2" : "KVM_ARM_VCPU_INIT");
	return v;
}

static void drop_vm(struct vm v)
{
	close(v.vcpu);
	close(v.vm);
}

// What a counter read `got` is, given the value written `v`, the nested offset `x`, the
// host counter ticks slept `d` and the host counter now `host`. `tol` is 50 ms of ticks.
static const char *classify(uint64_t got, uint64_t v, uint64_t x, uint64_t d, uint64_t host,
			    uint64_t tol)
{
	static char other[64];
	int64_t e;

	e = (int64_t)(got - (v + d));
	if (e > -(int64_t)tol && e < (int64_t)tol)
		return "the value written plus the sleep";
	if (x) {
		e = (int64_t)(got - (v - x + d));
		if (e > -(int64_t)tol && e < (int64_t)tol)
			return "the value written plus the sleep, less CNTVOFF_EL2";
		e = (int64_t)(got - (v - 2 * x + d));
		if (e > -(int64_t)tol && e < (int64_t)tol)
			return "the value written plus the sleep, less twice CNTVOFF_EL2";
	}
	e = (int64_t)(got - host);
	if (e > -(int64_t)tol && e < (int64_t)tol)
		return "the host's counter";
	if (x) {
		e = (int64_t)(got - (host - x));
		if (e > -(int64_t)tol && e < (int64_t)tol)
			return "the host's counter less CNTVOFF_EL2";
	}
	snprintf(other, sizeof(other), "OTHER");
	return other;
}

// One restore-like replay on a new VM. `x`: the saved CNTVOFF_EL2 (EL2 vCPUs only; the saved
// virtual counter is then the saved physical one less x, as a guest hypervisor's is).
// `ioctl_first`: KVM_ARM_SET_COUNTER_OFFSET before the replay, as the fork's fix does.
static void replay(int kvm, const char *label, int el2, uint64_t x, int ioctl_first)
{
	uint64_t freq = counter_freq();
	uint64_t tol = freq / 20;
	struct vm v = new_vm(kvm, el2);
	uint64_t t0 = host_counter();
	uint64_t saved_p = t0 + 10000 * freq; // far from the host's counter, either way
	uint64_t saved_v = saved_p - x;
	uint64_t p, c, t1;
	struct timespec ts = { .tv_sec = 1, .tv_nsec = 0 };

	if (ioctl_first) {
		struct kvm_arm_counter_offset off = { .counter_offset = t0 - saved_p };
		if (ioctl(v.vm, KVM_ARM_SET_COUNTER_OFFSET, &off) < 0)
			die("KVM_ARM_SET_COUNTER_OFFSET");
	}
	// KVM_GET_REG_LIST order: CNTPCT_EL0, CNTVCT_EL0, and far later CNTVOFF_EL2.
	set_reg(v.vcpu, REG_CNTPCT_EL0, saved_p, "set CNTPCT_EL0");
	set_reg(v.vcpu, KVM_REG_ARM_TIMER_CNT, saved_v, "set KVM_REG_ARM_TIMER_CNT");
	if (el2)
		set_reg(v.vcpu, REG_CNTVOFF_EL2, x, "set CNTVOFF_EL2");
	nanosleep(&ts, NULL);
	p = get_reg(v.vcpu, REG_CNTPCT_EL0, "get CNTPCT_EL0");
	c = get_reg(v.vcpu, KVM_REG_ARM_TIMER_CNT, "get KVM_REG_ARM_TIMER_CNT");
	t1 = host_counter();

	printf("%s: CNTPCT_EL0 reads %s; CNTVCT_EL0 reads %s\n", label,
	       classify(p, saved_p, x, t1 - t0, t1, tol),
	       classify(c, saved_p, x, t1 - t0, t1, tol));
	fprintf(stderr,
		"  %s: freq %" PRIu64 " host %" PRIu64 " -> %" PRIu64 " written P %" PRIu64 " V %" PRIu64
		" read P %" PRIu64 " V %" PRIu64 "\n",
		label, freq, t0, t1, saved_p, saved_v, p, c);
	drop_vm(v);
}

int main(void)
{
	int kvm = open("/dev/kvm", O_RDWR | O_CLOEXEC);
	uint64_t x;

	if (kvm < 0)
		die("/dev/kvm");
	if (ioctl(kvm, KVM_CHECK_EXTENSION, KVM_CAP_COUNTER_OFFSET) <= 0) {
		fprintf(stderr, "counter-replay: this kernel has no KVM_CAP_COUNTER_OFFSET\n");
		return 2;
	}
	x = 100 * counter_freq();

	replay(kvm, "plain vCPU, registers replayed", 0, 0, 0);
	replay(kvm, "plain vCPU, KVM_ARM_SET_COUNTER_OFFSET then the replay", 0, 0, 1);

	if (ioctl(kvm, KVM_CHECK_EXTENSION, KVM_CAP_ARM_EL2) <= 0) {
		fprintf(stderr,
			"counter-replay: no KVM_CAP_ARM_EL2: boot this host with kvm-arm.mode=nested "
			"on hardware with FEAT_NV2 to run the EL2 cases\n");
		return 3;
	}
	replay(kvm, "EL2 vCPU, CNTVOFF_EL2 = 0, registers replayed", 1, 0, 0);
	replay(kvm, "EL2 vCPU, CNTVOFF_EL2 = X, registers replayed", 1, x, 0);
	replay(kvm, "EL2 vCPU, CNTVOFF_EL2 = 0, KVM_ARM_SET_COUNTER_OFFSET then the replay", 1, 0, 1);
	replay(kvm, "EL2 vCPU, CNTVOFF_EL2 = X, KVM_ARM_SET_COUNTER_OFFSET then the replay", 1, x, 1);
	return 0;
}
