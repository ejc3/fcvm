# Disk performance

fcvm runs containers inside lightweight virtual machines. Each virtual machine has a
writable raw disk image. fcvm stores those images on Btrfs and uses Btrfs
copy-on-write features to create snapshots and clones without copying unchanged data.

This note compares seven ways to provide that Btrfs storage. It measures the host-side
disk path, not virtual-machine startup or container execution. The tests ran on July
26 and 27, 2026, on one AWS `i8ge.large` host with 2 vCPUs and 16 GiB of memory in
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
| Local NVMe image (fcvm loop) | 8 GiB sparse file on local NVMe/ext4, mounted with fcvm's `mount -o loop` path and formatted as Btrfs |
| Local NVMe image (direct-I/O loop) | 8 GiB sparse file on local NVMe/ext4, attached through a direct-I/O loop device and formatted as Btrfs |
| EBS direct | Dedicated 32 GiB baseline gp3 volume formatted directly as Btrfs |
| EBS image (fcvm loop) | 8 GiB sparse file on the host's gp3/ext4 root volume, mounted with fcvm's `mount -o loop` path and formatted as Btrfs |
| EBS image (direct-I/O loop) | 8 GiB sparse file on the host's gp3/ext4 root volume, attached through a direct-I/O loop device and formatted as Btrfs |
| Archil image (fcvm loop) | 8 GiB sparse file on an Archil FUSE mount, mounted with fcvm's `mount -o loop` path and formatted as Btrfs |
| Archil image (direct-I/O loop) | 8 GiB sparse file on an Archil FUSE mount, attached through a direct-I/O loop device and formatted as Btrfs |

The fcvm-loop rows reproduced fcvm's first-time formatting and mount commands for a
non-Btrfs host filesystem: `mkfs.btrfs IMAGE` followed by
`mount -o loop IMAGE DIRECTORY`. This left loop-device direct I/O off. The
direct-I/O-loop rows use the same 8 GiB image layout with loop-device direct I/O
enabled. The image size was held at 8 GiB for comparison. fcvm normally makes the
logical image size equal to the total capacity of the filesystem containing the
image's parent directory.

A sparse file does not allocate storage for ranges that have never been written. A
Linux loop device makes that file look like a block device, allowing Btrfs to be
created inside it. For the image layouts, a request followed this path:
`fio file → Btrfs → loop device → sparse file → ext4 or Archil FUSE → storage`.
These layers are part of the measured path, so the image results are not raw-provider
or raw-device measurements.

The direct-I/O-loop and direct-EBS mounts requested
`noatime,space_cache=v2,discard=async`. The fcvm-loop mounts requested no Btrfs options
of their own. On this host, `findmnt` reported effective options that included
`relatime,ssd,space_cache=v2,discard=async`.

## I/O results

Sequential tests transferred 2 GiB with 1 MiB requests at queue depth 16. Random tests
used 4 KiB requests for seven seconds at queue depths 1 and 16. Queue depth is the
maximum number of I/O requests fio can have outstanding at once. Every pair in the
table is shown as read first, then write. kIOPS means thousands of I/O operations per
second. Each fio value comes from one run for that layout; it is not an average across
repeated jobs.

fio requested direct I/O, which bypassed the Linux page cache for the test file inside
Btrfs. On the fcvm-loop path, the loop device still used buffered I/O for the sparse
backing file. The backing file could therefore remain in the host page cache. The
direct-I/O-loop path bypassed that cache as well. Caching could still occur in the FUSE
client, storage service, or physical device.

| Storage | Sequential R / W (MiB/s) | 4K QD1 R / W (kIOPS) | 4K QD16 R / W (kIOPS) |
|---|---:|---:|---:|
| Local NVMe image (fcvm loop) | 9,990 / 213 | 81.6 / 39.9 | 225 / 20.6 |
| Local NVMe image (direct-I/O loop) | 841 / 292 | 11.6 / 23.5 | 54.6 / 23.1 |
| EBS direct | 126 / 133 | 1.77 / 1.15 | 3.00 / 1.15 |
| EBS image (fcvm loop) | 9,352 / 117 | 81.9 / 39.5 | 227 / 3.85 |
| EBS image (direct-I/O loop) | 126 / 133 | 1.76 / 1.12 | 3.00 / 1.12 |
| Archil image (fcvm loop) | 9,309 / 490 | 80.5 / 16.8 | 232 / 15.2 |
| Archil image (direct-I/O loop) | 561 / 528 | 1.43 / 17.3 | 13.2 / 16.2 |

The buffered probe used one synchronous fio worker, 4 KiB random I/O, a 70/30
read/write mix, and queue depth 1 for ten seconds. Each run started with a new
copy-on-write copy of that layout's 2 GiB source file. The `fsync` probe configured fio
with `fsync=1` for 256 direct 4 KiB writes; fio reported 255 sync calls. The p50 value
is the median; p99 is the 99th percentile. fio invalidated the test file before the
buffered run, but the experiment did not force provider-level caches into a cold
state.

| Storage | Buffered 70/30 (kIOPS) | `fsync` p50 / p99 / max (ms) |
|---|---:|---:|
| Local NVMe image (fcvm loop) | 12.7 | 0.46 / 0.78 / 371.8 |
| Local NVMe image (direct-I/O loop) | 15.9 | 0.23 / 0.39 / 75.2 |
| EBS direct | 2.57 | 1.94 / 2.24 / 18.0 |
| EBS image (fcvm loop) | 6.52 | 2.04 / 6.32 / 82.3 |
| EBS image (direct-I/O loop) | 2.57 | 5.67 / 5.93 / 13.1 |
| Archil image (fcvm loop) | 32.5 | 5.08 / 10.3 / 210.9 |
| Archil image (direct-I/O loop) | 2.26 | 4.82 / 6.85 / 162.0 |

The fio tests without `fsync` measure how quickly each complete stack returned
ordinary I/O. They do not measure durability. The `fsync` column measures how long the
call took to return at each stack's persistence boundary. Those boundaries do not have
the same failure guarantees: the local NVMe device belongs to one host, while EBS and
Archil are remote durable storage services. This test did not inject failures or
independently verify provider durability.

The very high read results on all three fcvm-loop layouts came from the sparse backing
file's page cache. Each 2 GiB test file had just been written and fit in the host's
16 GiB of memory. These are warm-cache results, not storage-provider read rates. The
buffered random results also include that cache. The cache can improve a running
workload's latency, but it is finite and is lost when a workload moves to another
host.

Direct EBS and the EBS direct-I/O-loop image both clustered around baseline gp3's
provisioned 3,000 IOPS and 125 MiB/s. The fcvm-loop EBS image buffered QD1 writes in
memory, while QD16 random writes and sequential writes pushed enough data to approach
the underlying gp3 limits. Median `fsync` was 1.94 ms on direct EBS, 2.04 ms through
the fcvm loop, and 5.67 ms through the direct-I/O-loop image.

The Archil fcvm-loop row was also dominated by the backing-file cache for reads and
buffered mixed I/O. Its median `fsync` was 5.08 ms, close to the 4.82 ms
direct-I/O-loop result. These numbers describe the complete FUSE, sparse-file,
loop-device, and Btrfs path.

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
data and metadata for the whole filesystem. Each median was calculated from ten clone
operations.

| Storage | Source extents | Reflink copy median | `syncfs` after 10 reflinks | Subvolume snapshot median | `syncfs` after 10 snapshots |
|---|---:|---:|---:|---:|---:|
| Local NVMe image (fcvm loop) | 443,027 | 56.4 s | 101 ms | 6.08 ms | 1.12 ms |
| Local NVMe image (direct-I/O loop) | 315,858 | 1.075 s | 1.402 s | 5.97 ms | 1.16 ms |
| EBS direct | 20,355 | 64.6 ms | 190 ms | 7.84 ms | 1.18 ms |
| EBS image (fcvm loop) | 440,589 | 107.5 s | 181 ms | 11.3 ms | 1.17 ms |
| EBS image (direct-I/O loop) | 19,984 | 64.3 ms | 199 ms | 11.7 ms | 1.12 ms |
| Archil image (fcvm loop) | 246,474 | 828 ms | 1.531 s | 13.1 ms | 1.22 ms |
| Archil image (direct-I/O loop) | 252,497 | 853 ms | 1.184 s | 13.1 ms | 1.11 ms |

The final `syncfs` value is only the work still outstanding after the ten commands.
Btrfs may have committed metadata while those commands were running, so this value
cannot be added to the median or treated as total durable clone latency.

An extent is a contiguous range of file data described by one filesystem metadata
record. A fragmented file has more extents. The direct-I/O-loop reflinks took roughly
3.2–3.4 microseconds per extent. The fcvm-loop local-NVMe and EBS files had more
extents and were also much slower per extent. Their median reflinks took 56 and 108
seconds. The Archil fcvm-loop reflink remained close to its direct-I/O-loop result.
Extent count alone therefore does not explain the fcvm-loop results.

Subvolume snapshot latency stayed between 6 and 13 ms across all seven layouts despite
the different extent counts and raw-file reflink times. A subvolume per virtual machine
made clone creation latency much less sensitive to source-file fragmentation and to
the loop-device path in this experiment.

## Reclamation

After deleting test data, the experiment ran `fstrim` inside Btrfs. `fstrim` reports
freed block ranges to lower storage layers. For an image-backed filesystem, that space
returns to the outer filesystem only if discard passes through the loop device and the
outer filesystem can remove the corresponding ranges from the sparse file.

| Storage | Observed behavior |
|---|---|
| Local NVMe image (fcvm loop) | `fstrim` propagated through the loop device; the sparse file's allocated blocks fell by about 794 MiB |
| Local NVMe image (direct-I/O loop) | `fstrim` propagated through the loop device; the sparse file's allocated blocks fell by about 655 MiB |
| EBS image (fcvm loop) | `fstrim` propagated through the loop device; the sparse file's allocated blocks fell by about 1,056 MiB |
| EBS image (direct-I/O loop) | `fstrim` propagated through the loop device; the sparse file's allocated blocks fell by about 111 MiB |
| EBS direct | `fstrim` was unsupported on the tested attachment path |
| Archil image (fcvm loop) | The tested client and mount path did not support removing ranges from the sparse file, so freed Btrfs blocks remained allocated to that file |
| Archil image (direct-I/O loop) | The tested client and mount path did not support removing ranges from the sparse file, so freed Btrfs blocks remained allocated to that file |

The reclaimed amounts are not a speed or efficiency comparison. The backing files did
not contain the same quantity and layout of deleted data.

Reclaiming blocks from the EBS-backed sparse file did not reduce the provisioned size
or billing size of the EBS volume. EBS volumes can grow in place but cannot shrink;
shrinking requires copying live data to a smaller volume. On the tested Archil client
and mount path, compaction required copying live data to a new sparse image because
hole punching was unavailable.

Clone cleanup was not a timed benchmark. During the runs, deleting ten heavily
fragmented reflinks took minutes and sometimes blocked inside the delete call itself.
The fcvm-loop cleanup was especially slow on local NVMe and EBS. This should not be
used as a provider ranking, but it does show that deletion and garbage collection need
their own latency and capacity tests.

## Summary

- fcvm's non-Btrfs-host loop path used the host page cache for the sparse backing file.
  That made warm reads and some buffered writes much faster, but the results do not
  represent cold storage-provider performance or durability.
- Median `fsync` through the fcvm-loop EBS image was 2.04 ms, close to direct EBS at
  1.94 ms, but its p99 was 6.32 ms rather than 2.24 ms and its maximum was 82.3 ms
  rather than 18.0 ms. The direct-I/O-loop EBS image measured a 5.67 ms median.
- Raw-file reflinks through the fcvm loop were unexpectedly slow on local NVMe and EBS,
  with medians of 56 and 108 seconds. The Archil fcvm-loop median was 828 ms.
- Btrfs subvolume snapshots stayed between 6 and 13 ms across all layouts and were much
  less sensitive to source-file fragmentation than reflink-copying the raw disk file.
- The tests did not measure attach or mount time, cold-cache behavior, concurrent
  virtual machines, migration, or recovery after a failure. Those measurements are
  still required before selecting a default storage layout.
