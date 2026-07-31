# Design: durable remote workloads

Status: **proposal**

fcvm currently runs a container in a local microVM and removes the VM's writable
disk when that run ends. This design turns the same execution model into a durable
service. A client keeps a small workload identifier while the service owns compute
placement, persistent storage, snapshots, idle shutdown, restart, and cache reuse.

The client experience is intentionally smaller than Docker. The first version
supports run, start, stop, restart, delete, exec, logs, inspection, images, and port
forwarding. Port forwarding is TCP-only in the first version. It does not support
host mounts or the full Docker API.

## Core model

The durable object is a **workload**, not a running VM. A workload contains:

- a stable workload ID;
- a container image and command;
- a compute shape (CPU, memory, architecture, and required capabilities);
- port-forwarding configuration;
- one private logical workspace;
- lifecycle state, stop reason, and the current writer epoch (a monotonically
  increasing fencing number); and
- an optional bootable VM checkpoint.

The VM is an ephemeral execution of that workload. It can disappear while the
workload remains.

The client stores only the workload ID and its credentials. It never stores a host
name, disk ID, mount path, or snapshot location. The workload ID routes every
request to the cell that owns the workload.

## Non-negotiable invariants

1. Every workload has exactly one authoritative logical workspace.
2. A workload enters `Running` only after its storage provider has committed an
   exclusive, self-sufficient workspace.
3. Every workload block device visible to the guest is workspace-owned. Every boot
   or checkpoint input required to start it is an authoritative workload resource.
   Cache paths are never attached as runtime drives.
4. Deleting every host and cell artifact-cache entry after preparation cannot
   affect start, run, stop, restart, or fork from the workload.
5. At most one VM can write a workspace. A new writer starts only after the old
   writer has been fenced.
6. A running workload stays on one compute host. There is no live migration.
7. Relocation happens only while the workload is stopped.
8. A fork has its own complete logical workspace before it starts.
9. A client's workloads and caches remain inside its assigned cell.

Logical completeness does not mean eagerly copying every byte. A Btrfs subvolume
snapshot, an EBS volume created from a snapshot, or an Archil workspace clone can
share physical data or populate blocks lazily. The storage provider must guarantee
that the new workspace remains valid after its source cache entry is deleted. If a
backing object is still required, that object is authoritative workload storage,
not cache.

## Workspaces

A workspace is the folder-like storage namespace presented to one workload. It
contains the root disk, container storage, and the durable metadata needed to boot
that disk. A storage backend is free to represent the namespace as a Btrfs
subvolume, a block volume, or a remote workspace, but the control plane always sees
one workspace per workload.

A **workspace generation** is an immutable, committed point-in-time view of that
workspace. A **VM checkpoint** is the VMM, device, and memory state paired with
exactly one workspace generation. A new or cold-booted workload has no VM
checkpoint. An idle-suspended workload has one. The checkpoint file and every
backing object needed for lazy memory restore are part of the workspace's
authoritative storage namespace.

Preparation also places the exact kernel, initrd, and other boot inputs needed by
the workload in that authoritative namespace. The VMM executable, runtime sockets,
and worker diagnostics belong to ephemeral compute and are not workload data.

The client-visible durability boundary follows normal filesystem rules. A
successful `stop` or idle suspension returns only after guest filesystems are
flushed and the storage provider has durably committed the workspace generation.
An `fsync` that completed before abrupt compute loss survives within the provider's
declared failure domain; dirty data that was never flushed has normal Linux crash
semantics. EBS and Archil providers must cover loss of the compute host. The local
provider covers loss of the VM or worker process while its underlying local disk
remains intact. The provider conformance suite verifies this boundary with failure
injection.

Btrfs is the preferred local implementation. A host uses a Btrfs filesystem with
one subvolume per workload and uses subvolume snapshots or reflinks for fast
copy-on-write creation. The [disk measurements](disk-performance.md) found
subvolume snapshot creation much less sensitive to raw-disk fragmentation than
reflinking the current raw image.

The storage provider owns these operations:

- prepare a new workspace from an image, committed generation, or empty base;
- attach it to a compute host under a fenced writer lease;
- flush and commit a generation;
- create an independent fork from a committed generation;
- detach and fence a writer;
- relocate or reattach a stopped workspace;
- grow capacity; and
- delete the workspace and its retained generations.

`prepare` is a transaction. It creates a staging workspace, establishes every
logical path, validates the result, commits its initial generation, and atomically
publishes the control-plane reference to that workspace. A failed preparation
leaves no runnable workload. Orphaned staging resources are safe to reclaim.

## Compute and storage providers

Compute and storage are separate plugin boundaries. A compute provider allocates a
host of a declared shape and runs fcvm there. A storage provider supplies the
workload workspace and its durability operations. The scheduler chooses a
compatible pair.

The initial provider set is:

| Boundary | Providers |
|---|---|
| Compute | local, AWS, Kubernetes |
| Storage | local folder, EBS, Archil |

The local providers implement the complete contracts without a remote service.
They are the reference implementation for lifecycle tests, failure injection, and
fuzzing.

The AWS compute provider uses Terraform-managed instance pools and a declared
catalog of shapes, quotas, and spot eligibility. The scheduler can add capacity
only within those limits. The Kubernetes provider allocates workers with the
required virtualization capabilities. These compute providers sit above fcvm's
existing `Hypervisor` abstraction; Firecracker versus Cloud Hypervisor is a VMM
choice inside an allocated worker, not a compute-provider choice.

The storage mappings are:

| Provider | Workspace representation | Fork and relocation |
|---|---|---|
| Local folder | One Btrfs subvolume per workload when Btrfs is available; otherwise one fully copied directory | Btrfs snapshot/reflink locally; stopped copy to another host |
| EBS | One EBS volume per workload | EBS snapshot and volume creation; detach, fence, and reattach while stopped |
| Archil | One Archil workspace per workload | Validated native workspace clone, otherwise a full workspace copy |

EBS Multi-Attach is not part of this design. An EBS workspace has one attached
writer. EBS volumes can grow in place; shrinking is a stopped compaction that copies
live data to a smaller volume and atomically switches the workload pointer. Archil
and local storage follow the same logical contract even when their physical
capacity and clone mechanisms differ.

The Archil provider is enabled only after its selected clone path passes the
independence, fencing, durability, and artifact-cache deletion tests. Native clone
failure falls back to a full copy; it never weakens the workspace contract.

## Writer fencing

The control plane records a writer epoch for each workspace. Every start
atomically allocates an epoch strictly greater than every previous epoch and passes
it to both compute and storage providers. An expired control-plane lease is not
enough to authorize a second writer: the storage provider must prove that every
previous writer can no longer issue writes.

Examples of proof are confirmation that the old EBS compute instance has reached a
terminal state and its non-Multi-Attach volume is detached, or a provider-enforced
access mechanism that revokes old credentials and rejects stale epochs. Submitting
a termination or detach request is not proof.

An advisory filesystem lock can serialize cooperating local starts, but it is not
writer fencing: it cannot revoke an old process's ability to write through an open
file descriptor. Before activating a higher epoch, the local provider must either
terminate the old worker, confirm process exit, and tear down its writable mount,
or use a mandatory, revocable access layer that rejects the old epoch. If any
provider cannot prove fencing, the workload stays in `Fencing` and cannot start,
relocate, or be reported as stopped.

Every mutating request has an idempotency key scoped to the client registration
inside its cell. Before side effects, the cell durably records the key, operation
kind, canonical request-parameter digest, progress, and eventual result. The first
version never expires or reuses an accepted key.

A retry with the same key and parameters joins the in-progress operation or
replays its exact terminal result, including the original workload ID, relocation
result, or deletion tombstone. A retry with different parameters is rejected.
These rules apply to `run`, relocation, deletion, and every other mutating
operation across controller restarts and network retries. State changes also use
conditional updates against the expected workload revision and writer epoch.
Duplicate requests, controller restarts, and network partitions cannot create,
attach, or publish two physical writers.

## Lifecycle

The externally visible semantics are:

- `run IMAGE ...` creates and starts a durable workload. Container exit leaves the
  workload stopped with its workspace intact.
- `run --rm IMAGE ...` deletes the workload and workspace after container exit.
  This matches fcvm's current cleanup behavior.
- `start` runs the configured container again. `restart` performs a cold restart
  against the same workspace.
- `stop` stops the container and VM but retains the workload and workspace.
- `exec`, live logs, and new port connections automatically wake a workload that
  the system suspended for idleness. They do not restart a container that exited or
  was explicitly stopped.
- `inspect` reads the workload record without starting compute.
- `delete` stops and fences a running workload before deleting its storage.

The internal state machine is:

```text
Absent -> Preparing -> Stopped -> Starting -> Running
Preparing -> Absent
Starting -> Fencing -> Stopped
Running -> Checkpointing -> Stopped
Checkpointing -> Running
Checkpointing -> Fencing -> Stopped
Running -> Stopping -> Stopped
Running -> Fencing -> Stopped
Running -> Stopping -> Deleting -> Deleted
Stopped -> Relocating -> Stopped
Stopped -> Deleting -> Deleted
```

`Stopped` always means the workspace is bootable and no old writer can issue
writes. Initial `Stopped` has no compute owner. `Starting` allocates the next writer
epoch, attaches storage under that epoch, and launches the VMM. It publishes
`Running` only after the guest is healthy. A failed start enters `Fencing` and
returns to `Stopped` only after the attempted writer is proven unable to write. The
stop reason distinguishes an initial, idle-suspended, exited, explicitly stopped,
or failed workload. Only an idle-suspended workload wakes without an explicit
`start` or `restart`.

Idle shutdown uses the `Checkpointing` transition. The system quiesces and pauses
the guest, flushes the workspace, commits a workspace generation, captures a VM
checkpoint paired with that generation, then terminates and fences the writer. The
guest remains quiesced from the snapshot boundary through fence confirmation, so
the workspace cannot advance beyond the paired generation. Only then does the
control plane publish `Stopped`. The VM checkpoint and any backing used for lazy
memory restore are authoritative workload storage. They are never artifact-cache
entries.

If checkpoint creation fails before termination begins, the system discards the
staged generation and checkpoint and safely resumes the same exclusively leased
writer. If termination has begun or writer ownership is uncertain, the workload
enters `Fencing` and remains non-runnable until that writer is fenced. A VM
checkpoint is never paired with a different workspace generation.

The initial idle rule is no active command or exec session, no live log session, no
active forwarded connection, and no client keep-awake lease for the configured
timeout. Any wake-up request races through the same conditional state transition,
so only one start can win.

An unexpected compute loss enters `Fencing` and has ordinary machine-crash
semantics. Durable workspace writes survive according to the boundary above;
uncommitted VM memory does not. After fencing the failed host, the system
cold-boots the authoritative workspace. It never restores a VM checkpoint against
a workspace that has advanced beyond the checkpoint's paired generation.

A VM checkpoint records its architecture, CPU requirements, kernel, hypervisor, and
snapshot format. The scheduler restores it only on a compatible host and against
its exact workspace generation. An explicit cold restart discards incompatible VM
state and boots the same workspace.

Delete first stops and fences the workload, then atomically commits an irreversible
tombstone before physical cleanup. A failure before the tombstone leaves the intact
stopped workload. Once the tombstone is committed, `Deleting` never returns to
`Stopped`; retries only finish cleanup, and leftover provider resources are
non-authoritative orphans that cannot resurrect the workload ID.

## Container image delivery and artifact caching

For a registry image, the cell resolves immutable image digests and reuses any
content already present. For a locally defined image, the client exports it on
first use and uploads only content the cell does not already have. This preserves
fcvm's current content-keyed image reuse while allowing the compute host to be
remote.

Preparation chooses the fastest correct source for each piece of content. Its cost
model includes:

- an immutable workspace generation or digest-verified immutable content resident
  beside an attached workspace;
- complete or partial content on a compatible compute host;
- content on another mounted disk in the cell;
- cell-level cached content; and
- the original registry or client upload.

Placement and source selection happen together. A host with the right shape and
most of the required content can be faster than an empty host, while a constrained
host can still lose to fresh capacity. Stale locality information only causes a
cache miss and fallback; it cannot affect correctness.

The content-addressed store is an **artifact cache**. It holds immutable OCI blobs,
base filesystems, image exports, and other reusable preparation inputs. Entries are
published by atomic rename after digest validation. Host caches use capacity-based
LRU eviction. Cell caches use the same rule with their own capacity. Neither cache
uses running-workload pins or correctness reference counts.

Preparation acquires a short-lived read handle for each cache input. Eviction marks
an entry unavailable to new readers and removes it only after existing read handles
close. This handle ends before `prepare` commits and is not a workload pin. If a
backend cannot preserve an active read, preparation treats disappearance as a read
failure, discards the incomplete staging data for that digest, and retries from
another source. It validates every digest before publication, so eviction can
delay or fail preparation but cannot publish a partial workspace.

Btrfs maintains the physical extent reference counts created by reflinks and
subvolume snapshots. Storage providers maintain the resources that back
authoritative workspaces and checkpoints. Those are separate from artifact-cache
garbage collection.

## Forking

Fork consumes a workspace generation and creates a new workload ID and workspace.
The child cold-boots from that disk state; it does not inherit running memory or
open connections. If the source is running, fcvm briefly quiesces it, commits the
generation, and resumes it on the same host. That is snapshot creation, not
migration.

The destination provider then creates a logically complete child workspace. Btrfs
can share extents, EBS can create a volume from a snapshot, and Archil can use a
native workspace clone. Physical sharing is invisible above the provider contract.
Deleting the source workload or any artifact-cache entry cannot break the child.

## Stopped relocation and capacity

The scheduler never moves a running workload. Each storage provider enforces a
per-workspace allocation and a physical-pool reserve that workloads cannot consume.
Local Btrfs can use qgroups and reserved pool capacity; block and remote providers
use equivalent quotas or write admission. High-water marks trigger growth, stop
new workload admission, and idle eligible workloads while protected capacity still
remains.

If growth cannot complete before a workload reaches its enforced allocation, the
provider backpressures its writes for a bounded interval and then returns the
provider's documented `ENOSPC` or quota error. It never reports a write as
successful if capacity prevented allocation or caused any of that write to be
discarded. Durability across a provider failure follows the `fsync` and generation
commit boundary defined above. Workload data cannot consume the protected pool
reserve, and one workload cannot exhaust storage needed by another workload or the
control plane. Relocation uses only workloads that are already stopped; it is not
recovery from an exhausted running filesystem. Relocation then:

1. verifies that the workload is stopped;
2. fences and detaches the old writer;
3. reattaches the workspace or prepares a complete copy at the destination;
4. atomically publishes the new authoritative location; and
5. removes the old copy only after the new location is committed.

A failed relocation leaves one authoritative location. Retries use the same
operation ID and either finish the pending switch or discard the uncommitted
destination.

Spot interruption follows the same boundary. On interruption notice, the worker
checkpoints and stops the workload. It does not start elsewhere until the old
writer is fenced. If the instance disappears before checkpoint completion, the
system uses the unexpected-compute-loss path.

## Cells

The service starts with two cells to force the architecture to respect cell
boundaries from the beginning. Client registration permanently selects one cell.
The registration credential and every workload ID contain a routable cell
component, so the front door forwards requests without a global workload lookup.

Each cell owns its workload database, scheduler, provider credentials, workspaces,
retained generations, VM checkpoints, backups, artifact caches, and network
endpoints. Workload state, authoritative storage, and cached content do not leak
across cells. The first version does not fail a workload over to another cell; cell
disaster recovery restores that cell's control-plane data and storage in place.

## Networking

Port mappings belong to the workload record, not a compute host. A cell-level
router or proxy owns the stable client endpoint and forwards new connections to the
workload's current placement. A connection that arrives while the workload is
idle-suspended triggers a wake-up before forwarding. An exited or explicitly
stopped workload rejects the connection until the client starts it.

Stopping, restarting, or relocating a workload breaks existing TCP connections.
To both the workload and its client this looks like a network interruption. They
reconnect through the same workload endpoint after the VM is available. The design
does not preserve TCP state.

## Relationship to fcvm today

This is a target service architecture, not a description of shipped behavior.
Current fcvm provides the local execution primitives but has these gaps:

- `podman run` always cleans up the VM directory and writable root disk;
- the CLI has no durable workload or `--rm` distinction;
- `serve` keeps sandboxes in one process's memory and deletes them on an
  age-based timeout or graceful SIGINT/SIGTERM shutdown;
- every locally defined image mode attaches a read-only cache-backed image drive;
  Overlay actively uses it as an additional image store, while Btrfs and Archive
  import its data but still retain the drive in saved restore configuration;
- snapshot clones are ephemeral local VMs rather than durable workloads; and
- restored port mappings can receive a new host address instead of a stable
  cell-owned endpoint.

Implementation must materialize all required container content and remove
cache-backed image drives from every mode before `Running` or checkpoint
publication. Existing full and disk-only snapshot code, atomic snapshot
publication, Btrfs reflinks, image digest locks, and local port-forwarding
implementations remain useful primitives underneath the new contracts.

## Test strategy

All provider implementations run the same conformance suite. The local compute and
local-folder storage providers run it first and remain capable of exercising the
entire design on one development machine.

The required tests are:

- prepare a workload, delete the entire artifact cache, then start, run, stop,
  restart, and fork it;
- concurrently issue duplicate starts and prove that only one writer reaches the
  workspace;
- retry `run`, relocation, and deletion across controller restarts, verify exact
  result replay, and reject key reuse with different parameters;
- inject failures before and after epoch allocation, storage attachment, VMM launch,
  VMM termination, storage detachment, and fence confirmation;
- kill the controller or worker after every durable transition, restart it, and
  verify idempotent recovery;
- partition a stale worker that retains an open writable file descriptor, fence it,
  and prove its writes fail before a new epoch attaches;
- evict a cache input during preparation, verify retry or clean failure, and prove
  that no partial workspace is published;
- fail every step of preparation, checkpoint, fork, and relocation and verify that
  exactly one authoritative source-workload workspace remains;
- fail every step of deletion and verify that a pre-tombstone failure leaves the
  intact workload while a post-tombstone failure converges to zero authoritative
  workspaces and never resurrects the workload;
- drive a host disk to its admission high-water mark, inject growth failure, and
  verify bounded backpressure or an isolated quota error without physical-pool
  exhaustion, silently discarded capacity-constrained writes, or loss after a
  successful `fsync` or generation commit;
- interrupt spot compute before and after checkpoint commit;
- reconnect through the same port endpoint after stop and relocation;
- assign clients to two cells and prove state, credentials, routing, and caches do
  not cross the boundary;
- fuzz lifecycle command sequences, duplicate delivery, controller restarts, and
  provider error timing; and
- run the existing fcvm filesystem, snapshot, clone, and container tests once a
  prepared workspace lands on a physical host.

The provider contract is accepted only when these tests pass without sleeps,
skipped assertions, or cleanup races.

## Implementation order

1. Add the workload record, state machine, cell routing, and complete local
   compute/storage providers.
2. Add transactional workspace preparation and remove runtime dependencies on
   image caches.
3. Add durable run versus `--rm`, wake-up, idle checkpointing, and stable port
   routing.
4. Add fork and stopped relocation using the local providers.
5. Run the full fault-injection and fuzz suites locally.
6. Add AWS and Kubernetes compute providers and EBS and Archil storage providers
   behind the same conformance-tested contracts.

## Non-goals

- live migration or zero-downtime host movement;
- simultaneous writers to one workspace;
- preserving open TCP connections across stop or relocation;
- a global control plane or global artifact cache across cells;
- host directory mounts;
- UDP port forwarding in the first version; and
- complete Docker API compatibility.
