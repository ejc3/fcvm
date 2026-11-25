/*
 * Drives the kernel's own functions, cut verbatim from a pinned release, with the operations of
 * the Lean model, and prints the vCPU state after each one. run.sh compares the output with what
 * the model computed.
 *
 * Input, one operation a line (i is the vCPU, 0 or 1):
 *   set i MP PV APF       load a state: mp_state, pv_unhalted, apf.halted
 *   fresh i               a new vCPU, as kvm_arch_vcpu_create leaves a boot processor
 *   halt i EV ST          __kvm_emulate_halt(vcpu, ST, ...) with events pending or not
 *   block i EV            vcpu_block() with events pending or not
 *   present i             kvm_arch_async_page_present()
 *   kick i                __apic_accept_irq(..., APIC_DM_REMRD, ...)
 *   getmp i               KVM_GET_MP_STATE
 *   setmp i MP            KVM_SET_MP_STATE
 *   apfhalt i             apf.halted = true (KVM_REQ_APF_HALT in vcpu_enter_guest: not cut)
 *   show i                print the state
 * MP is runnable, halted, initReceived or apResetHold.
 * Output: "MP PV APF" and, for block, " blocked" or " ran"; for getmp, the answer alone.
 */
#include "stubs.h"

struct knobs K;

#include CUT

static struct kvm the_kvm;
static struct kvm_run runs[2];
static struct kvm_lapic apics[2];
static struct kvm_vcpu vcpus[2];

static const char *mp_name(int mp)
{
	switch (mp) {
	case KVM_MP_STATE_RUNNABLE: return "runnable";
	case KVM_MP_STATE_HALTED: return "halted";
	case KVM_MP_STATE_INIT_RECEIVED: return "initReceived";
	case KVM_MP_STATE_AP_RESET_HOLD: return "apResetHold";
	default: return "?";
	}
}

static int mp_value(const char *s)
{
	if (!strcmp(s, "runnable")) return KVM_MP_STATE_RUNNABLE;
	if (!strcmp(s, "halted")) return KVM_MP_STATE_HALTED;
	if (!strcmp(s, "initReceived")) return KVM_MP_STATE_INIT_RECEIVED;
	if (!strcmp(s, "apResetHold")) return KVM_MP_STATE_AP_RESET_HOLD;
	fprintf(stderr, "harness: unknown mp_state %s\n", s);
	return -1;
}

static void show(struct kvm_vcpu *v, const char *extra)
{
	printf("%s %d %d%s\n", mp_name(v->arch.mp_state), v->arch.pv.pv_unhalted,
	       v->arch.apf.halted, extra);
}

static void reset(struct kvm_vcpu *v, int i)
{
	memset(v, 0, sizeof(*v));
	v->vcpu_id = i;
	v->kvm = &the_kvm;
	v->run = &runs[i];
	v->arch.apic = &apics[i];
	apics[i].vcpu = v;
	v->arch.mp_state = KVM_MP_STATE_RUNNABLE;
}

int main(void)
{
	char line[256], op[32], a1[32], a2[32];
	int i, n, b, c;

	reset(&vcpus[0], 0);
	reset(&vcpus[1], 1);
	while (fgets(line, sizeof(line), stdin)) {
		struct kvm_vcpu *v;

		a1[0] = a2[0] = 0;
		n = sscanf(line, "%31s %d %31s %31s", op, &i, a1, a2);
		if (n < 2 || i < 0 || i > 1) {
			fprintf(stderr, "harness: bad line: %s", line);
			return 2;
		}
		v = &vcpus[i];
		memset(&K, 0, sizeof(K));
		v->async_pf.done.nonempty = 0;
		if (!strcmp(op, "set")) {
			if (sscanf(line, "%*s %*d %31s %d %d", a1, &b, &c) != 3 || mp_value(a1) < 0)
				return 2;
			reset(v, i);
			v->arch.mp_state = mp_value(a1);
			v->arch.pv.pv_unhalted = b;
			v->arch.apf.halted = c;
			show(v, "");
		} else if (!strcmp(op, "fresh")) {
			reset(v, i);
			show(v, "");
		} else if (!strcmp(op, "halt")) {
			v->async_pf.done.nonempty = a1[0] == '1';
			if (mp_value(a2) < 0)
				return 2;
			__kvm_emulate_halt(v, mp_value(a2), KVM_EXIT_HLT);
			show(v, "");
		} else if (!strcmp(op, "block")) {
			v->async_pf.done.nonempty = a1[0] == '1';
			vcpu_block(v);
			show(v, K.did_block ? " blocked" : " ran");
		} else if (!strcmp(op, "present")) {
			struct kvm_async_pf work = { 0 };

			kvm_arch_async_page_present(v, &work);
			show(v, "");
		} else if (!strcmp(op, "kick")) {
			__apic_accept_irq(v->arch.apic, APIC_DM_REMRD, 0, 1, 0, NULL);
			show(v, "");
		} else if (!strcmp(op, "getmp")) {
			struct kvm_mp_state mp;

			if (kvm_arch_vcpu_ioctl_get_mpstate(v, &mp))
				return 2;
			printf("%s\n", mp_name(mp.mp_state));
		} else if (!strcmp(op, "setmp")) {
			struct kvm_mp_state mp;

			if (mp_value(a1) < 0)
				return 2;
			mp.mp_state = mp_value(a1);
			if (kvm_arch_vcpu_ioctl_set_mpstate(v, &mp))
				return 2;
			show(v, "");
		} else if (!strcmp(op, "show")) {
			show(v, "");
		} else if (!strcmp(op, "apfhalt")) {
			v->arch.apf.halted = true;
			show(v, "");
		} else {
			fprintf(stderr, "harness: unknown operation: %s", line);
			return 2;
		}
	}
	return 0;
}
