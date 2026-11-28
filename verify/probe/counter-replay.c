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
// KVM_RUN. Per case: the saved physical counter is P, the saved CNTVOFF_EL2 is X, and the
// saved virtual counter is V = P - X, as a guest hypervisor's is. Without the ioctl the saved
// values are written. With KVM_ARM_SET_COUNTER_OFFSET first (computed from P, as the fork's
// fix does), P + 500 s and V + 700 s are written instead, so that a write the kernel honours
// shows. Output is one line per case: what CNTVOFF_EL2 reads after the two counter writes and
// before its own, and each counter classified against ITS OWN saved value, the sleep and the
// host's counter. run.sh compares the output with expected.txt.
//
// What it does not measure: what a guest reads while it runs (these are reads through
// KVM_GET_ONE_REG), more than one vCPU, and any kernel but the one it runs on.
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

// What a counter read `got` is, given its own saved value `saved`, the value written to it
// `written` (the saved one unless the ioctl came first), the nested offset `x`, the host
// counter ticks slept `d` and the host counter now `host`. `tol` is 50 ms of ticks.
static const char *classify(uint64_t got, uint64_t saved, uint64_t written, uint64_t x,
			    uint64_t d, uint64_t host, uint64_t tol)
{
	int64_t e;

	e = (int64_t)(got - (saved + d));
	if (e > -(int64_t)tol && e < (int64_t)tol)
		return "its saved value plus the sleep";
	if (written != saved) {
		e = (int64_t)(got - (written + d));
		if (e > -(int64_t)tol && e < (int64_t)tol)
			return "the value written after the ioctl plus the sleep";
	}
	if (x) {
		e = (int64_t)(got - (saved - x + d));
		if (e > -(int64_t)tol && e < (int64_t)tol)
			return "its saved value plus the sleep, less CNTVOFF_EL2";
	}
	e = (int64_t)(got - host);
	if (e > -(int64_t)tol && e < (int64_t)tol)
		return "the host's counter";
	if (x) {
		e = (int64_t)(got - (host - x));
		if (e > -(int64_t)tol && e < (int64_t)tol)
			return "the host's counter less CNTVOFF_EL2";
	}
	return "OTHER";
}

// One restore-like replay on a new VM. `x`: the saved CNTVOFF_EL2 (EL2 vCPUs only).
// `ioctl_first`: KVM_ARM_SET_COUNTER_OFFSET before the writes, as the fork's fix does.
static void replay(int kvm, const char *label, int el2, uint64_t x, int ioctl_first)
{
	uint64_t freq = counter_freq();
	uint64_t tol = freq / 20;
	struct vm v = new_vm(kvm, el2);
	uint64_t t0 = host_counter();
	uint64_t saved_p = t0 + 10000 * freq; // far from the host's counter, either way
	uint64_t saved_v = saved_p - x;
	uint64_t write_p = saved_p + (ioctl_first ? 500 * freq : 0);
	uint64_t write_v = saved_v + (ioctl_first ? 700 * freq : 0);
	uint64_t p, c, t1, voff_before = 0, voff_after = 0;
	struct timespec ts = { .tv_sec = 1, .tv_nsec = 0 };

	if (ioctl_first) {
		struct kvm_arm_counter_offset off = { .counter_offset = t0 - saved_p };
		if (ioctl(v.vm, KVM_ARM_SET_COUNTER_OFFSET, &off) < 0)
			die("KVM_ARM_SET_COUNTER_OFFSET");
	}
	// KVM_GET_REG_LIST order: CNTPCT_EL0, CNTVCT_EL0, and far later CNTVOFF_EL2.
	set_reg(v.vcpu, REG_CNTPCT_EL0, write_p, "set CNTPCT_EL0");
	set_reg(v.vcpu, KVM_REG_ARM_TIMER_CNT, write_v, "set KVM_REG_ARM_TIMER_CNT");
	if (el2) {
		voff_before = get_reg(v.vcpu, REG_CNTVOFF_EL2, "get CNTVOFF_EL2");
		set_reg(v.vcpu, REG_CNTVOFF_EL2, x, "set CNTVOFF_EL2");
		voff_after = get_reg(v.vcpu, REG_CNTVOFF_EL2, "get CNTVOFF_EL2 again");
	}
	nanosleep(&ts, NULL);
	p = get_reg(v.vcpu, REG_CNTPCT_EL0, "get CNTPCT_EL0");
	c = get_reg(v.vcpu, KVM_REG_ARM_TIMER_CNT, "get KVM_REG_ARM_TIMER_CNT");
	t1 = host_counter();

	printf("%s:", label);
	if (el2)
		printf(" CNTVOFF_EL2 reads %s after the counter writes and %s after its own;",
		       voff_before == 0 ? "zero" : "NOT ZERO",
		       voff_after == x ? "the value written" : "SOMETHING ELSE");
	printf(" CNTPCT_EL0 reads %s; CNTVCT_EL0 reads %s\n",
	       classify(p, saved_p, write_p, x, t1 - t0, t1, tol),
	       classify(c, saved_v, write_v, x, t1 - t0, t1, tol));
	fprintf(stderr,
		"  %s: freq %" PRIu64 " host %" PRIu64 " -> %" PRIu64 " saved P %" PRIu64 " V %" PRIu64
		" written P %" PRIu64 " V %" PRIu64 " read P %" PRIu64 " V %" PRIu64 "\n",
		label, freq, t0, t1, saved_p, saved_v, write_p, write_v, p, c);
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

	replay(kvm, "plain vCPU, saved counters written", 0, 0, 0);
	replay(kvm, "plain vCPU, KVM_ARM_SET_COUNTER_OFFSET then other values written", 0, 0, 1);

	if (ioctl(kvm, KVM_CHECK_EXTENSION, KVM_CAP_ARM_EL2) <= 0) {
		fprintf(stderr,
			"counter-replay: no KVM_CAP_ARM_EL2: boot this host with kvm-arm.mode=nested "
			"on hardware with FEAT_NV2 to run the EL2 cases\n");
		return 3;
	}
	replay(kvm, "EL2 vCPU, CNTVOFF_EL2 = 0, saved counters written", 1, 0, 0);
	replay(kvm, "EL2 vCPU, CNTVOFF_EL2 = X, saved counters written", 1, x, 0);
	replay(kvm, "EL2 vCPU, CNTVOFF_EL2 = 0, KVM_ARM_SET_COUNTER_OFFSET then other values written", 1, 0, 1);
	replay(kvm, "EL2 vCPU, CNTVOFF_EL2 = X, KVM_ARM_SET_COUNTER_OFFSET then other values written", 1, x, 1);
	return 0;
}
