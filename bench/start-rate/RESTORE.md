# Restore: where the time goes between resume and fc-agent ready

One session on one c8g.metal-48xl (192 cores, kernel 7.0.0-1013-aws), fcvm at 4f67a0fbd, store on
btrfs in RAM. Guest: nginx:alpine, 1 vCPU, 256 MiB, rootless network, snapshot taken when the
baseline first reported healthy. Four snapshots, each with its own cells: m2, m3 and m4, and m1,
whose clones run on a kcompactd pass the snapshot caught (last section). Every cell is
`make bench-start-rate-run`; traced cells also ran `vcpustat.py` and `s2fault.bt` (README,
"Restore timeline"). UFFD cells ran after the memory server had recorded a working set.

How to read the numbers:

- A value is the p50 over the cell's clones unless the row says otherwise. Three values in a row
  are m2, m3, m4.
- "Resume to ready" is between two lines of the clone's fcvm process. Steps between fc-agent's
  console lines are the harness's receive times of those lines, which lag the guest by several
  ms (the acknowledgement, which arrives over vsock, is often logged before the console line
  the guest wrote ahead of it). fc-agent's phase record is the guest's own clock.
- Counts and fault times are the part of `s2fault.bt`'s buckets inside the window (10 ms buckets,
  the last one in proportion). "Fault to guest entry" is measured per fault, from
  `kvm:kvm_guest_fault` on guest memory to the vCPU's next `kvm:kvm_entry`.
- Ticks are scheduler ticks of 10 ms; a row of ticks is the mean over the cell's clones.
- The tracers slow a clone: untraced, the same three snapshots take 37.9, 34.8 and 36.1 ms from
  resume to ready on File at C=1 (10 clones each).

## One clone at a time (C=1, N=30, traced)

| | File | UFFD |
|---|---|---|
| resume to ready (ms) | 44.1, 42.5, 44.0 | 27.3, 22.8, 24.1 |
| vCPU thread on a CPU (ms; samples 4 ms apart bracket the window) | 47.7, 47.6, 48.7 | 31.6, 29.6, 28.4 |
| vCPU thread runnable, waiting for a CPU (ms) | 0.0 | 0.0 |
| vCPU thread in a userfault (ms) | 0.0 | 0.1 |
| stage-2 faults on guest memory in the window | 9,907, 9,509, 9,770 | 7,128, 6,544, 7,059 |
| fault to guest entry, all of the window's faults (ms) | 21.0, 20.2, 21.3 | 6.2, 5.8, 6.3 |
| fault to guest entry, per fault (µs) | 2.1, 2.1, 2.2 | 0.9, 0.9, 0.9 |
| device aborts in the window (console, virtio) | 3,039, 2,962, 2,970 | 2,856, 2,263, 2,039 |
| ticks: guest / host kernel / VMM user (mean ms) | 9.0 / 27.0 / 0.7, 10.0 / 29.7 / -0.3, 9.3 / 29.7 / 0.0 | 8.7 / 12.0 / 0.3, 8.3 / 10.3 / 0.0, 7.0 / 12.0 / -0.3 |
| ready to healthy (ms) | 6.5, 6.7, 6.8 | 6.9, 7.5, 6.8 |

All 30 clones of each cell are in every row. The vCPU is on a CPU for the whole window. On File
about half the window is the host mapping the ten thousand guest pages the restore touches into
an empty stage-2 table; the rest is guest code and three thousand exits to the VMM for device
accesses. On UFFD the recorded working set is already in the clone's memory and a fault costs
0.9 µs.

Steps on File, console-line times (ms): resume to `detected restore-epoch` 12.6, 11.9, 10.5; to
tcp-cleanup begin 17.2, 16.7, 19.7; tcp-cleanup 2.1, 1.9, 3.3; to the clock step 5.1, 5.2, 5.4;
to `snapshot network published` 1.3. The gate, from fc-agent's phase record: verify 0.24, 0.23,
0.36 ms; UDP rule 0.22, 0.25, 0.26 ms; egress commit 1.74, 1.72, 2.78 ms; publish 0.08, 0.07,
0.08 ms; all 30 restores of every cell went over nfnetlink (`tcp_gate_netlink`), none through
an iptables process.

## After ready

Per clone, the 2 s that follow its first healthy check (the clones still alive then: 8 or 9 of a
File cell's 30, 5 of a UFFD cell's):

| | File | UFFD |
|---|---|---|
| vCPU thread on a CPU (ms) | 240, 217, 244 | 242, 220, 259 |
| VMM main thread on a CPU (ms) | 24.4, 23.2, 21.7 | 22.9, 22.9, 22.3 |

That is five to eleven times the restore's window. fc-agent's restore-epoch watcher reads the vsock
transport reset after it has dispatched the restore (29 or 30 of 30 clones in every cell), and
the reset opens a one-second window of metadata fetches 2 ms apart. The chronyd start (`chronyd
restarted` 39 to 45 ms after ready on File) and the journald restart (75 to 78 ms after) run in
the same two seconds.

## Ramp: 128 starting at once, to 1000 alive

Untraced cells:

| | File | UFFD |
|---|---|---|
| clones healthy per second, whole ramp | 30.9, 32.6, 30.9 | 24.4, 29.0, 20.1 |
| resume to ready, p50 (ms) | 2,631, 1,270, 2,698 | 99, 73, 140 |
| resume to ready, p99 (ms) | 4,131, 3,210, 3,833 | 840, 1,213, 2,048 |
| CPU-seconds per start, all processes / Firecracker (pidstat) | 5.04 / 4.81, 4.60 / 4.30, 5.12 / 4.91 | 5.65 / 5.16, 4.69 / 4.25, 7.73 / 7.21 |
| cores busy over the ramp, of 192 (pidstat CPU-seconds / ramp seconds) | 156, 150, 158 | 138, 136, 155 |

Traced cells (resume to ready window, per clone):

| | File | UFFD |
|---|---|---|
| clones in the rows, of 1000 | 1000, 1000, 1000 | 939, 971, 983 (the sampler never saw the VMM of the rest) |
| window (ms) | 2,522, 1,233, 2,870 | 86, 101, 172 |
| vCPU thread on a CPU (ms) | 2,402, 1,237, 2,664 | 106, 111, 132 |
| waiting for a CPU, p50 / p90 (ms) | 89 / 411, 25 / 220, 157 / 595 | 3 / 52, 10 / 128, 38 / 244 |
| in a userfault, p50 / p90 (ms) | 0 / 0 | 0.1 / 18, 2 / 103, 13 / 237 |
| stage-2 faults on guest memory in the window | 42,431, 14,418, 52,980 | 7,543, 7,334, 7,599 |
| fault to guest entry, all of the window's faults (ms) | 2,370, 1,168, 2,644 | 38, 45, 80 |
| fault to guest entry, per fault (µs) | 51.3, 74.3, 48.8 | 4.9, 5.7, 8.8 |
| ticks: guest / host kernel (mean ms) | 116 / 2,182, 29 / 957, 143 / 2,316 | 36 / 66, 51 / 64, 76 / 139 |
| vCPU thread on a CPU in the 2 s after healthy (ms) | 1,304, 1,634, 1,181 | 1,572, 1,438, 1,644 |

Within one snapshot, File: m2's clones take 9,907 faults at 2.1 µs alone and 42,431 at 51.3 µs in
the ramp; m3's 9,509 at 2.1 µs and 14,418 at 74.3 µs. The File window is the fault path: 92 to
95% of it is fault-to-entry time, 2 to 6% waiting for a CPU. On UFFD a fault costs 5 to 9 µs in
the ramp and the window is 86 to 172 ms. On both backends a clone then spends 1.2 to 1.6
CPU-seconds in the 2 s after it is healthy.

### What a File fault does in the ramp

A fourth snapshot without a pass, one traced ramp (29.2 clones/s, resume to ready 2,208 ms),
entry and return probes on the fault path's functions for the vCPU threads of one clone in
eight. `perf` and bpftrace `profile` samples cannot show this path: arm64 KVM reports every
sample taken while a vCPU is loaded as guest state, and the kernel stack comes back empty.

| function | calls | µs per call | µs per stage-2 fault |
|---|---|---|---|
| `user_mem_abort` | 8,808,873 | 47.4 | 47.4 |
| `handle_mm_fault` (22% of the faults reach it) | 1,944,582 | 197 | 43.5 |
| `wp_page_copy` | 805,300 | 367 | 33.5 |
| `kvm_pgtable_stage2_unmap`, from the MMU notifier in `wp_page_copy` | 869,402 | 208 | 20.5 |
| `ptep_clear_flush`, in `wp_page_copy` | 805,283 | 143 | 13.1 |
| `do_fault` (`filemap_fault`, `do_read_fault`) | 696,381 | 90 | 7.1 |
| `do_numa_page` | 409,613 | 50 | 2.3 |
| `filemap_map_pages` | 268,585 | 29 | 0.9 |

71% of the time is the copy-on-write of a page the guest had read before it wrote it: the
memory file is mapped `MAP_PRIVATE`, the read mapped the page-cache page read-only, and the write
replaces it. Replacing a mapped page costs two TLB invalidations, KVM's unmap of the stage-2
entry through the MMU notifier and the `ptep_clear_flush` of the host mapping. On arm64 both are
broadcast to every CPU, and with 128 clones doing it at once each takes 143 to 208 µs. About
6,400 pages per clone go through it. The lock contention tracepoints show no lock in this path:
the vCPU threads' contended waits were `folio_lruvec_lock` in `__page_cache_release` (121 s over
615,802 waits) and `rtnl_lock` in `tun_chr_close`, both at exit.

## A snapshot can carry a compaction pass

m1's clones take 68.0 ms from resume to ready on File at C=1 (26.9 ms on UFFD) and 18,885 stage-2
faults in the window, where m2 to m4 take 42.5 to 44.1 ms and 9,509 to 9,907: the guest's
kcompactd was in a proactive compaction pass when the snapshot was taken, and every clone runs
the pass on (#2020 has the guest program counters). In the ramp m1 gives UFFD 20.2 clones/s and
337 ms from resume to ready (traced: 10,469 faults at 17.8 µs), against 20.1 to 29.0 clones/s
and 73 to 140 ms for the other three. On File its ramp (30.9 clones/s, 2,654 ms) is not apart
from m2's and m4's.
