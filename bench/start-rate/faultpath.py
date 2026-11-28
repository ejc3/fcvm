#!/usr/bin/env python3
"""What a vCPU thread does inside a stage-2 fault: time per kernel function, and lock waits.

`perf` and bpftrace `profile` samples cannot show this path on arm64: KVM reports every
sample taken while a vCPU is loaded as guest state, and the kernel stack comes back empty.
This times the fault path's functions from entry to return instead.

  faultpath.py program [--one-in N]   print the bpftrace program for this host
  faultpath.py report FAULT.TXT       the table from that program's output

    sudo python3 bench/start-rate/faultpath.py program > $OUT.trace/fault.bt
    sudo BPFTRACE_MAX_MAP_KEYS=100000 bpftrace $OUT.trace/fault.bt > $OUT.trace/fault.txt &
    # wait for "attached" in fault.txt, run the cell, then SIGINT bpftrace
    python3 bench/start-rate/faultpath.py report $OUT.trace/fault.txt

The program traces:
  - entry and return of each function in FUNCTIONS, on the vCPU threads ("fc_vcpu 0") of
    the firecracker processes whose pid is a multiple of N (default 8: the probes cost
    time on every call, and one VMM in eight is enough calls). Times are inclusive: a
    function's time holds its callees'.
  - for the callees in INSIDE, the same again counting only the calls made while their
    caller was running on the thread, so a row can be read as a part of that caller.
  - `kvm:kvm_guest_fault` on guest memory for the same threads: the stage-2 faults the
    times are divided by.
  - `lock:contention_begin` / `contention_end` on every vCPU thread: time waited per stack.

It runs from attach to SIGINT, so the table covers everything the traced vCPUs did in that
time, not one clone's resume-to-ready window. `program` asks bpftrace which functions it
can probe on this kernel and leaves the others out, naming them in a comment.
"""
import argparse
import re
import subprocess
import sys

VCPU = 'comm == "fc_vcpu 0"'
# Firecracker's arm64 guests have RAM from 2 GiB up; an abort below it is a device access.
GUEST_RAM = "0x80000000"

FUNCTIONS = """
kvm_handle_guest_abort user_mem_abort __kvm_faultin_pfn hva_to_pfn handle_mm_fault
__handle_mm_fault do_wp_page wp_page_copy do_fault do_read_fault do_cow_fault
filemap_map_pages filemap_fault next_uptodate_folio ptep_clear_flush flush_tlb_page
__flush_tlb_range __mmu_notifier_invalidate_range_start
kvm_mmu_notifier_invalidate_range_start kvm_unmap_gfn_range kvm_pgtable_stage2_map
kvm_pgtable_stage2_relax_perms kvm_pgtable_stage2_unmap __mem_cgroup_charge
vma_alloc_folio_noprof folio_add_new_anon_rmap folio_add_file_rmap_ptes
folio_remove_rmap_ptes folio_wait_bit_common __folio_lock copy_mc_to_kernel copy_page
kvm_tlb_flush_vmid_ipa kvm_flush_remote_tlbs smp_call_function_many_cond do_numa_page
lru_add_drain folio_add_lru __pte_offset_map_lock mark_page_dirty_in_slot
kvm_release_faultin_page
""".split()

# caller -> callees also counted for the calls made inside that caller.
INSIDE = {
    "wp_page_copy": ["kvm_pgtable_stage2_unmap", "kvm_unmap_gfn_range", "ptep_clear_flush",
                     "__mmu_notifier_invalidate_range_start", "__flush_tlb_range"],
}


def probeable(listing):
    """The functions of FUNCTIONS in `listing`, the output of `bpftrace -l 'kprobe:*'`: the
    ones this kernel lets bpftrace attach a kprobe to."""
    listed = {line.strip() for line in listing.splitlines()}
    return {name for name in FUNCTIONS if f"kprobe:{name}" in listed}


def program(available, one_in=8):
    """The bpftrace program, probing the functions in `available`."""
    traced = f"{VCPU} && pid % {one_in} == 0"
    lines = [
        'BEGIN { printf("attached\\n"); }',
        f"tracepoint:lock:contention_begin /{VCPU}/ {{ @cb[tid] = nsecs; @cs[tid] = kstack(7); }}",
        "tracepoint:lock:contention_end /@cb[tid]/ { @lock_ns[@cs[tid]] = sum(nsecs - @cb[tid]); "
        "@lock_n[@cs[tid]] = count(); delete(@cb[tid]); delete(@cs[tid]); }",
        f"tracepoint:kvm:kvm_guest_fault /{traced} && args->ipa >= {GUEST_RAM}/ {{ @faults = count(); }}",
    ]
    callers = {callee: caller for caller, callees in INSIDE.items() for callee in callees}
    for name in FUNCTIONS:
        if name not in available:
            lines.append(f"// not probeable: {name}")
            continue
        entry = f"@e_{name}[tid] = nsecs;"
        # One reading of the clock per call, so a row inside a caller never exceeds the
        # function's own row.
        done = f'$d = nsecs - @e_{name}[tid]; @ns["{name}"] = sum($d); @n["{name}"] = count();'
        caller = callers.get(name)
        if caller in available:
            # The caller's entry stamp doubles as "this thread is inside the caller".
            done += (f' if (@e_{caller}[tid]) {{ @ns_in["{name}", "{caller}"] = sum($d); '
                     f'@n_in["{name}", "{caller}"] = count(); }}')
        lines.append(f"kprobe:{name} /{traced}/ {{ {entry} }}")
        lines.append(f"kretprobe:{name} /@e_{name}[tid]/ {{ {done} delete(@e_{name}[tid]); }}")
    return "\n".join(lines) + "\n"


def parse(text):
    """bpftrace's map dump -> faults, per-function and per-(callee, caller) time and calls,
    and the lock waits by stack."""
    faults = re.search(r"^@faults: (\d+)$", text, re.M)
    out = {"faults": int(faults.group(1)) if faults else 0, "ns": {}, "n": {}, "ns_in": {}, "n_in": {},
           "locks": []}
    for kind in ("ns", "n"):
        for m in re.finditer(rf"^@{kind}\[(\w+)\]: (\d+)$", text, re.M):
            out[kind][m.group(1)] = int(m.group(2))
        for m in re.finditer(rf"^@{kind}_in\[(\w+), (\w+)\]: (\d+)$", text, re.M):
            out[f"{kind}_in"][(m.group(1), m.group(2))] = int(m.group(3))

    def stacks(name):
        found = {}
        for m in re.finditer(rf"^@{name}\[\n(.*?)\n\]: (\d+)$", text, re.M | re.S):
            frames = tuple(frame.strip().split("+")[0] for frame in m.group(1).split("\n"))
            found[frames] = found.get(frames, 0) + int(m.group(2))
        return found
    waited, count = stacks("lock_ns"), stacks("lock_n")
    out["locks"] = sorted(((ns, count.get(frames, 0), frames) for frames, ns in waited.items()), reverse=True)
    return out


def rows(parsed):
    """(label, calls, total s, us per call, us per stage-2 fault), longest first."""
    faults = max(1, parsed["faults"])
    table = [(name, parsed["n"].get(name, 0), ns) for name, ns in parsed["ns"].items()]
    table += [(f"{callee} inside {caller}", parsed["n_in"].get((callee, caller), 0), ns)
              for (callee, caller), ns in parsed["ns_in"].items()]
    return [(label, calls, ns / 1e9, ns / 1e3 / max(1, calls), ns / 1e3 / faults)
            for label, calls, ns in sorted(table, key=lambda row: -row[2])]


def report(text, top=10):
    parsed = parse(text)
    if not parsed["faults"]:
        sys.exit("no @faults count in the output: bpftrace prints its maps when it is stopped with SIGINT, "
                 "and the traced VMMs are the ones whose pid is a multiple of --one-in")
    lines = [f"stage-2 faults on guest memory of the traced vCPU threads: {parsed['faults']}",
             "inclusive time per function, from attach to SIGINT:",
             f"  {'function':58} {'calls':>10} {'total s':>9} {'us/call':>9} {'us/fault':>9}"]
    for label, calls, total, per_call, per_fault in rows(parsed):
        lines.append(f"  {label:58} {calls:10d} {total:9.2f} {per_call:9.2f} {per_fault:9.2f}")
    waited = sum(ns for ns, _, _ in parsed["locks"])
    lines.append(f"lock contention of every vCPU thread: {waited / 1e9:.1f} s waited in "
                 f"{sum(n for _, n, _ in parsed['locks'])} waits; longest stacks (s, waits):")
    for ns, count, frames in parsed["locks"][:top]:
        lines.append(f"  {ns / 1e9:8.2f} {count:9d}  {' < '.join(frames)}")
    return "\n".join(lines)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="command", required=True)
    p = sub.add_parser("program", help="print the bpftrace program for this host")
    p.add_argument("--one-in", type=int, default=8, help="trace the VMMs whose pid is a multiple of this")
    p.add_argument("--bpftrace", default="bpftrace")
    r = sub.add_parser("report", help="the table from the program's output")
    r.add_argument("output")
    r.add_argument("--top", type=int, default=10, help="lock stacks to print")
    a = ap.parse_args(argv)
    if a.command == "program":
        if a.one_in < 1:
            sys.exit("--one-in must be 1 or more")
        listing = subprocess.run([a.bpftrace, "-l", "kprobe:*"], capture_output=True, text=True)
        available = probeable(listing.stdout)
        if "user_mem_abort" not in available:
            sys.exit(f"`{a.bpftrace} -l` lists no kprobe:user_mem_abort (run as root, on an arm64 KVM "
                     f"host): {listing.stderr.strip()[:200]}")
        sys.stdout.write(program(available, a.one_in))
    else:
        with open(a.output, errors="replace") as f:
            print(report(f.read(), a.top))


if __name__ == "__main__":
    main()
