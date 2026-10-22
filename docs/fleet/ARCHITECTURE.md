# fcvm fleet: a GPU VM wall

Status: **design**, first PR of a stack. Nothing here is implemented yet.

A web page shows every GPU in a cluster as a live tile. The cluster runs all the time on
spot instances, uses every GPU it has, and grows and shrinks as spot capacity comes and
goes. Each tile is a Firecracker VM with
one NVIDIA GPU passed through, running a renderer whose frames stream to the browser.
Anyone watching can relaunch a tile or change what it renders, and everyone watching sees
the same state and the same button presses in the same order. The point is to show that
fcvm restores a GPU VM from a snapshot fast enough to do it on camera.

## What a viewer sees

- A grid of tiles. The viewer picks how many tiles fit on the screen; that choice sets each
  tile's pixel size, and the tile asks its VM for frames at that size. Clicking a tile opens
  it large and asks for a high resolution stream.
- On each tile: the slot name, the GPU, its state (running, relaunching, failed), frames per
  second, and the time its last relaunch took, from the click to the first new frame.
- Buttons: relaunch one tile, relaunch all, and a few controls that change the scene a tile
  renders. A relaunch deletes the VM and restores a fresh one from the snapshot; the tile
  shows the clock while that happens.
- A header with the number of tiles up, the GPUs in the cluster and the last relaunch times.

## Components

```
browser (Next.js on Cloudflare Workers)
   |  wss  state + intents                 |  wss  frames, one socket per visible tile,
   v                                       |  straight to the VM over IPv6, TLS in the VM
coordinator (Rust, `fcvm fleet coord`)     v
  ECS Fargate service, one task          slot-<n>.<zone>  AAAA -> the VM's own address
   ^  wss, agents dial out                 |
   |                                       v
agent (Rust, `fcvm fleet agent`)         renderer in the slot's VM (container, GPU)
  one per GPU host: spot g4dn.metal / g5g.metal in auto scaling groups, several regions
   |  spawns, supervises, tears down
   v
fcvm snapshot run --snapshot <gold> --gpu <sbdf> --network routed --ipv6-address <slot addr>
```

Locally the same binaries run on one machine: the coordinator starts the agent as a child
process and talks to it over stdio, and `ssh <host> fcvm fleet agent --stdio` does the same
for a handful of hosts without any cloud pieces.

### Coordinator

The single authority for fleet state. It holds, in memory and in a small JSON file it
rewrites atomically on every change:

- **desired state:** the slots (name, host, GPU address, IPv6 address, scene parameters)
  and each slot's generation, a counter a relaunch increments;
- **observed state:** what each agent last reported per slot (phase, VM id, timings,
  frames per second, the failure text if it failed);
- **the intent log:** every viewer action with its client-chosen id, in the order applied.

It runs as one ECS Fargate task in a cluster dedicated to this project. Its JSON state
file lives in S3 (rewritten whole on each change, read once at start), so a replaced task
resumes with the same slots and generations. It serves two WebSocket routes to browsers: `/live` (one snapshot of the whole fleet on
connect, then a new snapshot whenever the version moves; a client ignores a snapshot older
than the last it accepted) and the intents a browser sends on the same socket
(`intent {id, name, args}`, answered by `receipt {id, outcome, version}`). An intent id seen
before returns the original receipt, so a retry after a dropped socket applies once. This
is the ordering model of the Orbit game server and the Lattice receipts in the
constellation repository, rebuilt here in Rust with no database.

Every state change goes through one pure function, `apply(state, event) -> state`, where an
event is either an accepted intent or an agent report. The function has no clock, no I/O and
no randomness; time arrives inside the event. That function is the multiplayer engine: two
viewers see the same thing because there is one sequence of events and one function over
it, and its tests are replayed vectors.

Agents dial the coordinator (`wss://fleet-api.<zone>/agent`, through a Cloudflare tunnel
from the Fargate task, authenticated by an Access service token the agent reads from
Secrets Manager with its instance role). Spot hosts in any region reach it outbound and no
agent listens on a port. The messages are the same newline-delimited JSON as the local
stdio and ssh transports, so the three transports share one protocol and one test suite.

### Hosts and capacity

The GPU hosts are spot instances in EC2 auto scaling groups, one group per region, each
with a mixed instances policy over both bare-metal GPU types (g4dn.metal: x86, eight T4;
g5g.metal: arm64, two T4G; fcvm needs `/dev/kvm`, so nothing smaller qualifies), every AZ
the region offers, and the `price-capacity-optimized` allocation strategy. The groups run
in us-east-2, us-west-2 and us-east-1, so one region losing spot capacity leaves the
others. Each group's desired capacity is 1. The account's G spot quota is 96 vCPUs per
region (64 in us-east-1), which is one g4dn.metal or one g5g.metal, so three regions give
up to three hosts; a quota increase raises it.

A host boots a fixed AMI (Ubuntu, IOMMU on, GPUs bound to vfio-pci, fcvm and its assets for
that architecture, the agent as a systemd unit), so a replacement host joins in minutes
without building anything. The agent builds the gold snapshot on first start and keeps it
on the instance store. A spot interruption notice (two minutes) makes the agent report its
slots as draining; the coordinator stops sending relaunches there.

ECS is used for the coordinator only. The GPU hosts stay plain EC2: the ECS GPU AMIs bind
the NVIDIA driver on the host, and these hosts must leave every GPU to vfio-pci.

### Agent

One per GPU host. It reports the GPUs it has, is told the slots its host owns and their
desired generation, and makes the host match:

- **Inventory.** At start it lists the NVIDIA functions bound to vfio-pci and reports
  them. A slot names one of them; two slots naming one GPU is refused by the coordinator.
- **Reconcile.** For each slot, if the running VM's generation is below the desired one, it
  tears the VM down, waits until the old Firecracker process has exited and the slot's
  address and GPU are free, then restores a new VM from the gold snapshot with the slot's
  name, GPU and address. One relaunch per slot at a time; a newer generation that arrives
  mid-relaunch waits for the current one to finish and then runs once.
- **Relaunch all, as fast as the hosts allow.** "Relaunch all" is one intent that bumps
  every slot's generation in one event. Each agent then relaunches all of its slots at
  once, not one after another: teardowns run in parallel, and restores start as each
  slot's old VM has exited. The snapshot is served to every clone from one memory server
  per host, so the restores share the page cache. The header shows the time from the click
  to the last tile's first frame, and that number is the one the demo is tuned for.
- **Report.** Phase changes (`stopping`, `restoring`, `attaching_gpu`, `starting`,
  `running`, `failed`), the timings fcvm logs for the restore (including `gpu_ms` from the
  restore acknowledgement), and the renderer's own health, read from its control port.
- **Fail closed.** A VM that dies is reported and relaunched with backoff (1 s, doubling,
  capped at 30 s), never in a loop.

The agent drives fcvm through its CLI, one `fcvm snapshot run` child per slot, so it owns
the child's PID and sees its exit. fcvm already arms `PR_SET_PDEATHSIG` on every hop, so an
agent that dies takes its VMs with it, and the next agent starts from nothing.

### Slots and addressing

Every GPU the fleet has is a slot; the coordinator creates slots when a host joins and
retires them when it leaves, so the grid always shows the whole capacity. Slots have
stable logical names (`slot-1` ... `slot-n`); a name is reused for the next GPU that needs
one, so the grid keeps its order when a host is replaced. Within a host's life a slot keeps
its address across relaunches, so a browser reconnects to the same address. Two fcvm
changes make that possible:

- `--ipv6-address <addr>` on `snapshot run` and `podman run` with `--network routed`: use
  this address instead of hashing the random VM id into the prefix. The address must lie
  in the prefix; a collision with a live VM is refused, as the hash path refuses one today.
- VM names unique among live VMs, so `--name slot-3` identifies one VM.

The slot's address is `<host prefix>::<gpu index + 1>`. The host prefix is a public IPv6
/80 that AWS delegates to the host's network interface at launch (`Ipv6PrefixCount = 1` in
the launch template). The browser connects straight to the VM: `slot-<n>.<zone>` is an
unproxied AAAA record pointing at the slot's address, which the coordinator writes through
the Cloudflare API when a slot is placed on a host (TTL 60 s). The host's security group
allows TCP 443 from anywhere over IPv6.

TLS terminates in the VM. The coordinator keeps one wildcard certificate for
`*.<fleet zone>` from Let's Encrypt (DNS-01 through the Cloudflare API, renewed 30 days
before expiry) in Secrets Manager; the agent reads it with its instance role and hands it to
each renderer read-only. Every VM holds the wildcard key, which is acceptable because a
viewer can only send scene parameters, never code; per-slot certificates are the follow-up
if that changes.

A viewer with no IPv6 cannot reach a VM directly. For them each slot also has a proxied
name, `slot-<n>-v4.<zone>`, through Cloudflare (which reaches the VM over IPv6 and checks its
certificate); the page uses it only when the direct address fails.

Locally there is no Cloudflare: the page connects to the slot's address directly.

### Renderer (the workload)

A container in each VM, the same image everywhere. It renders a scene on the GPU with CUDA,
encodes H.264 with NVENC, and serves:

- `GET /frames?h=<height>` (WebSocket): encoded frames at that height. The renderer draws
  once at the largest height anyone is watching and scales down for smaller ones, and it
  encodes only the heights that have a subscriber. The browser decodes with WebCodecs.
- `POST /scene`: scene parameters (from the coordinator, through the agent, never from a
  browser directly), so a scene change is ordered like any other action.
- `GET /health`: frames per second, the heights being encoded, GPU name and memory.

The first scene is recommended below; the renderer is built so a scene is one module.

### Web

Next.js on Cloudflare Workers (OpenNext), following the house style: the rules live in a
TypeScript client package (snapshots in, view models out, every visible word composed
there), and React only draws view models and sends actions. The page holds two kinds of
connection: one to the coordinator for state and intents, and one per visible tile for
frames. When a tile's VM relaunches its frame socket closes, and the tile reconnects to the
same URL once the coordinator reports the slot running again; no state is resynced.

Staging is `<app>-stage` behind Cloudflare Access. A public URL needs the owner's go-ahead.

## Invariants (each becomes a test)

1. At most one VM runs per slot, and at most one slot holds a GPU.
2. A relaunch starts only after the slot's previous Firecracker process has exited.
3. A slot's address, name and GPU are the same after every relaunch.
4. An intent id is applied at most once; a repeated id returns its first receipt.
5. Every viewer that has accepted version v has the same state for v.
6. A browser never sends a scene change to a renderer; only the agent does.
7. An agent that loses its coordinator, and a coordinator that loses an agent, leave no VM
   running that nothing supervises.

## Infrastructure

All of it is Terraform in ejc3/aws, applied by the jumpbox:

- **Network:** one VPC per region with an IPv6 /56, public subnets in every AZ, and a
  security group allowing TCP 443 from `::/0` and nothing else inbound (no SSH; SSM for
  access).
- **Hosts:** a launch template per region and architecture (the AMI, IOMMU kernel
  arguments, `Ipv6PrefixCount = 1`, the instance role), and one spot auto scaling group per
  region with the mixed instances policy above.
- **AMI:** built by a pipeline from the fcvm release (fcvm, the gpu kernel and Firecracker,
  the driver image), one per architecture, copied to each region.
- **Coordinator:** a dedicated ECS cluster, one Fargate service with the coordinator and a
  `cloudflared` container, its S3 state object, and the Secrets Manager entries (the
  certificate, the Cloudflare DNS token, the Access service token for agents).
- **Cloudflare:** the tunnel to `fleet-api.<zone>`, the Access application for staging, the
  Worker for the site.
- **Testing before the fleet exists:** `pbox up N kvm-gpu-96 --open` gives one host with a
  delegated prefix, and the local transports drive it.

## Stack of PRs

1. This document.
2. The GPU stack (#1129, #1130, #1131), merged once it passes on a GPU.
3. fcvm: `--ipv6-address` and unique live VM names.
4. Renderer image (`fleet/renderer`): one scene, NVENC, the three routes; tested on a GPU box.
5. Fleet core (`fleet/core`, Rust): state, events, `apply`, protocol types, test vectors.
6. Agent: inventory, reconcile, reports; tested against a fake `fcvm`.
7. Coordinator: browser routes, intents and receipts, agents over stdio and ssh.
8. Web: client package and pages; local mode first.
9. Infrastructure (ejc3/aws): the coordinator service, the regional host groups, the AMI
   pipeline, DNS and certificates, and the staging deploy.

## Measurements to take

None of these exist yet; each is taken on a parallel box, never on a shared host:

- relaunch time from click to first new frame, per slot, and its parts (teardown, restore,
  GPU attach, container start, first frame);
- renderer frames per second at each height on a T4;
- relaunch-all time across the fleet, click to the last first frame, and which phase of
  which slot was the last to finish.

## Open questions

1. **Scene.** Recommended: a ray-marched fractal fly-through, one seed and palette per
   slot, because it is GPU-bound, looks different on every tile and needs no assets.
   Alternatives: a fluid simulation viewers can stir; an image model generating frames.
2. **Hardware.** Recommended: g4dn.metal, eight T4 per host and NVENC on each, so one host
   fills a grid. g5g.metal has two T4G per host at a sixth of the price.
3. **Zone.** Recommended: `fcvm.dev`, unregistered when checked on 2026-10-22, bought
   through Cloudflare Registrar so the zone is in Cloudflare from the start. The site at the
   apex, slots at `slot-<n>.fcvm.dev`, the coordinator at `fleet-api.fcvm.dev`. `.dev` is
   HTTPS-only in browsers (HSTS preload), which this design meets everywhere.
4. **Quota.** Three regions at the current G spot quota give at most three hosts. Raising
   it in each region allows more hosts per region.
