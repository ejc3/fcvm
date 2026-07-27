# Disk performance

fcvm runs containers inside lightweight virtual machines. Each virtual machine has a
writable raw disk image. fcvm stores those images on Btrfs and uses Btrfs
copy-on-write features to create snapshots and clones without copying unchanged data.

This note compares four ways to provide that Btrfs storage. It measures the host-side
disk path, not virtual-machine startup or container execution. The tests ran on July
26, 2026, on one AWS `i8ge.large` host with 2 vCPUs and 16 GiB of memory in
`us-west-2`, using fio 3.36.

The tests cover sequential throughput, 4 KiB random I/O, buffered mixed I/O, `fsync`
latency, copy-on-write clone latency, and free-space reclamation. They are short,
single-host observations rather than provider benchmarks or service-level guarantees.

## Storage layouts

Each layout presented a Btrfs filesystem, and fio operated on files inside that
filesystem. Amazon EBS is AWS's block-volume service; gp3 is its general-purpose SSD
volume type. Archil is a remote POSIX filesystem mounted by its Linux FUSE client.

| Name | Layout |
|---|---|
| Local NVMe image (direct-I/O loop) | 8 GiB sparse file on local NVMe/ext4, attached through a direct-I/O loop device and formatted as Btrfs |
| EBS direct | Dedicated 32 GiB baseline gp3 volume formatted directly as Btrfs |
| EBS image (direct-I/O loop) | 8 GiB sparse file on the host's gp3/ext4 root volume, attached through a direct-I/O loop device and formatted as Btrfs |
| Archil image (direct-I/O loop) | 8 GiB sparse file on an Archil FUSE mount, attached through a direct-I/O loop device and formatted as Btrfs |

The image layouts are candidate configurations, not fcvm's current loop-device path.
fcvm currently mounts its storage image with `mount -o loop`, which does not enable
direct I/O for the loop device. This experiment explicitly enabled loop-device direct
I/O, which controls how the sparse backing file is opened. That setting is separate
from fio's own direct-I/O setting. Results for the three image layouts therefore do
not measure fcvm's shipped loop configuration. This qualification applies to the I/O,
clone, and reclamation results; EBS direct does not use a loop device. The same tests
need to be repeated through fcvm's shipped automatic mount path before using these
numbers to choose or change fcvm's default.

A sparse file does not allocate storage for ranges that have never been written. A
Linux loop device makes that file look like a block device, allowing Btrfs to be
created inside it. For the image layouts, a request followed this path:
`fio file → Btrfs → loop device → sparse file → ext4 or Archil FUSE → storage`.
These layers are part of the measured path, so the image results are not raw-provider
or raw-device measurements.

As benchmark parameters, all Btrfs mounts used
`noatime,space_cache=v2,discard=async`. fcvm's automatic `mount -o loop` path does not
explicitly request these mount options.

## I/O results

Sequential tests transferred 2 GiB with 1 MiB requests at queue depth 16. Random tests
used 4 KiB requests for seven seconds at queue depths 1 and 16. Queue depth is the
maximum number of I/O requests fio can have outstanding at once. Every pair in the
table is shown as read first, then write. kIOPS means thousands of I/O operations per
second.

Separately from the loop-device setting described above, fio requested direct I/O,
which bypassed the Linux page cache for the test file. Loop-device direct I/O requested
direct access to the sparse backing file. Caching could still occur in the FUSE client,
storage service, or physical device.

| Storage | Sequential R / W (MiB/s) | 4K QD1 R / W (kIOPS) | 4K QD16 R / W (kIOPS) |
|---|---:|---:|---:|
| Local NVMe image (direct-I/O loop) | 841 / 292 | 11.6 / 23.5 | 54.6 / 23.1 |
| EBS direct | 126 / 133 | 1.77 / 1.15 | 3.00 / 1.15 |
| EBS image (direct-I/O loop) | 126 / 133 | 1.76 / 1.12 | 3.00 / 1.12 |
| Archil image (direct-I/O loop) | 561 / 528 | 1.43 / 17.3 | 13.2 / 16.2 |

The buffered probe used one synchronous fio worker, 4 KiB random I/O, a 70/30
read/write mix, and queue depth 1 for ten seconds. Each run started with a new
copy-on-write copy of the same 2 GiB source file. The `fsync` probe performed 256
direct 4 KiB writes and called `fsync` after every write. The p50 value is the median;
p99 is the 99th percentile. fio invalidated the test file before the buffered run, but
the experiment did not force provider-level caches into a cold state.

| Storage | Buffered 70/30 (kIOPS) | `fsync` p50 / p99 / max (ms) |
|---|---:|---:|
| Local NVMe image (direct-I/O loop) | 15.9 | 0.23 / 0.39 / 75.2 |
| EBS direct | 2.57 | 1.94 / 2.24 / 18.0 |
| EBS image (direct-I/O loop) | 2.57 | 5.67 / 5.93 / 13.1 |
| Archil image (direct-I/O loop) | 2.26 | 4.82 / 6.85 / 162.0 |

The direct-I/O results measure how quickly each complete stack returned ordinary I/O.
They do not measure durability. The `fsync` column measures how long the call took to
return at each stack's persistence boundary. Those boundaries do not have the same
failure guarantees: the local NVMe device belongs to one host, while EBS and Archil are
remote durable storage services. This test did not inject failures or independently
verify provider durability.

The EBS results clustered around baseline gp3's provisioned 3,000 IOPS and 125 MiB/s.
Because both EBS layouts reached roughly the same provisioned limit, this run cannot
show how much throughput the candidate image layer would cost above that limit. It did
show a difference in `fsync`: p50 increased from 1.94 ms on direct EBS to 5.67 ms
through the candidate direct-I/O-loop image layout.

In the tested candidate configuration, the Archil image had higher sequential
throughput and QD16 random I/O rates than baseline gp3. Its QD1 random-read and
buffered results were lower, and its `fsync` distribution had a longer tail. These
numbers describe the complete FUSE, sparse-file, loop-device, and Btrfs path.

## Clone results

A Btrfs reflink creates a new file that initially shares the source file's disk blocks.
Btrfs copies a block only after one copy changes it. fcvm currently uses this operation
to clone a virtual machine's raw disk image.

A Btrfs subvolume is a directory tree that Btrfs can snapshot as one unit. The
experiment compared reflink-copying the raw disk file with snapshotting a subvolume
that contained the disk file.

The median columns measure the time until the clone command returned and the new file
or subvolume was visible. They do not include an explicit persistence operation. To
show that cost separately, the test created ten clones and then timed one `syncfs` call
for the containing filesystem. `syncfs` asks the operating system to persist pending
data and metadata for the whole filesystem. Each median also contains ten clone
operations.

| Storage | Source extents | Reflink copy median | `syncfs` after 10 reflinks | Subvolume snapshot median | `syncfs` after 10 snapshots |
|---|---:|---:|---:|---:|---:|
| Local NVMe image (direct-I/O loop) | 315,858 | 1.075 s | 1.402 s | 5.97 ms | 1.16 ms |
| EBS direct | 20,355 | 64.6 ms | 190 ms | 7.84 ms | 1.18 ms |
| EBS image (direct-I/O loop) | 19,984 | 64.3 ms | 199 ms | 11.7 ms | 1.12 ms |
| Archil image (direct-I/O loop) | 252,497 | 853 ms | 1.184 s | 13.1 ms | 1.11 ms |

The final `syncfs` value is only the work still outstanding after the ten commands.
Btrfs may have committed metadata while those commands were running, so this value
cannot be added to the median or treated as total durable clone latency.

An extent is a contiguous range of file data described by one filesystem metadata
record. A fragmented file has more extents. The test files had very different extent
counts, and the reflink results all worked out to roughly 3.2–3.4 microseconds per
extent. The reflink times therefore show the cost of walking extent metadata, not a
speed ranking among the storage providers.

Subvolume snapshot latency stayed between 6 and 13 ms despite the different extent
counts. In these candidate configurations, a subvolume per virtual machine made clone
creation latency less sensitive to source-file fragmentation. Verify this behavior
through fcvm's shipped mount path before changing its storage layout.

## Reclamation

After deleting test data, the experiment ran `fstrim` inside Btrfs. `fstrim` reports
freed block ranges to lower storage layers. For an image-backed filesystem, that space
returns to the outer filesystem only if discard passes through the loop device and the
outer filesystem can remove the corresponding ranges from the sparse file.

| Storage | Observed behavior |
|---|---|
| Local NVMe image (direct-I/O loop) | `fstrim` propagated through the loop device; the sparse file's allocated blocks fell by about 655 MiB |
| EBS image (direct-I/O loop) | `fstrim` propagated through the loop device; the sparse file's allocated blocks fell by about 111 MiB |
| EBS direct | `fstrim` was unsupported on the tested attachment path |
| Archil image (direct-I/O loop) | The tested client and mount path did not support removing ranges from the sparse file, so freed Btrfs blocks remained allocated to that file |

The two reclaimed amounts are not a speed or efficiency comparison. The backing files
did not contain the same quantity and layout of deleted data.

Reclaiming blocks from the EBS-backed sparse file did not reduce the provisioned size
or billing size of the EBS volume. EBS volumes can grow in place but cannot shrink;
shrinking requires copying live data to a smaller volume. On the tested Archil client
and mount path, compaction required copying live data to a new sparse image because
hole punching was unavailable.

In one additional observation, deleting ten heavily fragmented reflink copies on local
NVMe was followed by roughly three minutes of Btrfs transaction writeback. This was
background work after the delete commands, not delete-call latency. It was not a
controlled comparison, so it should not be used as a provider result. It does show that
deletion and garbage collection need their own latency and capacity tests.

## Summary

- The candidate direct-I/O-loop local NVMe image produced the highest random I/O rate
  and lowest median `fsync` latency, but the device is tied to one host.
- Both EBS layouts reached the baseline gp3 limits. In the candidate direct-I/O-loop
  image layout, the clearest measured cost was median `fsync` latency, which was about
  2.9 times the direct-EBS result.
- In the tested candidate configuration, the Archil image had higher sequential
  throughput and QD16 random I/O rates than baseline gp3, but lower QD1 random-read and
  buffered results and a longer `fsync` tail.
- In the candidate configurations, Btrfs subvolume snapshots were less sensitive to
  source-file fragmentation than reflink-copying the raw disk file.
- The tests did not measure attach or mount time, cold-cache behavior, concurrent
  virtual machines, migration, or recovery after a failure. Those measurements are
  still required before selecting a default storage layout.
- These measurements do not benchmark fcvm's current automatic image-backed setup. Run
  the same suite against the shipped `mount -o loop` path and the direct-I/O-loop
  candidate side by side before using the results to predict current performance or
  select a default.
