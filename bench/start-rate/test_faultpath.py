import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import faultpath  # noqa: E402

# bpftrace's dump at SIGINT, cut down: the per-thread scratch maps it also prints, two
# lock stacks (each in both maps, one of them split over two keys that differ only in the
# offsets), and the function maps.
OUTPUT = """\
Attaching 68 probes...
attached


@cb[1234]: 5
@e_user_mem_abort[1234]: 6
@faults: 1000
@lock_n[
    __traceiter_contention_begin+76
    __traceiter_contention_begin+76
    queued_spin_lock_slowpath+564
    _raw_spin_lock_irqsave+164
    folio_lruvec_lock_irqsave+128
    __page_cache_release.part.0+112
    folios_put_refs+548
]: 600
@lock_n[
    trace_contention_begin+276
    trace_contention_begin+276
    __mutex_lock.constprop.0+2032
    __mutex_lock_slowpath+32
    mutex_lock+112
    rtnl_lock+36
    tun_chr_close+36
]: 3
@lock_n[
    trace_contention_begin+276
    trace_contention_begin+276
    __mutex_lock.constprop.0+336
    __mutex_lock_slowpath+32
    mutex_lock+112
    rtnl_lock+36
    tun_chr_close+36
]: 1
@lock_ns[
    trace_contention_begin+276
    trace_contention_begin+276
    __mutex_lock.constprop.0+2032
    __mutex_lock_slowpath+32
    mutex_lock+112
    rtnl_lock+36
    tun_chr_close+36
]: 3000000000
@lock_ns[
    trace_contention_begin+276
    trace_contention_begin+276
    __mutex_lock.constprop.0+336
    __mutex_lock_slowpath+32
    mutex_lock+112
    rtnl_lock+36
    tun_chr_close+36
]: 1000000000
@lock_ns[
    __traceiter_contention_begin+76
    __traceiter_contention_begin+76
    queued_spin_lock_slowpath+564
    _raw_spin_lock_irqsave+164
    folio_lruvec_lock_irqsave+128
    __page_cache_release.part.0+112
    folios_put_refs+548
]: 120000000000
@n[wp_page_copy]: 100
@n[kvm_pgtable_stage2_unmap]: 130
@n[user_mem_abort]: 1000
@n_in[kvm_pgtable_stage2_unmap, wp_page_copy]: 100
@ns[kvm_pgtable_stage2_unmap]: 26000000
@ns[wp_page_copy]: 40000000
@ns[user_mem_abort]: 50000000
@ns_in[kvm_pgtable_stage2_unmap, wp_page_copy]: 21000000
"""


class Program(unittest.TestCase):
    def test_only_probeable_functions_are_probed_and_the_rest_are_named(self):
        listing = "kprobe:user_mem_abort\nkprobe:wp_page_copy\nkprobe:ptep_clear_flush\nkprobe:wp_page_copy_other\n"
        available = faultpath.probeable(listing)
        self.assertEqual(available, {"user_mem_abort", "wp_page_copy", "ptep_clear_flush"})
        text = faultpath.program(available, one_in=8)
        self.assertEqual(text.count("\nkprobe:"), 3)
        self.assertEqual(text.count("\nkretprobe:"), 3)
        self.assertIn("// not probeable: kvm_pgtable_stage2_unmap", text)
        self.assertNotIn("kprobe:kvm_pgtable_stage2_unmap", text)
        # One VMM in eight for the function probes and the fault count; every vCPU thread
        # for the lock waits.
        self.assertIn('kprobe:user_mem_abort /comm == "fc_vcpu 0" && pid % 8 == 0/', text)
        self.assertIn('tracepoint:kvm:kvm_guest_fault /comm == "fc_vcpu 0" && pid % 8 == 0 && args->ipa >= 0x80000000/', text)
        self.assertIn('tracepoint:lock:contention_begin /comm == "fc_vcpu 0"/', text)
        self.assertIn("pid % 3 == 0", faultpath.program(available, one_in=3))

    def test_a_callee_is_also_counted_inside_its_caller_when_both_are_probed(self):
        text = faultpath.program({"wp_page_copy", "ptep_clear_flush", "kvm_pgtable_stage2_unmap"})
        flush = [line for line in text.splitlines() if line.startswith("kretprobe:ptep_clear_flush")][0]
        # The call's time is read once and goes to both rows.
        self.assertIn('$d = nsecs - @e_ptep_clear_flush[tid]; @ns["ptep_clear_flush"] = sum($d);', flush)
        self.assertIn('if (@e_wp_page_copy[tid]) { @ns_in["ptep_clear_flush", "wp_page_copy"] = '
                      'sum($d); @n_in["ptep_clear_flush", "wp_page_copy"] = count(); }', flush)
        self.assertEqual(flush.count("nsecs"), 1)
        self.assertTrue(flush.endswith("delete(@e_ptep_clear_flush[tid]); }"))
        # Without the caller's probe there is no entry stamp to test.
        alone = faultpath.program({"ptep_clear_flush"})
        self.assertNotIn("@ns_in", alone)

    def test_every_listed_function_is_one_word(self):
        self.assertEqual(len(faultpath.FUNCTIONS), len(set(faultpath.FUNCTIONS)))
        for name in faultpath.FUNCTIONS + [c for callees in faultpath.INSIDE.values() for c in callees]:
            self.assertRegex(name, r"^\w+$")
            self.assertIn(name, faultpath.FUNCTIONS)


class Report(unittest.TestCase):
    def test_times_are_per_call_and_per_fault_and_a_callee_has_a_row_inside_its_caller(self):
        parsed = faultpath.parse(OUTPUT)
        self.assertEqual(parsed["faults"], 1000)
        self.assertNotIn("1234", str(parsed["n"]))
        table = {label: (calls, round(per_call, 3), round(per_fault, 3))
                 for label, calls, _, per_call, per_fault in faultpath.rows(parsed)}
        self.assertEqual(table, {
            "user_mem_abort": (1000, 50.0, 50.0),
            "wp_page_copy": (100, 400.0, 40.0),
            # All 130 calls, 30 of them from another caller ...
            "kvm_pgtable_stage2_unmap": (130, 200.0, 26.0),
            # ... and the 100 made inside wp_page_copy.
            "kvm_pgtable_stage2_unmap inside wp_page_copy": (100, 210.0, 21.0),
        })
        self.assertEqual([row[0] for row in faultpath.rows(parsed)][0], "user_mem_abort")

    def test_lock_waits_are_summed_per_stack_of_function_names(self):
        parsed = faultpath.parse(OUTPUT)
        self.assertEqual([(ns, count, frames[-2:]) for ns, count, frames in parsed["locks"]], [
            (120_000_000_000, 600, ("__page_cache_release.part.0", "folios_put_refs")),
            # Two keys that differ only in an offset are one stack.
            (4_000_000_000, 4, ("rtnl_lock", "tun_chr_close")),
        ])
        text = faultpath.report(OUTPUT, top=1)
        self.assertIn("stage-2 faults on guest memory of the traced vCPU threads: 1000", text)
        self.assertIn("lock contention of every vCPU thread: 124.0 s waited in 604 waits", text)
        self.assertIn("folio_lruvec_lock_irqsave < __page_cache_release.part.0 < folios_put_refs", text)
        self.assertNotIn("tun_chr_close", text)

    def test_an_output_without_the_fault_count_is_refused(self):
        with self.assertRaises(SystemExit) as stop:
            faultpath.report("Attaching 68 probes...\nattached\n")
        self.assertIn("no @faults count", str(stop.exception))


if __name__ == "__main__":
    unittest.main()
