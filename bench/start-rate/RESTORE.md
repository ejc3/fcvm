# Restore: where the time goes between resume and fc-agent ready

One session on one c8g.metal-48xl (192 cores, kernel 7.0.0-1013-aws), fcvm at 4f67a0fbd (built from
this branch at 320b5a55, which differs from it under bench/start-rate only), measured with the tools
at 3d43a1c52, store on btrfs in RAM. Guest: nginx:alpine, 1 vCPU, 256 MiB, rootless network,
snapshot taken when the baseline first reported healthy. Five snapshots: m1, m4 and m7, each with
its own cells; m2, whose clones run on a kcompactd pass the snapshot caught (last section); and fp,
used for the fault-path function trace alone. Snapshots that carried a pass were taken and not
measured further: m3, m5 and m6, and two before fp. Every cell is `make bench-start-rate-run`;
traced cells also ran `vcpustat.py` and `s2fault.bt`, and the function trace ran `faultpath.py`
(README, "Restore timeline" and "Fault path"). UFFD cells ran after the memory server had recorded a
working set.

How to read the numbers:

- A value is the p50 over the cell's clones unless the row says otherwise. Three values in a row
  are m1, m4, m7.
- "Resume to ready" is between two lines of the clone's fcvm process. Steps between fc-agent's
  console lines are the harness's receive times of those lines, which lag the guest by several
  ms (the acknowledgement, which arrives over vsock, is often logged before the console line
  the guest wrote ahead of it). fc-agent's phase record is the guest's own clock.
- Counts and fault times are the part of `s2fault.bt`'s buckets inside the window. The buckets
  are 10 ms wide for the first second and 100 ms wide after it, and they count from the vCPU's
  first stage-2 fault, not from the resume line. Each clone's buckets are shifted by the time
  between the two (the "resume stamp to first fault" rows), and a bucket the window starts or
  ends in counts in proportion. "Fault to guest entry" is measured per fault, from
  `kvm:kvm_guest_fault` on guest memory to the vCPU's next `kvm:kvm_entry`.
- On-CPU and waiting time run from the later of the resume stamp and the first fault, where the
  tracer read the vCPU thread's counters, to the ready stamp, read off the line between the two
  records around it: the first-fault record and the sampler's samples, which come up to 11 ms
  apart at C=1 and about 40 ms apart in the ramp.
- The tracers slow a clone: untraced, the same three snapshots take 39.3, 42.8 and 36.1 ms from
  resume to ready on File at C=1 (10 clones each).

## One clone at a time (C=1, N=30, traced)

| | File | UFFD |
|---|---|---|
| resume to ready (ms) | 46.7, 50.1, 43.5 | 29.6, 31.7, 26.9 |
| resume stamp to the vCPU's first stage-2 fault (ms) | -0.01, -0.01, -0.01 | -0.01, -0.01, -0.01 |
| vCPU thread on a CPU (ms) | 46.1, 49.5, 43.0 | 28.9, 30.8, 26.2 |
| vCPU thread runnable, waiting for a CPU (ms) | 0.0 | 0.0 |
| vCPU thread in a userfault (ms) | 0.0 | 0.2, 0.1, 0.1 |
| stage-2 faults on guest memory in the window | 10,638, 12,139, 9,669 | 7,576, 9,236, 7,052 |
| fault to guest entry, all of the window's faults (ms) | 22.2, 25.8, 20.8 | 6.7, 8.7, 6.4 |
| fault to guest entry, per fault (µs) | 2.1, 2.1, 2.1 | 0.9, 0.9, 0.9 |
| device aborts in the window (console, virtio) | 2,737, 2,789, 2,970 | 2,917, 2,703, 2,610 |
| ready to healthy (ms) | 7.2, 7.1, 6.9 | 7.3, 7.4, 7.0 |

All 30 clones of each cell are in every row. The vCPU's first fault comes 0.01 to 0.04 ms before
fcvm stamps the resume, in every clone of the six cells. The vCPU is on a CPU for the whole window:
the p50 over a cell's clones of the window less on-CPU, waiting and userfault time is 0.2 to 0.5 ms
(the differences of the table's own p50s are 0.5 to 0.8 ms). On File, fault-to-entry time is 47 to
52% of the cell's summed windows: the host handling 9,669 to 12,139 stage-2 faults, which map the
guest pages the restore touches into an empty stage-2 table. The rest is guest code and under three
thousand exits to the VMM for device accesses. On UFFD the recorded working set is already in the
clone's memory and a fault costs 0.9 µs.

Steps on File, console-line times (ms): resume to `detected restore-epoch` 8.8, 22.2, 14.9; to
tcp-cleanup begin 24.1, 15.6, 14.3; tcp-cleanup 3.5, 3.1, 3.5; to the clock step 3.8, 4.7, 4.9;
to `snapshot network published` 1.4, 3.1, 1.3. The gate, from fc-agent's phase record:
`tcp_verify_ms` 1.98, 0.23, 0.24; `tcp_gate_arm_ms` 0.24, 0.24, 0.25;
`tcp_gate_egress_commit_ms` 0.16, 1.79, 2.34; `tcp_reopen_ms` 0.08, 0.07, 0.08; the whole
`tcp_cleanup_ms` 3.43, 3.12, 3.65. All 30 restores of every cell went over nfnetlink
(`tcp_gate_netlink`), none through an iptables process.

## After ready

Per clone, the 2 s that follow its first healthy check (the clones still alive then: 9 or 10 of a
File cell's 30, 5 to 7 of a UFFD cell's):

| | File | UFFD |
|---|---|---|
| vCPU thread on a CPU (ms) | 239, 232, 218 | 245, 227, 212 |
| VMM main thread on a CPU (ms) | 23.7, 26.5, 24.6 | 23.1, 24.7, 23.6 |

That is about five times the restore's window on File and seven to eight times on UFFD. fc-agent's
restore-epoch watcher reads the vsock transport reset after it has dispatched the restore (29 or 30
of 30 clones in every cell), and the reset opens a one-second window of metadata fetches 2 ms apart.
The chronyd start (`chronyd restarted` 39 to 42 ms after ready on File) and the journald restart (73
to 78 ms after) run in the same two seconds.

## Ramp: 128 starting at once, to 1000 alive

Untraced cells:

| | File | UFFD |
|---|---|---|
| clones healthy per second, whole ramp | 31.9, 30.1, 36.0 | 22.3, 21.9, 30.7 |
| resume to ready, p50 (ms) | 2,604, 2,260, 1,384 | 125, 211, 73 |
| resume to ready, p99 (ms) | 3,765, 4,305, 3,548 | 1,802, 1,844, 1,192 |
| CPU-seconds per start, all processes / Firecracker (pidstat) | 4.82 / 4.63, 5.08 / 4.89, 4.69 / 4.47 | 6.24 / 5.78, 7.33 / 6.82, 4.08 / 3.60 |
| cores busy over the ramp, of 192 (pidstat CPU-seconds / ramp seconds) | 154, 153, 169 | 139, 161, 125 |

Traced cells (resume to ready window, per clone):

| | File | UFFD |
|---|---|---|
| clones in the rows above the last, of 1000 | 1000, 1000, 1000 | 1000, 1000, 1000 |
| window (ms) | 2,804, 2,352, 1,413 | 154, 316, 86 |
| resume stamp to the vCPU's first stage-2 fault, p50 / p99 (ms) | -0.3 / 3.1, -0.1 / 3.2, -0.1 / 4.0 | -0.1 / 0.8, -0.2 / 0.4, -0.1 / 0.2 |
| vCPU thread on a CPU (ms) | 2,622, 2,104, 1,311 | 112, 218, 72 |
| waiting for a CPU, p50 / p90 (ms) | 127 / 365, 100 / 662, 70 / 404 | 16 / 166, 25 / 200, 1 / 42 |
| in a userfault, p50 / p90 (ms) | 0 / 0 | 20 / 176, 57 / 361, 4 / 94 |
| stage-2 faults on guest memory in the window | 52,830, 37,440, 16,328 | 8,503, 9,984, 7,423 |
| fault to guest entry, all of the window's faults (ms) | 2,601, 2,234, 1,346 | 74, 220, 44 |
| fault to guest entry, per fault (µs) | 45.6, 53.1, 78.5 | 7.8, 18.6, 5.6 |
| on a CPU, mean over the cell (ms): in the guest / outside it | 131 / 2,220, 81 / 1,983, 33 / 1,333 | 67 / 76, 77 / 175, 30 / 54 |
| vCPU thread on a CPU in the 2 s after healthy (ms); clones in the row | 1,232, 1,514, 1,703; 951, 966, 964 | 1,596, 1,466, 976; 996, 997, 977 |

- Every clone of every cell is in every row but the last, which holds the clones the sampler was
  still following 2 s after their healthy line. In the UFFD cells 20, 7 and 101 clones have no
  sample inside their window, and their on-CPU and waiting time is read off the line from the
  first-fault record to the first sample after ready.
- The first fault is within a few ms of the resume stamp for all but a few clones: the widest
  offsets in the six cells are 32.6 ms before the stamp (fcvm stamped the resume late) and
  12.3 ms after it.
- "In the guest" is the kernel's tick accounting, which `/proc` gives in whole 10 ms against an
  exact start: it reads about 5 ms low per clone, and "outside it" as much high.
- A userfault's time runs until the vCPU thread is back on a CPU after the memory server's wake,
  so it overlaps the waiting row, and it is inside the fault-to-entry time of the fault that
  took it.

Within one snapshot, File: m1's clones take 10,638 faults at 2.1 µs alone and 52,830 at 45.6 µs
in the ramp; m7's 9,669 at 2.1 µs and 16,328 at 78.5 µs. The File window is the fault path:
fault-to-entry time is 93 to 95% of the cell's summed windows and waiting for a CPU 7 to 10%
(the two overlap where a fault waits for a CPU), and of 1,366 to 2,351 ms on a CPU, 33 to 131 ms
are in the guest. On UFFD a fault costs 6 to 19 µs in the ramp, userfault waits included, and
the window is 86 to 316 ms. On both backends a clone then spends 1.0 to 1.7 CPU-seconds in the
2 s after it is healthy.

### What a File fault does in the ramp

Snapshot fp, one traced ramp (27.0 clones/s, resume to ready 2,399 ms). `faultpath.py` timed the
fault path's functions from entry to return on the vCPU threads of the Firecracker processes whose
pid is a multiple of 8, and the lock waits of every vCPU thread:

```bash
sudo python3 bench/start-rate/faultpath.py program > $OUT.trace/fault.bt
sudo BPFTRACE_MAX_MAP_KEYS=100000 bpftrace $OUT.trace/fault.bt > $OUT.trace/fault.txt &
make bench-start-rate-run PREP=$R/prep/prep.json OUT=$OUT COUNT=1000 CONCURRENCY=128
# then SIGINT bpftrace
python3 bench/start-rate/faultpath.py report $OUT.trace/fault.txt
```

`perf` and bpftrace `profile` samples cannot show this path: arm64 KVM reports every sample
taken while a vCPU is loaded as guest state, and the kernel stack comes back empty.

The table covers the whole trace, from before the first clone starts until the run has stopped its
clones: everything the traced vCPU threads did, 8,036,815 stage-2 faults on guest memory, not the
resume to ready window alone. A time is wall time from a function's entry to its return: it holds
the function's callees, and any preemption or wait inside it. A row counts every entry of its
function on a traced vCPU thread, whatever called it, except the two rows marked as inside
`wp_page_copy`.

| function | calls | µs per call | µs per stage-2 fault |
|---|---|---|---|
| `user_mem_abort` | 8,036,815 | 54.2 | 54.2 |
| `handle_mm_fault` (0.23 calls per stage-2 fault) | 1,813,387 | 223 | 50.3 |
| `wp_page_copy` | 768,345 | 372 | 35.5 |
| `kvm_pgtable_stage2_unmap`, the calls made inside `wp_page_copy` | 768,345 | 205 | 19.6 |
| `ptep_clear_flush` (every call is inside `wp_page_copy`) | 768,333 | 145 | 13.9 |
| `do_fault` (`filemap_fault`, `do_read_fault`) | 639,538 | 116 | 9.3 |
| `do_numa_page` | 370,596 | 88 | 4.1 |
| `filemap_map_pages` | 259,373 | 28 | 0.9 |

`kvm_pgtable_stage2_unmap` ran 857,693 times in all; the other 89,348 calls were made outside
`wp_page_copy` and are not in its row.

`wp_page_copy`'s time is 66% of `user_mem_abort`'s (35.5 against 54.2 µs per fault), the
copy-on-write of a page that was already mapped read-only: the memory file is mapped `MAP_PRIVATE`,
an earlier read fault mapped the page-cache page read-only (the page it wanted, or the ones
`filemap_map_pages` maps around it), and the write replaces it. Of those 35.5 µs, 33.5 are inside
two calls: KVM's unmap of the stage-2 entry through the MMU notifier, 205 µs a call, and the
`ptep_clear_flush` of the host mapping, 145 µs a call. Why they take that long is not measured. Each
ends in a TLB invalidation, but the three functions the program lists for one were not probeable on
this kernel (`__flush_tlb_range`, `flush_tlb_page`, `kvm_tlb_flush_vmid_ipa`); of the two related
ones that were
probed, `kvm_flush_remote_tlbs` was never called on a traced thread and
`smp_call_function_many_cond` ran 115 times, 0.11 s in all.

The report prints totals, not the number of traced processes. The filter matched 115 of the run's
1000 clones, counted from the VMM pid each clone's log names:

```bash
grep -m1 -ho 'Firecracker Child.id() returned: Some([0-9]*)' $OUT/clones/*.log |
    tr -dc '0-9\n' | awk '$1 % 8 == 0' | wc -l
```

That gives about 69,900 stage-2 faults and about 6,700 `wp_page_copy` calls per traced clone over
its life in the trace. Both are approximate: the snapshot's baseline VM, idle through the ramp, is
in the filter too if its VMM's pid is a multiple of 8. That was not checked: its log
(`prep/baseline.log`) carries the same line, and one more VMM would change the figures in the third
digit (8,036,815 / 116 = 69,283).

Lock waits (`lock:contention_begin` to `contention_end`) of the vCPU threads of all 1000 clones:
655.7 s in all, 543.9 s of it on `rtnl_lock` in `tun_chr_close` (357 waits) and 111.0 s on
`folio_lruvec_lock` in `__page_cache_release` under `folios_put_refs` (578,094 waits); the other 48
stacks hold 0.8 s between them. These are lower bounds. The program keeps one pending wait per
thread, so a contention that begins while another is pending on the same thread replaces it, and the
outer wait is not counted.
The `folios_put_refs` stack is seven frames deep and does not show its caller, and that function is
also reached from the fault path (its LRU batch flush calls it), so whether its
111.0 s belongs to the fault path is not determined. If all of it did, it would be 111.0 s over 1000
clones against `wp_page_copy`'s 285.8 s over the 115 traced ones (768,345 calls of 372 µs), about
2,485 s scaled to 1000: about 4.5% of it, on the waits the program counted.

## A snapshot can carry a compaction pass

m2's clones take 79.6 ms from resume to ready on File at C=1 (53.7 ms on UFFD) and 21,861 stage-2
faults in the window (18,449 on UFFD), where m1, m4 and m7 take 43.5 to 50.1 ms and 9,669 to 12,139
on File: the guest's kcompactd was in a proactive compaction pass when the snapshot was taken, and
every clone runs the pass on (#2020 has the guest program counters). Four of the seven snapshots m1
to m7 carried one: untraced on File at C=1 they take 59.2 to 69.6 ms from resume to ready, the other
three 36.1 to 42.8 ms. The ramp does not set m2 apart on either backend: untraced, File 30.1
clones/s (1000 clones in 33.26 s) and 2,685 ms from resume to ready, UFFD 208 ms, where m4 has 30.1
clones/s (33.23 s), 2,260 ms and 211 ms.
