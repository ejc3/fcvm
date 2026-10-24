# fcvm fleet: a GPU VM wall

Status: **design**, first PR of a stack. Nothing here is implemented yet.

A web page shows every GPU in a cluster as a live tile. The cluster runs all the time on
spot instances, uses every GPU it has, and grows and shrinks as spot capacity comes and
goes. Each tile is a Firecracker VM with
one NVIDIA GPU passed through, running a renderer whose frames stream to the browser.
Anyone watching can relaunch a tile or change what it renders, and everyone watching sees
the same state and the same button presses in the same order. The point is to show that
fcvm restores a GPU VM from a snapshot fast enough to do it on camera, next to the usual
way: about half the tiles are ordinary EC2 spot GPU instances launched from an AMI, running
the same renderer, relaunched by terminating and launching a new instance. The two kinds of
tile show the same clock, so the difference is on screen.

## What a viewer sees

**DECIDED (2026-10-22): the wall is one image.** Every GPU renders its own rectangle of a
single real-time scene, a flight through a ray-marched fractal world, so the whole wall
reads as one picture. The left half of the picture is rendered by fcvm GPUs and the right
half by EC2 GPUs, with a line between them. "Relaunch all" turns the whole picture black;
the fcvm half heals back tile by tile in seconds while the EC2 half stays dark, and each
half stamps the time its last tile came back. Viewers steer one shared camera (drag to
turn, scroll to dive) and appear in the scene as small lights; clicking a square relaunches
the GPU behind it, for everyone. The per-tile view below is the detail view, one click away.

**Hover shows the seams. DECIDED (2026-10-22).** The image reads as one picture until the
pointer is over it: then every tile boundary fades in as a faint line, and the tile under
the pointer is outlined in its half's color with a label naming the tile, its kind, its GPU,
its host and its last relaunch time. A long press does the same on touch screens, and a
focusable tile grid does it for keyboard users. The page draws these over the frames; the
renderers never draw them.

**Proof that it is live, not recorded.** Several things on the page could not exist in a
recording:

- **Your words in the world.** A viewer types a word and it is written into the scene, lit
  by the scene's sun, on every viewer's screen within a few frames (bounded length, rate
  limited per viewer). **DECIDED (2026-10-22):** every piece of viewer text is checked by
  Claude Haiku (Amazon Bedrock, in the fleet account) before the coordinator accepts it;
  text the check refuses, or any text while the check cannot run, never reaches the scene
  (fail closed). The check's verdict is part of the intent's receipt.
- **The time, rendered.** A clock in the world shows the current UTC time to the second,
  rendered by the GPUs from `t`, continuous across tile seams.
- **Input to pixels, measured.** After a camera drag the page shows the time from the
  viewer's input to the first frame that reflects it.
- **A stats switch.** It overlays each tile with frames per second, frame age (now minus
  the frame's `t`, against a clock the page syncs with the coordinator), the GPU's
  temperature, power draw and utilization (`nvidia-smi`, sampled by the renderer once a
  second), and the VM's name and time since its last relaunch.
- **Kill one.** Clicking a tile relaunches its VM; the square goes dark for everyone.

The detail view:

- A grid of tiles. The viewer picks how many tiles fit on the screen; that choice sets each
  tile's pixel size, and the tile asks its VM for frames at that size. Clicking a tile opens
  it large and asks for a high resolution stream.
- On each tile: the slot name, the GPU, its state (running, relaunching, failed), frames per
  second, and the time its last relaunch took, from the click to the first new frame.
- Buttons: relaunch one tile, relaunch all, and a few controls that change the scene a tile
  renders. A relaunch deletes the VM and restores a fresh one from the snapshot; the tile
  shows the clock while that happens.
- A header with the number of tiles up, the GPUs in the cluster and the last relaunch times,
  split by kind: fcvm sandbox or EC2 instance, with the median of each.
- An auto-relaunch setting: off, or every N seconds (shared by every viewer, like any other
  action). When it is on, the coordinator relaunches every tile on that schedule, fcvm and
  EC2 alike, so the two clocks keep ticking without anyone clicking.

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
- **the intent log:** every viewer action with its client-chosen id, in the order applied;
- **settings:** the auto-relaunch interval. The coordinator emits a `tick` event once a
  second carrying the time, and `apply` relaunches the tiles whose last relaunch is older
  than the interval, so the schedule is part of the replayed state, not a timer outside it.

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
others. Each group's desired capacity is 1, so up to three hosts render at once; one host is
enough to show the whole image, at coarser tiles. The account's G spot quota is 96 vCPUs per
region (64 in us-east-1), which is one g4dn.metal or one g5g.metal, so three regions give
up to three hosts; a quota increase raises it.

A host boots a fixed AMI (Ubuntu, IOMMU on, GPUs bound to vfio-pci, fcvm and its assets for
that architecture, the agent as a systemd unit), so a replacement host joins in minutes
without building anything. The agent builds the gold snapshot on first start and keeps it
on the instance store. A spot interruption notice (two minutes) makes the agent report its
slots as draining; the coordinator stops sending relaunches there.

ECS is used for the coordinator only. The GPU hosts stay plain EC2: the ECS GPU AMIs bind
the NVIDIA driver on the host, and these hosts must leave every GPU to vfio-pci.

### EC2 tiles (the comparison)

An EC2 tile is one spot instance with one GPU: g4dn.xlarge (x86, one T4) or g5g.xlarge
(arm64, one T4G), the same GPU as an fcvm slot on g4dn.metal or g5g.metal. It boots an AMI
we build (Ubuntu, the NVIDIA driver, the container toolkit, the renderer image already
pulled, a systemd unit that starts it), with nothing done at boot that the AMI could have
done, so the comparison is against EC2 at its best rather than a cold install.

The coordinator drives these tiles itself, through the EC2 API with its task role. A
relaunch launches the replacement from the same launch template first and terminates the
old instance at the same time, since nothing but the DNS name ties a tile to an instance;
the tile is running when the new renderer's health check answers. That keeps termination
out of the clock, which is EC2's best case. It needs quota for both instances at once: a
terminating instance counts against the spot vCPU quota until it is gone. Pending,
terminating and refused instances are not billed, so this churn costs only running time. The tile keeps its name, but
each instance gets a host name of its own, `<tile>-g<generation>.<zone>`, an AAAA record
for the new instance's IPv6 that the coordinator writes before it reports the tile running
and deletes once the old instance is gone. The tile's `frame_url` in fleet state names that
host, so a browser reconnecting after a relaunch resolves a name it has never cached: reusing
one name would leave a browser or resolver on the terminated address for up to the record's
TTL. The renderer gets the same wildcard certificate from Secrets Manager at boot. The
timings it reports are the parts that apply: launch requested, launch accepted, instance
running, renderer first frame. When EC2 refuses a launch for lack of spot capacity, the
coordinator retries the next AZ or instance type, and that relaunch's time includes the
wait, marked on that relaunch only; with enough quota and capacity the wait is zero.

The EC2 tiles share the fleet's regions where quota allows. The G spot quota counts their
vCPUs (4 each) against the same 96 per region that one g4dn.metal fills, so they need
either a quota increase or regions of their own; the split is about half the tiles each
way, with the fcvm side set by the GPUs its hosts have.

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
the child's PID and sees its exit. fcvm arms `PR_SET_PDEATHSIG` on the processes it spawns
(Firecracker, the namespace holder, pasta), but not on itself, so the agent arms it on each
fcvm child: `PR_SET_PDEATHSIG(SIGKILL)` in the child's `pre_exec`, then a `getppid()` check
that exits if the agent already died between fork and exec, as
`tests/common/mod.rs::set_test_pdeathsig` does for tests. With that hop, an agent that dies
takes its VMs with it, and the next agent starts from nothing.

### Slots and addressing

**DECIDED (2026-10-22): every GPU the fleet has renders, and the fleet's size sets the
granularity.** Every GPU is a slot (`slot-1` ... `slot-n`), and every slot renders. When a
host joins, the coordinator adds its GPUs as slots and re-tiles the image in one event;
every renderer gets its new rectangle through `POST /scene` without a relaunch, so the
picture changes from coarser to finer tiles at one frame `t`. When a host is reclaimed (a
two-minute spot notice) the coordinator re-tiles onto the remaining GPUs at once, before
the host disappears, so the image never shows a hole for a lost host. Slot names are
reused lowest-first, so a stable fleet keeps stable names. Both sides are re-tiled in the
same event (each side's width follows its live GPU count), as a new layout generation that
takes effect at a stated frame; the EC2 side keeps its configured number of tiles.

Within a host's life a slot keeps its address across relaunches, so a browser reconnects
to the same address. Two fcvm changes make that possible:

- `--ipv6-address <addr>` on `snapshot run` and `podman run` with `--network routed`: use
  this address instead of hashing the random VM id into the prefix. The address must lie
  in the prefix; a collision with a live VM is refused, as the hash path refuses one today.
- VM names unique among live VMs, so `--name slot-3` identifies one VM.

The slot's address is `<host prefix>::<gpu index + 2>` (fcvm refuses the prefix's
addresses 0 and 1). The host prefix is a public IPv6
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

A container in each VM, the same image everywhere. The scene is a pure function of time,
camera and the rectangle being drawn: `frame(t, camera, rect)`. Each renderer is told its
rectangle of the whole image (its place in the mosaic) and follows the camera path the
coordinator publishes against a shared clock, so all renderers draw the same frame `t`
without talking to each other. It renders with CUDA (kernels compiled ahead of time, so a
first frame needs no compile), encodes H.264 with NVENC, and serves:

- `GET /frames?h=<height>` (WebSocket): encoded frames of its rectangle at that height,
  each stamped with its `t`. The renderer draws once at the largest height anyone is
  watching and scales down for smaller ones, and encodes only the heights with a
  subscriber. The browser decodes with WebCodecs.
- `POST /scene`: the camera path, the clock offset and its rectangle (from the coordinator,
  through the agent, never from a browser), so a camera move is ordered like any other
  action.
- `GET /health`: frames per second, the heights being encoded, GPU name and memory, the
  `t` of its latest frame, and the GPU's temperature, power draw and utilization.

Scene inputs also include the viewers' words (placed in the world by the coordinator) and
the dropped lights; both are part of the camera-and-scene state every renderer follows.

**Convergence. DECIDED (2026-10-22).** A path-traced frame averages a few random light
paths per pixel and is grainy; averaging the same pixel over many frames removes the grain,
but only while nothing in the view changes. So the camera path holds at viewpoints for a
few seconds, where each tile converges toward noise-free and a relaunched tile shows as a
grainy patch healing in; and while the camera moves, each renderer reuses its previous
frames' samples by reprojection (temporal accumulation), so motion is clean too.

**No seams.** The browser shows every tile at one common `t`, holding each stream in a
short buffer (a few frames) until all visible tiles have that frame; a tile that falls
behind shows its last frame dimmed rather than tearing the image. A relaunched tile is
dark until its first frame at the current `t`.

**Detail follows the rectangle.** Each renderer has a fixed budget per frame (pixels times
ray-march steps at its target frame rate) and spends all of it on its own rectangle, so a
smaller rectangle gets more detail: more steps, deeper fractal iterations, more samples per
pixel. More GPUs in the fleet means smaller rectangles and a finer whole image. Detail per
unit of scene is that budget divided by the scene area a GPU covers. **DECIDED
(2026-10-22):** opening a tile
magnifies the same rectangle: the scene area is unchanged, so the detail per unit of scene
is too; the GPU spends the budget on more pixels and fewer steps per pixel, so the region
is drawn larger and sharper. A view that showed more of the scene through one GPU would
spread the same budget over more area and lose detail, so the opened view never widens
the rectangle.

**Two ways to receive the image. DECIDED (2026-10-22).** Each viewer chooses (the page
suggests one from its measured bandwidth); the choice is per viewer, not shared.

- **Direct:** one stream per tile, straight from its VM, at a quality that keeps the total
  within the viewer's bandwidth (about 100 lower-quality streams for a full wall).
- **Aggregated:** an aggregator (`fcvm fleet aggregate`) pulls tiles at near-lossless quality
  over the host's internal network, joins them at one common `t`, and encodes the joined
  region once, so there are no tile edges inside it and the viewer receives a few
  higher-quality streams. One aggregator runs on each GPU host for that host's tiles (the
  coordinator places a host's tiles as one contiguous region), and one serves the EC2 half.

Hover outlines and labels, dark tiles, phases and clocks are drawn by the page from the
shared state in both modes, so they behave identically. Switching never hitches: the page
opens the other mode's streams in the background, waits until it has a decoded frame for
every region at a `t` it is already showing, swaps in one draw, and only then closes the
old streams.

**Resolution follows the viewer.** The page asks each tile for the height its rectangle
occupies on that screen: a 4K screen showing 144 tiles asks for about 240 px each, the
detail view for 1080p.

**The image is fixed; its tiling follows the fleet.** The picture is always the same
frame of the same scene. The coordinator splits each half into as many rectangles as that
half has GPUs, so more hosts make each GPU's rectangle smaller and the whole image finer
(more pixels and more ray-marching steps per pixel in total). **DECIDED (2026-10-23, replacing "the halves are symmetrical"): each side shows the GPUs it
has live.** The fcvm and EC2 sides no longer mirror each other: each is as wide as its
share of the live GPUs (fcvm 100 and EC2 12 give the fcvm side 100/112 of the width), one
rectangle per GPU, never more, and no dark "waiting for capacity" tiles. An EC2 tile still
waiting for spot capacity is not on the wall; its wait is shown in its own panel and kept
out of the EC2 side's race time. A side with no GPU has no width, and with none on either
side (asleep) there is no tile. A change of capacity re-tiles without a hitch: the
coordinator publishes a new layout generation with the frame it takes effect at, each
renderer switches to its new rectangle at that frame, and the page keeps the old tiles on
screen until every new tile's first frame has arrived, then swaps them all at once. A GPU
keeps its slot and addresses; only its rectangle changes. Every viewer sees the same
tiling, because the tiling is part of the shared state (fleet/PROTOCOL.md, `layout`).

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

## Decisions from the second design review (2026-10-22)

- **The race is the permanent hero. DECIDED.** A global relaunch-all runs on a cadence only
  the owner sets; every visitor sees a large countdown ("next restart in 0:41") and a
  two-bar scoreboard, fcvm against EC2, that is always on screen. Viewers may press
  "restart now" under a global cooldown shown to everyone. This replaces the
  viewer-changeable auto-relaunch setting.
- **Only measured numbers. DECIDED.** Every figure on the page comes from the coordinator's
  measurements, timed for both halves by the same clock (click to first frame seen by the
  coordinator), and says how many GPUs it was measured on.
- **EC2 capacity waits are shown, not counted. DECIDED.** A wait for spot capacity is drawn
  hatched and labelled "AWS had no spot GPU", and kept out of EC2's headline time.
- **Directed camera with nudges. DECIDED.** The camera flies a choreographed path with
  scenic holds, where the path-traced image converges; viewers vote or queue where the
  next hold goes instead of steering continuously.
- **Your own GPU. DECIDED.** Any viewer can claim a GPU: a private VM restores for them in
  seconds, with their own camera, full screen, timed from click to first frame, for 60
  seconds. The GPU is borrowed from the wall: the claimed tile visibly leaves the image,
  labelled as a visitor's private GPU, and when the session ends its VM is destroyed and
  the tile's VM restores back into the wall. One claim per address at a time, with a
  cooldown; a cap on how many tiles may be borrowed at once keeps the wall mostly whole.
- **The gpu profile uses NVIDIA's proprietary kernel modules with the GPU firmware off on
  x86 (#1149). DECIDED.** GPU setup takes about 0.58 s instead of 1.07 s with the open
  modules; the profile setting keeps `open` available for GPUs the proprietary modules do
  not support.

## Decisions from the sandbox landscape review (2026-10-23)

From a survey of sandbox and GPU-cloud providers (no one else publishes restoring a whole GPU
VM from a snapshot with the GPU attached at restore; almost no one publishes how GPU memory is
cleared between customers). DECIDED, all owner-approved:

- **GPU memory is scrubbed twice:** inside each new VM before its workload runs (fails
  closed), and on the host before a GPU returns to the pool. A GPU whose reset or scrub fails
  is quarantined and the wall re-tiles around it. The residue test and both scrub times are
  published.
- **A standby pool:** GPUs beyond `max_tiles` stay reset, scrubbed and bound, ready for a
  failed or reclaimed tile or a claim.
- **The wall counts:** a running count of GPU VMs restored and GPUs scrubbed, and a short,
  sourced "How it's isolated" panel (own kernel per VM, GPU behind the IOMMU, scrubbed between
  VMs).
- **Warm models:** a GPU snapshot that brings a model back already resident in GPU memory
  (CUDA state moved to RAM with NVIDIA's cuda-checkpoint before the snapshot), measured as time
  to first token.
- **Tail latencies:** p50, p95 and p99 under a named external benchmark, and for GPU restores.

## Scale to zero (DECIDED 2026-10-23)

With no active viewers for 10 minutes (a hidden tab paused after 5 minutes does not count),
the coordinator scales the GPU host groups to zero and terminates the EC2 tiles; only the
coordinator stays up (about $18 a month). The first viewer to arrive wakes the fleet: the
page shows the poster frame and "Waking up the GPUs" with a live clock and the real wake
phases, and the measured wake time (first viewer to complete image) is recorded. The fleet
never sleeps during a race or a "Your own GPU" session. Running cost is then about $5 an hour
while anyone watches (one g4dn.metal at current spot prices, plus EC2 tiles while they run).

**The wake reel. DECIDED (2026-10-23).** While the fleet is asleep or waking, the stage plays a
60 to 90 second screen recording of the real wall in use (real GPUs, real coordinator, real
EC2 instances): the planet with hover showing the seams, a restart-all race with the fcvm
half healing while EC2 stays dark, the 80 km hold, and a "Your own GPU" borrow. A badge says
when it was recorded and that the live wall is waking; the race plays in real time, and any
compressed stretch is marked as skipped while the real clock keeps running. When the live
image is complete the page crossfades to it. The reel is recorded during the end-to-end run
and re-recorded whenever the measured numbers change.

## Stack of PRs

1. This document.
2. The GPU stack (#1129, #1130, #1131), merged once it passes on a GPU.
3. fcvm: `--ipv6-address` and unique live VM names.
4. Renderer image (`fleet/renderer`): one scene, NVENC, the three routes; tested on a GPU box.
5. Fleet core (`fleet/core`, Rust): state, events, `apply`, protocol types, test vectors.
6. Agent: inventory, reconcile, reports; tested against a fake `fcvm`.
7. Coordinator: browser routes, intents and receipts, agents over stdio and ssh.
8. Web: client package and pages; local mode first.
8a. EC2 tiles: the AMI, the coordinator's EC2 driver, the timings.
9. Infrastructure (ejc3/aws): the coordinator service, the regional host groups, the AMI
   pipeline, DNS and certificates, and the staging deploy.

## The target

Relaunch all, click to the last tile's first frame, under 3 s on the fcvm half. The
phases a GPU relaunch spends time in are teardown, restore, GPU attach (driver load and
the GPU's firmware boot), container start and first frame; each is measured, and the
largest is attacked first. A GPU VM cannot be snapshotted after it warms up (the device's
state is not saved), so the driver and first-frame work happen on every relaunch and are
where the work is.

## The launch event: 100 GPUs

Day to day the fleet runs on whatever spot capacity gives it. For the announcement, about
100 GPU VMs in one image: 50 fcvm GPUs (seven g4dn.metal, 56 GPUs) against 50 EC2 GPUs
(50 g4dn.xlarge), relaunched all at once and recorded in one take. At current spot prices
an hour costs about $28 for the fcvm half and $13 for the EC2 half, and it needs about 900
vCPUs of G spot quota across the fleet regions.

**DECIDED (2026-10-22): the wall never asks more of a browser than it can play.** Every
tile streams straight from its own VM, with no compositor in between, so the number of
tiles is capped by what a browser can decode and draw: `max_tiles` in the coordinator,
100 until the browser measurement below sets it. A browser holds 200 to 255 WebSockets
(Chrome's socket pool caps WebSockets at 255, Firefox defaults to 200), so connections are
not the limit at 100; the open question is 100 concurrent decoders, which the measurement
answers. Each side shows at most `max_tiles` tiles; fcvm GPUs beyond that are standby.

## Measurements to take

None of these exist yet; each is taken on a parallel box, never on a shared host:

- what a browser can play: N synthetic 128x72 H.264 streams decoded with WebCodecs into one
  canvas, N = 25, 50, 100, 200, with dropped frames, frame time and CPU, in Chrome and
  Safari on a laptop and on a phone; this sets `max_tiles`;

- relaunch time from click to first new frame, per slot, and its parts (teardown, restore,
  GPU attach, container start, first frame);
- renderer frames per second at each height on a T4;
- relaunch-all time across the fleet, click to the last first frame, and which phase of
  which slot was the last to finish.

## Open questions

1. **Scene. DECIDED (2026-10-22).** One world shared by every tile, each GPU rendering its
   rectangle (see "What a viewer sees"): a procedurally generated planet, path
   traced (terrain and megastructures by ray marching, volumetric clouds, atmospheric
   scattering, a moving sun), about 92 megapixels per frame at 100 tiles of 1280x720. Two
   properties are required of any scene: every pixel is a pure function of time, camera
   and pixel position, so a relaunched GPU rejoins at the current frame with no state to
   catch up and tile edges match exactly; and while the camera holds still each GPU keeps
   adding samples, so the image converges to near-photoreal and a relaunched tile shows as
   a noisy patch healing. Viewers share the camera, the time of day, and lights they drop
   into the world. A scene with shared simulation state (particles, fluid) across tiles is
   ruled out: it needs the GPUs to agree on state and a relaunched GPU could not rejoin
   at once.
2. **Hardware.** Recommended: g4dn.metal, eight T4 per host and NVENC on each, so one host
   fills a grid. g5g.metal has two T4G per host at a sixth of the price.
3. **Zone. DECIDED (2026-10-22): `fcvm.dev`**, bought through Cloudflare Registrar so the
   zone is in Cloudflare from the start. The site at the
   apex, slots at `slot-<n>.fcvm.dev`, the coordinator at `fleet-api.fcvm.dev`. `.dev` is
   HTTPS-only in browsers (HSTS preload), which this design meets everywhere.
4. **Quota.** Three regions at the current G spot quota give at most three hosts, and the
   EC2 tiles compete for the same quota. Raising it in each region allows more hosts and
   room for the EC2 half.
