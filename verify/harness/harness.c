/*
 * Drives the kernel's own functions, cut verbatim from a pinned release (build/<variant>/cut.c),
 * with the operations of the Lean model, and prints the vCPU's state after each one. run.sh
 * compares the output with what the model computed.
 *
 * Input, one operation a line (i is the vCPU, 0 or 1). "cut" names the kernel function the
 * operation calls; "stand-in" marks state the harness sets by hand for code that is not cut.
 *
 *   set i MP PV APFH   stand-in: a vCPU in this state, nothing queued, slot free
 *   fresh i            stand-in for kvm_arch_vcpu_create: vCPU 0 runnable, any other UNINITIALIZED
 *   pvapf i            stand-in: the guest enabled async page faults long ago (MSR values only)
 *   env i IF IRQ NMI   stand-in: RFLAGS.IF, another interrupt pending in the IRR, an NMI requested
 *   slot i V           stand-in: the guest's page-ready slot, as a snapshot's memory holds it
 *   irr i V            stand-in for KVM_SET_LAPIC: the page-ready vector pending in the IRR
 *   queue i KIND       stand-in for async_pf_execute putting a finished fault on the done list
 *                      (virt/kvm/async_pf.c), KIND pv (page-not-present was injected) or synth;
 *                      then cut: kvm_arch_async_page_present_queued
 *   enable i           cut: kvm_pv_enable_async_pf, as KVM_SET_MSRS(MSR_KVM_ASYNC_PF_EN) calls it;
 *                      it calls the cut kvm_async_pf_wakeup_all
 *   enter i            cut: kvm_check_async_pf_completion, as vcpu_enter_guest calls it for
 *                      KVM_REQ_APF_READY; it calls the cut kvm_arch_can_dequeue_async_page_present
 *                      and kvm_arch_async_page_present, which calls the cut apf_put_user_ready
 *                      and kvm_apic_set_irq
 *   ack i              stand-in for the guest's page-ready handler: slot cleared, vector taken
 *   halt i ST          cut: __kvm_emulate_halt(vcpu, ST, ...); kvm_vcpu_has_events is cut too
 *   block i            cut: vcpu_block
 *   kick i             cut: __apic_accept_irq(..., APIC_DM_REMRD, ...)
 *   getmp i            cut: KVM_GET_MP_STATE
 *   setmp i MP         cut: KVM_SET_MP_STATE
 *   apfhalt i          stand-in for KVM_REQ_APF_HALT in vcpu_enter_guest: apf.halted = true
 *   show i             print the state
 *
 * MP is runnable, uninitialized, halted, initReceived or apResetHold.
 * Output: "MP PV APFH done=D slot=S pr=P" (D: the done list is not empty; S: the slot is
 * occupied; P: the page-ready vector is pending), then " blocked" or " ran" for block. getmp
 * prints the answer alone.
 */
#include "stubs.h"

struct knobs K;

#include CUT

#define APF_VECTOR 0xf3

static struct kvm the_kvm;
static struct kvm_run runs[2];
static struct kvm_lapic apics[2];
static struct kvm_vcpu vcpus[2];
static bool initialised[2];

static const char *mp_name(int mp)
{
	switch (mp) {
	case KVM_MP_STATE_RUNNABLE: return "runnable";
	case KVM_MP_STATE_UNINITIALIZED: return "uninitialized";
	case KVM_MP_STATE_HALTED: return "halted";
	case KVM_MP_STATE_INIT_RECEIVED: return "initReceived";
	case KVM_MP_STATE_AP_RESET_HOLD: return "apResetHold";
	default: return "?";
	}
}

static int mp_value(const char *s)
{
	if (!strcmp(s, "runnable")) return KVM_MP_STATE_RUNNABLE;
	if (!strcmp(s, "uninitialized")) return KVM_MP_STATE_UNINITIALIZED;
	if (!strcmp(s, "halted")) return KVM_MP_STATE_HALTED;
	if (!strcmp(s, "initReceived")) return KVM_MP_STATE_INIT_RECEIVED;
	if (!strcmp(s, "apResetHold")) return KVM_MP_STATE_AP_RESET_HOLD;
	fprintf(stderr, "harness: unknown mp_state %s\n", s);
	return -1;
}

static void show(struct kvm_vcpu *v, const char *extra)
{
	printf("%s %d %d done=%d slot=%d pr=%d%s\n", mp_name(v->arch.mp_state),
	       v->arch.pv.pv_unhalted, v->arch.apf.halted, !list_empty(&v->async_pf.done),
	       v->arch.apf.data.token != 0, v->hx.irr, extra);
}

static void reset(struct kvm_vcpu *v, int i)
{
	if (initialised[i]) {
		while (!list_empty(&v->async_pf.done)) {
			struct kvm_async_pf *work =
				list_first_entry(&v->async_pf.done, struct kvm_async_pf, link);

			list_del(&work->link);
			free(work);
		}
	}
	memset(v, 0, sizeof(*v));
	v->vcpu_id = i;
	v->kvm = &the_kvm;
	v->run = &runs[i];
	v->arch.apic = &apics[i];
	apics[i].vcpu = v;
	INIT_LIST_HEAD(&v->async_pf.done);
	INIT_LIST_HEAD(&v->async_pf.queue);
	v->arch.mp_state = KVM_MP_STATE_RUNNABLE;
	initialised[i] = true;
}

int main(void)
{
	char line[256], op[32], a1[32];
	int i, n, b, c, d;

	reset(&vcpus[0], 0);
	reset(&vcpus[1], 1);
	while (fgets(line, sizeof(line), stdin)) {
		struct kvm_vcpu *v;

		a1[0] = 0;
		n = sscanf(line, "%31s %d %31s", op, &i, a1);
		if (n < 2 || i < 0 || i > 1) {
			fprintf(stderr, "harness: bad line: %s", line);
			return 2;
		}
		v = &vcpus[i];
		memset(&K, 0, sizeof(K));
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
			if (i != 0)
				v->arch.mp_state = KVM_MP_STATE_UNINITIALIZED;
			show(v, "");
		} else if (!strcmp(op, "pvapf")) {
			v->arch.apf.msr_en_val = KVM_ASYNC_PF_ENABLED | KVM_ASYNC_PF_DELIVERY_AS_INT;
			v->arch.apf.vec = APF_VECTOR;
			show(v, "");
		} else if (!strcmp(op, "env")) {
			if (sscanf(line, "%*s %*d %d %d %d", &b, &c, &d) != 3)
				return 2;
			v->hx.if_on = b;
			v->hx.other_irq = c;
			v->hx.nmi = d;
			show(v, "");
		} else if (!strcmp(op, "slot")) {
			v->arch.apf.data.token = atoi(a1) ? 1 : 0;
			show(v, "");
		} else if (!strcmp(op, "irr")) {
			v->hx.irr = atoi(a1) != 0;
			show(v, "");
		} else if (!strcmp(op, "queue")) {
			struct kvm_async_pf *work = calloc(1, sizeof(*work));
			bool first = list_empty(&v->async_pf.done);

			if (!work || (strcmp(a1, "pv") && strcmp(a1, "synth")))
				return 2;
			work->vcpu = v;
			work->arch.token = 1;
			work->notpresent_injected = !strcmp(a1, "pv");
			work->work.func = work;
			INIT_LIST_HEAD(&work->queue);
			list_add_tail(&work->link, &v->async_pf.done);
			v->async_pf.queued++;
			if (first)
				kvm_arch_async_page_present_queued(v);
			show(v, "");
		} else if (!strcmp(op, "enable")) {
			v->arch.apf.vec = APF_VECTOR;
			if (kvm_pv_enable_async_pf(v, 0x1000 | KVM_ASYNC_PF_ENABLED |
						      KVM_ASYNC_PF_DELIVERY_AS_INT))
				return 2;
			show(v, "");
		} else if (!strcmp(op, "enter")) {
			kvm_check_async_pf_completion(v);
			show(v, "");
		} else if (!strcmp(op, "ack")) {
			v->arch.apf.data.token = 0;
			v->hx.irr = false;
			v->arch.apf.pageready_pending = false;
			show(v, "");
		} else if (!strcmp(op, "halt")) {
			if (mp_value(a1) < 0)
				return 2;
			__kvm_emulate_halt(v, mp_value(a1), KVM_EXIT_HLT);
			show(v, "");
		} else if (!strcmp(op, "block")) {
			vcpu_block(v);
			show(v, K.did_block ? " blocked" : " ran");
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
		} else if (!strcmp(op, "apfhalt")) {
			v->arch.apf.halted = true;
			show(v, "");
		} else if (!strcmp(op, "show")) {
			show(v, "");
		} else {
			fprintf(stderr, "harness: unknown operation: %s", line);
			return 2;
		}
	}
	return 0;
}
