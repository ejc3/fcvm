# Restore: where the time goes between resume and fc-agent ready

One session on one m8gd.metal-24xl (96 Graviton4 cores, 377 GiB), host kernel Ubuntu's
7.0.0-1018-aws (it has the fix that keeps namespace cleanup from holding RTNL through one
`rcu_barrier()` per bridge), store btrfs on the instance-store NVMe RAID0 (`/dev/md127`),
2026-11-24, fcvm at main 9d7b6f94f. Guest: nginx:alpine, 1 vCPU, 256 MiB, rootless network,
snapshot taken when the baseline first reported healthy. Three snapshots per backend (m1, m2, m3),
each with its own cells. Every cell is `make bench-start-rate-run` with `vcpustat.py` and
`s2fault.bt` running, read with `restore_timeline.py --trace` (README, "Restore timeline"). UFFD
cells ran after 10 untraced clones had recorded a working set.

How to read the numbers:

- A value is the p50 over the cell's clones unless the row says otherwise. Three values in a row
  are m1, m2, m3.
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

## One clone at a time (C=1, N=30, traced)

| p50 (ms unless the row says) | File (m1, m2, m3) | UFFD (m1, m2, m3) |
|---|---|---|
| resume -> epoch | 10.3, 10.4, 9.8 | 5.8, 6.2, 5.7 |
| epoch -> handling | 0.6, 0.7, 0.4 | 0.0, -0.0, -0.0 |
| handling -> tcp_begin | 9.3, 8.3, 8.9 | 4.8, 5.3, 4.9 |
| tcp_begin -> tcp_done | 0.6, 2.6, 1.3 | 1.0, 1.1, 0.9 |
| tcp_done -> clock | 1.8, 2.2, 4.6 | 1.3, 2.2, 2.9 |
| clock -> published | 0.5, 2.3, 0.7 | 1.1, 1.0, 0.9 |
| phases_done -> ack | 10.6, 5.7, 6.5 | 4.8, 4.0, 3.6 |
| ack -> ready | 0.1, 0.1, 0.1 | 0.1, 0.1, 0.1 |
| resume -> ready | 33.6, 32.1, 32.1 | 18.8, 20.1, 18.9 |
| ready -> healthy | 2.6, 2.3, 3.5 | 2.5, 2.8, 1.9 |
| fc-agent's restore phases, guest clock (total_ms) | 12.2, 15.4, 15.1 | 8.4, 9.7, 9.8 |
| stage-2 fault time | 17.8, 17.8, 17.0 | 6.1, 7.0, 6.4 |
| userfault time | 0.0, 0.0, 0.0 | 0.1, 0.7, 0.1 |
| vCPU on CPU | 29.6, 29.1, 28.3 | 12.7, 16.6, 13.5 |
| vCPU waiting to run | 0.0, 0.0, 0.0 | 0.0, 0.0, 0.0 |
| guest time (p50 of whole 10 ms ticks) | 6.8, 6.5, 7.1 | 5.6, 6.3, 5.7 |
| stage-2 faults (count) | 10418.4, 10195.3, 9964.7 | 7323.9, 7447.4, 7306.8 |
| device aborts (count) | 124.0, 92.8, 105.1 | 169.9, 87.0, 160.0 |
| userfaults (count) | 0.0, 0.0, 0.0 | 1.9, 37.0, 6.0 |
| per stage-2 fault to guest entry (us) | 1.7, 1.7, 1.7 | 0.8, 0.9, 0.9 |
| clones/s | 15.6, 15.9, 15.9 | 19.8, 19.0, 20.1 |
| most requests in flight on the store | 3, 4, 3 | 3, 5, 6 |

## Ramp: 128 starting at once, to 1000 alive

| p50 (ms unless the row says) | File (m1, m2, m3) | UFFD (m1, m2, m3) |
|---|---|---|
| resume -> epoch | 7.6, 6.8, 7.1 | 7.3, 16.5, 7.3 |
| epoch -> handling | 0.4, 0.5, 0.3 | 0.4, 0.0, 0.4 |
| handling -> tcp_begin | 5.5, 5.6, 5.9 | 5.8, 7.7, 6.1 |
| tcp_begin -> tcp_done | 1.0, 1.2, 1.0 | 1.1, 1.3, 1.1 |
| tcp_done -> clock | 2.0, 2.9, 3.8 | 1.8, 3.4, 3.4 |
| clock -> published | 0.3, -0.0, 0.9 | 1.7, 0.0, 0.9 |
| phases_done -> ack | 8.2, 6.6, 7.5 | 5.5, 6.4, 5.2 |
| ack -> ready | 0.1, 0.1, 0.1 | 0.1, 0.1, 0.1 |
| resume -> ready | 26.0, 26.3, 29.4 | 24.7, 55.3, 26.6 |
| ready -> healthy | 5.2, 9.1, 9.0 | 4.0, 6.0, 5.1 |
| fc-agent's restore phases, guest clock (total_ms) | 8.9, 10.3, 11.8 | 10.8, 15.5, 11.8 |
| stage-2 fault time | 8.9, 8.7, 9.2 | 8.1, 24.8, 8.9 |
| userfault time | 0.0, 0.0, 0.0 | 0.2, 0.5, 0.4 |
| vCPU on CPU | 19.4, 19.3, 20.6 | 18.1, 47.5, 18.9 |
| vCPU waiting to run | 0.0, 0.2, 0.3 | 0.1, 0.7, 0.2 |
| guest time (p50 of whole 10 ms ticks) | 8.2, 9.1, 9.0 | 8.4, 18.7, 9.0 |
| stage-2 faults (count) | 7669.4, 7516.3, 7580.7 | 7614.1, 12266.3, 7610.5 |
| device aborts (count) | 128.4, 115.3, 150.3 | 118.2, 542.7, 111.8 |
| userfaults (count) | 0.0, 0.0, 0.0 | 6.0, 7.0, 10.9 |
| per stage-2 fault to guest entry (us) | 1.2, 1.2, 1.2 | 1.1, 2.1, 1.2 |
| clones/s | 198.0, 170.4, 175.7 | 127.4, 111.7, 123.1 |
| most requests in flight on the store | 88, 58, 109 | 79, 40, 49 |

## Not measured again

This file's earlier sections (after ready, a File fault's path through the kernel in the ramp, and a
snapshot that carried a compaction pass) were measured on Ubuntu's 7.0.0-1013-aws, whose namespace
cleanup holds RTNL through one `rcu_barrier()` per dying bridge (fixed upstream in 6.18.60 and 7.2),
and are not kept. Their commands are in the README ("Restore timeline", "Fault path").
