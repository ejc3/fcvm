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
- **A stats switch.** It overlays each tile with frames per second, frame age (when the
  page drew the frame minus its `t`, against a clock the page syncs with the coordinator,
  with when the frame arrived and when it was decoded), the GPU's
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
  one per GPU host: spot g4dn.metal / g5g.metal the coordinator launches, several regions
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

The GPU hosts are spot instances the coordinator launches itself with EC2 Fleet
(`CreateFleet`, type `instant`), one request per region over both bare-metal GPU types'
launch templates (g4dn.metal: x86, eight T4; g5g.metal: arm64, two T4G; fcvm needs
`/dev/kvm`, so nothing smaller qualifies), every AZ the region offers a host type in, and
the `price-capacity-optimized` allocation strategy. When spot is refused for capacity the
same pass launches on demand, at most 8 on-demand hosts across the fleet
(fleet/PROTOCOL.md, `--host-template`). The regions are us-east-2, us-west-2 and us-east-1,
so one region losing spot capacity leaves the others. Each region runs 1 host while the
fleet is awake, so up to three hosts render at once; one host is
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
same event, as a new layout generation that takes effect at a stated frame; each side keeps
its configured number of places, and a place with no GPU is dark.

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
but only while nothing in the view changes. While the camera moves, each renderer reuses
its previous frames' samples by reprojection (temporal accumulation) and denoises what is
left, so motion is clean too. REVISED (2026-10-27): the directed camera no longer holds
still. Stopping dead at every viewpoint read as the motion dying, so a stop is a slow move
(an orbit, a push-in, a drift) that the reprojected history follows, and the renderer's
exact accumulation is left to a claimed GPU whose viewer stops steering.

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
shared state in both modes, so they behave identically. A third way, one CDN stream of the whole wall
for viewers beyond what direct streaming serves, is "Broadcast mode" below. Switching never hitches: the page
opens the other mode's streams in the background, waits until it has a decoded frame for
every region at a `t` it is already showing, swaps in one draw, and only then closes the
old streams.

**Resolution follows the viewer.** The page asks each tile for the height its rectangle
occupies on that screen: a 4K screen showing 144 tiles asks for about 240 px each, the
detail view for 1080p.

**The image is fixed; its tiling follows the fleet.** The picture is always the same
frame of the same scene. The coordinator splits each half into as many rectangles as that
half has GPUs, so more hosts make each GPU's rectangle smaller and the whole image finer
(more pixels and more ray-marching steps per pixel in total). **DECIDED (2026-10-26, replacing "each side shows the GPUs it has live" of 2026-10-23):
each side is sized by its configured GPUs.** The fcvm side has `--fcvm-tiles` places (or
one per live fcvm GPU when there are more) and the EC2 side `--ec2-tiles`; each is as wide
as its share of the places (8 and 8 give two halves, fcvm 100 and EC2 12 give the fcvm side
100/112 of the width). A place with no live renderer keeps its rectangle and is drawn dark
with its phase: an EC2 tile waiting for spot capacity or for its new instance in a race, an
fcvm place whose host has not connected or was reclaimed. Sizing the sides by the live GPUs
re-tiled the wall in the middle of every race, when the EC2 instances are replaced, so the
dark tiles the race counts up vanished. A capacity wait is kept out of the EC2 side's race
time. With no places on either side (asleep) there is no tile. A change of capacity re-tiles without a hitch: the
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

## Broadcast mode

Every direct or aggregated viewer opens streams to the fleet's own machines, so the fleet
serves a bounded number of them: about 500 at once on the wall as deployed (the fcvm half
fails first, at 750 viewers per tile, its renderer VM's 2 vCPUs binding first;
fleet/renderer/README.md, "Viewers per renderer"). Beyond that, viewers get the
**broadcast**: the whole wall composited into one video, encoded once and delivered by
Amazon IVS. The owner, 2026-10-27: "stream the aggregate video with less controls. Things
like skywriting can still happen"; on the toggle, "it's forced on / can't change if you are
a late viewer"; on delivery, "IVS is good".

```
renderers (every placed tile, at the broadcast's height)
   |  FCVF frames over wss, the published IPv6 names
   v
broadcast aggregator (`fcvm fleet aggregate --push-url-env`), a child of the coordinator
   |  one 1280x720 H.264 stream at 30 fps, every frame's SEI carrying its scene t
   |  FLV, copied by an ffmpeg child over RTMPS
   v
Amazon IVS low-latency channel, standard type: 720p, 480p, 360p and 160p renditions
   |  HLS from IVS's CDN; a private channel, so every request carries a playback token
   v
browser: IVS web player; the overlay drawn from the state in effect at the shown frame's t
```

### Who gets the broadcast

- **`--direct-capacity N`** on the coordinator (also `FLEET_DIRECT_CAPACITY`, default 500) is
  how many viewers hold a seat: direct or aggregated streams. 500 is the measured capacity
  of the wall as deployed; a fleet whose renderer VMs have 4 vCPUs may raise it toward 750.
- **A seat** is taken by a `/live` connection admitted while fewer than N are held. The
  admission message gives it a direct token (below) and a seat ticket. The seat is held while
  the connection is open and for 60 s after it closes, so a page that reconnects with its
  ticket (`/live?seat=<ticket>`) keeps it. A hidden tab keeps its seat while its socket is
  open.
- **Every viewer has a broadcast toggle.** A seated viewer may switch to the broadcast and
  back; switching does not give up the seat.
- **A late viewer**, one that connects while all N seats are held, is admitted to the
  broadcast only. Its toggle is shown on and disabled, with the reason ("Direct streams are
  full, so you are watching the broadcast"). It never receives a direct token, and
  `claim_gpu` (a private GPU, also a direct stream) is refused to it.
- **No promotion mid-session.** A late viewer stays on the broadcast for as long as its
  connection lasts. When a seat is free the coordinator says so in the admission message and
  the page offers a reload; the new connection is admitted afresh. Promoting a viewer in the
  middle of a session would swap its picture for the direct tiles while it watched, and two
  late viewers would race for one seat; a reload makes the order the order of connections.

**Enforcement is server side.** A renderer's or aggregator's `GET /frames` needs an `admit`
token once it holds an admission key: `v1.<expiry, unix s>.<viewer>.<mac>`, the mac an
HMAC-SHA256 of the first three fields with a 32-byte key the coordinator makes at start. The
key reaches renderers in the scene the agent posts (the coordinator posts the EC2 tiles'
scenes) and aggregators in their layout, and never appears in a snapshot. A renderer
accepts the current key and the one before it, so a new key does not cut off a viewer between
two token refreshes. Tokens last 10 minutes and each seated connection gets a fresh one every
5 minutes; a token is checked when a stream opens, not while it runs. A late viewer that
flips its page into direct mode opens sockets that are refused with 403, because nothing it
holds is a token. The coordinator mints tokens for the streams it reads itself (the EC2
aggregator, the broadcast, the poster), and an agent's aggregator mints its own from the
key. A known limit: `POST /scene` answers anyone who can reach a renderer and forges a JSON
content type, so a determined client can replace the key. That is the scene's own exposure,
not the broadcast's, and is tracked separately.

**Past about 5,000 viewers the coordinator needs more.** A broadcast viewer still holds a
`/live` socket for state, intents and its tokens, and the coordinator holds about 5,000 of
them, bound by memory at about 160 KB each. Beyond that, either the task gets more memory, or
the broadcast page gets a lighter state feed: the overlay's data carried in the stream as IVS
timed metadata (it already carries `t`), with no per-viewer socket, and intents sent over
short requests.

### The composite

The coordinator runs the broadcast aggregator beside itself (`--broadcast-listen`), as it runs
the EC2 half's. Its layout (`coord::broadcast::layout`) is the whole image, every placed tile
read directly at its published IPv6 name and at the broadcast's own height, so each renderer
encodes one small extra stream for it (`q=std`) and one decoder thread keeps up with each. A
region aggregator's stream is a near-lossless region a whole image tall, which one decoder
thread does not keep up with (a 1280x720 renderer stream at `q=hi` fell 3 s behind on a
g4dn.2xlarge). The aggregator waits for every tile at each slot as a region aggregator does,
draws a tile with nothing to send black, and encodes the image once: 1280x720, libx264
`veryfast`, zero-latency, High profile, no B-frames, a keyframe every 2 s, CRF 21 capped at
4.5 Mbit/s. IVS's standard channel transcodes it into the 720p, 480p, 360p and 160p
renditions players switch between, so the fleet encodes one stream, not a ladder. 720 lines,
not 1080: a 1080p rendition costs IVS's Full HD rate per viewer-hour (twice HD), and the
encode about twice the CPU. The aggregator runs only while a tile is placed: asleep, it is
stopped and nothing is pushed, and every viewer sees the wake reel.

The aggregator writes FLV itself, so each frame's timestamp is its scene time (a skipped
slot is a gap, never a drift). It opens the stream with an `onMetaData` tag naming the size,
so the copy probes nothing (with ffmpeg's default probing a copy held frames back 0.9 s at
the median), carries a silent AAC track beside the video (without audio IVS served every
frame about 3.4 s later), and puts an SEI message before each frame's first slice: user data unregistered under a fixed UUID
(`push::SEI_UUID`), payload `{"t": <scene seconds>, "e": <scene epoch, unix ms>}`. An `ffmpeg`
child copies it to `rtmps://<ingest>:443/app/<key>`. A child that exits is started again after 1 s, doubling to 30 s, at the next
keyframe. The ingest URL comes from the broadcast secret through the environment, so the
only command line holding it is that ffmpeg's, inside the coordinator's task.

The coordinator task grows from 0.5 vCPU and 1 GB to 2 vCPU and 4 GB (ARM64 Fargate). The
composite above took 1.7 cores on the measurement box's Xeon (encode 0.9, decode of one
1280x720 source 0.5, composing and the copy the rest); 16 small tile streams decode
the same pixels.

### The overlay follows the frame shown

IVS keeps the SEI through its transcode: every rendition's segments carry one per frame
(measured: 60 in each 2 s segment of the 720p and 480p renditions). The IVS web player raises
`MetadataEventType.SEI` with the message and its presentation timestamp, which is the video
element's own media time, so the page matches each presented frame
(`requestVideoFrameCallback`'s `mediaTime`) to its `t` exactly. The page keeps the snapshots
of the last 30 s with the coordinator time each arrived at (`serverNow`, from the clock it
already syncs) and in broadcast mode draws the overlay (scoreboard, race clocks, phases, dark
tiles, labels, nudges, words) from the newest snapshot that arrived at or before the shown
frame's wall time, `e + 1000 t`. A tile that went dark at a relaunch is drawn dark when the
video shows it dark, not seconds before. The page's "now" in broadcast mode is that frame
time plus the time since the frame was presented, so the overlay's clocks tick with the
picture. The stats line shows the broadcast's latency: the synced now minus the shown frame's
wall time.

### What a broadcast viewer can do

Scene inputs that go through the coordinator stay: words (the text check is unchanged),
lights (placed with the camera of the frame shown), camera votes, and "restart now" under its
global cooldown. Each appears in the video when the composite catches up, about one broadcast
latency later, and the page marks it pending until the frame shown is past the snapshot that
holds it. Gone in broadcast mode: clicking a tile to relaunch it, the detail view, "Your own
GPU", per-tile stats and the stream choice; the broadcast toggle is the only stream control.

### Staging access

The channel is private (authorization required): IVS refuses a playback request without a
token signed by the channel's playback key pair. The coordinator holds the private key (in the
broadcast secret) and gives each `/live` connection a playback URL with a token in its
admission message: an ES384 JWT with `aws:channel-arn`, `aws:access-control-allow-origin` set
to the page's origin, and a 1-hour expiry, refreshed every 30 minutes. On staging `/live` is
reached only through Cloudflare Access, so only Access-authenticated viewers ever get a
token; the playback URL alone returns 403. A public launch keeps the tokens, with `/live`
open.

### Failure behavior

| failure | what happens |
|---|---|
| the broadcast aggregator or its ffmpeg exits | restarted after 1 s, doubling to 30 s; the push resumes at the next keyframe. The player reports buffering, the page says "the broadcast is reconnecting" over the last frame, and the overlay resyncs with the next frame's SEI |
| a tile stops | drawn black, as an aggregator draws a missing tile; the overlay's phases say why |
| the fleet sleeps | no tile placed: the aggregator stops, the channel goes offline, every viewer sees the wake reel; waking starts the push again |
| IVS refuses the stream or is down | the push retries with backoff; broadcast viewers see "the broadcast is unavailable"; late viewers are not moved to direct, which would pass the measured capacity |
| the broadcast secret cannot be read at start | the coordinator does not start (fail closed), as for the TLS secret |
| the coordinator restarts | a new admission key: open tile streams continue, new ones need the tokens each reconnected page receives; seats are taken again in the order pages reconnect |
| a frame arrives without its SEI | the overlay keeps the last matched frame time and advances it with the player's clock, and the stats line says the sync is estimated |

### Delivery: IVS or CloudFront (measured 2026-10-27)

The rig: a g4dn.2xlarge (one T4) from the fleet's tile AMI, its renderer drawing the whole
image at 1280x720 (one T4 draws 1920x1080 at only 15 fps), the broadcast aggregator from this
stack reading it, an IVS standard low-latency channel in us-east-1, and headless Chrome 156
with IVS player 1.57.0 on the same instance, so the renderer, the composite and the player
read one clock. Five cycles of 60 s, each a fresh player; the page matches each presented
frame to its SEI by media time (`fleet/tools/ivsmeasure`).

| | IVS low latency, standard channel |
|---|---|
| glass to glass: scene time to the frame displayed, every frame (7,514) | 1.79 s median, 1.87 s p95 |
| of which the composite: scene time to the encoded frame leaving the aggregator | 84 ms median, 88 ms p95 |
| IVS's own `getLiveLatency()` (its ingest to the player) | 1.62 s median, 1.81 s p95 |
| startup: `load()` to the first frame presented, on a live channel | 0.21 s median (0.18 to 0.32, 5 loads) |
| the same stream with no audio track | 5.18 s median glass to glass |
| SEI through the transcode | one per frame in every segment checked (720p and 480p renditions) |
| CPU of the composite (decode, compose, 720p encode, copy) | 1.69 cores of the box's Xeon |

Without audio IVS served every frame about 3.4 s later, so the push carries a silent AAC
track (`push::AAC_SILENCE`). CloudFront in front of a self-packaged LL-HLS origin was not
built: the owner approved IVS on 2026-10-27 ("IVS is good") and asked for the LL-HLS origin
only if IVS did not measure as expected; 1.8 s glass to glass does. Its cost and build were
compared instead:

| per 1000 viewer-hours, North America, list prices 2026-10-27 | IVS low latency | CloudFront + self-packaged LL-HLS |
|---|---|---|
| delivery at 720p | $72 (HD output, first 10,000 h a month) | $115 (3 Mbit/s, 1.35 GB at $0.085/GB) plus about $11 of requests (LL-HLS parts and playlists, about 4 a second per viewer) |
| delivery at 480p | $36 (SD output) | $57 (1.5 Mbit/s) plus requests |
| ingest and transcode | $2.00 an hour while the channel is live | our own 3-rendition encode (about 3 more vCPU) or MediaLive |
| player, CDN, timed metadata, tokens | included | to build: packager, origin, signed cookies, player integration |

The channel is live, and billed $2.00 an hour, while the fleet is awake; the fleet sleeps
after 10 minutes with no viewer. The coordinator's larger task adds about $43 a month (1.5
vCPU and 3 GB more, us-east-2). **DECIDED (2026-10-27): IVS**, on the measured latency and
the cost: it is cheaper per viewer-hour at every rendition, it keeps the per-frame SEI
through its transcode, and it has nothing to run.

### Stack

1. This section and the measurement page (`fleet/tools/ivsmeasure`).
2. The push (`fcvm fleet aggregate --push-url-env`: FLV with scene-time stamps, SEI and the
   silent track) and the coordinator's broadcast aggregator (`--broadcast-listen`).
3. Admission: seats, `--direct-capacity`, direct and playback tokens in an `admission`
   message, `claim_gpu` refused to late viewers.
4. Enforcement: renderers and aggregators refuse `GET /frames` without a valid direct token.
5. The page: the toggle, the IVS player, the overlay synced by `t`, the reduced controls.
6. ejc3/aws: the IVS channel, its playback key pair, the broadcast secret, the coordinator's
   larger task and its flags.

## Phones mode

Status: **design**, from a feasibility spike on a g4dn.metal (2026-10-27). The first PR of
its stack is the phone VM profile and its test (`make phone-test`). The owner, 2026-10-27:
"I want to stand up Android phones in this environment that have access to the gpu. So by
default it's still this seamless view, but then you can 'zoom out' and the phone is revealed
running the simulation in an app ... If you take over the phone, we give predefined actions
you can take, not free form input. Have this as alternative mode with a query param for now.
Need to pass through the gpu all the way to the emulator." Later the same day: "Android has
snapshot capability now", "We can change the emulator to support attach", "Fix firecracker
if it doesn't support what we need out of the box", and "The scene we are rendering on the
phone, let's have something more androidy."

### What a viewer sees

- `?mode=phones` opens it; without the parameter the wall is unchanged.
- **Seamless by default.** Every phone renders its rectangle of one Android-themed scene
  (below), and the page stitches them with the same `t` sync and seam rules as the wall.
- **Zoom out.** The image animates apart into phone frames: the page draws a bezel around
  each tile and the phone shows its whole screen, the app's own interface around the scene.
  The app has two tabs, the scene and an info screen: device model, Android version, the
  GLES renderer string of its own context (the GPU's name), frames per second, uptime, the
  fcvm VM it runs in (slot, GPU, its last restore time) and the attribution lines.
- **Restore time.** A phone shows "Phone restored in X s" the way a wall tile shows its
  restore, measured from the click to the app's first frame on the restored GPU.
- **Take over.** A viewer borrows one phone like "Your own GPU" and gets a panel of fixed
  actions; nothing typed reaches the phone.

### The phone

```
fcvm VM, `phone` profile: one GPU (VFIO), KVM inside (nested), restored from a snapshot
  container fleet/phone (privileged: /dev/kvm; the GPU as CDI devices)
    Android Emulator: QEMU with KVM, Android 15 (AOSP x86_64 image)
      gfxstream, the emulator's host-side renderer: EGL-on-EGL through glvnd to NVIDIA's
      EGL (no X display), and Vulkan through NVIDIA's ICD, both on the passed-through GPU
    the app (guest GLES 3 / Vulkan through goldfish -> gfxstream -> the GPU)
    phone streamer: the screen -> NVENC H.264 -> FCVF frames over wss, stamped with t
```

Phones run on the x86 hosts (g4dn.metal): the emulator's x86_64 Android needs x86 KVM, and
the fleet's host AMI already loads `kvm_intel` with `nested=Y`. The arm64 hosts keep
rendering the wall.

The spike rig: g4dn.metal (T4, driver 580.196.07), host `kvm_intel nested=Y`, emulator
37.2.12, the AOSP `default` x86_64 Android 15 image, a 720x1280 phone with 4 cores. Every
number below is from that box.

| measurement | result |
|---|---|
| renderer SurfaceFlinger composes with, and the app's own GLES context | `Android Emulator OpenGL ES Translator (Tesla T4/PCIe/SSE2)`, `OpenGL ES 3.2 NVIDIA 580.196.07` |
| the emulator's Vulkan device | Tesla T4, Vulkan 1.4.312 |
| cold boot to `sys.boot_completed` | 26.5 s (16 GiB VM, 4 GiB phone); 30.6 s (6 GiB VM, 2 GiB phone) |
| probe app: 25,600 instanced, lit cubes, GLES 3 | 60.0 fps (the display's vsync), the emulator using 64% of one core; the T4 19% utilized at a 360 MHz graphics clock, 15 W |
| app start (`am start -W`) | 372 ms |
| emulator snapshot save with the app running | 3.9 s (4 GiB phone: 1.8 GB RAM image, 3.3 MB of textures); 4.8 s (2 GiB phone) |
| emulator snapshot load into a new emulator process, same VM: adb up, app's next frame | 5.4 s and 5.7 s with a cold cache; 4.0 s and 4.2 s warm |
| of a warm load: renderer up; RAM image; GPU state (framebuffer, 48 color buffers, 9 process resources) | 0.66 s; 1.60 to 2.94 s; 3.00 to 3.42 s |
| clone restore of a phone VM, `fcvm snapshot run --gpu`, 16 GiB | ACK 10.2 and 12.3 s, of which the memory load 9.06 s |
| the same, 6 GiB, three clones on three GPUs | ACK 4.72 to 4.99 s, of which the memory load 3.80 to 4.08 s and `gpu_ms` 0.86 to 0.87 s |
| flow A: clone restore, then the emulator loads its own snapshot; click to the app's first frame | 12.8 to 13.1 s (6 GiB, three clones; the load took 7.9 to 8.1 s in the fresh clone) |
| `fcvm snapshot create --gpu-warm` with the emulator holding the GPU | refused: "every process with a GPU open must be a CUDA process"; the park is undone and the phone keeps drawing |
| KVM in a restored VM (`fleet/phone/kvmprobe.c`), Firecracker without nested state | the guest's kernel oopses in `kvm_spurious_fault`, the probe dies with SIGSEGV; with the fork, `kvm ok` |

The memory load is the VFIO pin: a VM with a passed-through device has all its memory
faulted in and pinned for DMA at restore, about 0.65 s per GiB here, so the phone VM is
sized small (6 GiB for a 2 GiB phone) and hugepages are the next thing to measure.

**Headless host rendering.** The emulator's `-gpu host` on Linux opens GLX and fails with
no X display. gfxstream's EGL-on-EGL engine (`ANDROID_EGL_ON_EGL=1`) renders through EGL
instead, and NVIDIA's EGL works headless. The SDK's emulator then loads SwiftShader's
`libEGL.so` from its own directory before the system's, which renders in software; the spike
masks those files inside the container (`phone.sh`, a bind mount over them; nothing in the
SDK changes), and the emulator built from source (below) does not ship them.

**Paths considered.** In the order the task gave:

1. *The Android Emulator in the fcvm VM.* Works, above. **Chosen.**
2. *Cuttlefish with gfxstream.* The same renderer behind crosvm, also on nested KVM, and it
   needs vhost-vsock and vhost-net in the guest and its own host packages. Not spiked: path
   1 met every requirement and Cuttlefish changes nothing about the GPU path. It is the
   fallback if the emulator's QEMU side blocks the detach work.
3. *No KVM: Android in a container on the guest kernel (Redroid).* Ruled out. Android
   userspace has no GPU driver for a T4: NVIDIA ships Android drivers only for its Tegra
   parts, Mesa's NVIDIA drivers do not run on the proprietary kernel driver the VM loads, and
   gfxstream and virgl both need a virtual GPU device between a guest and a host renderer,
   which a container does not have. It would render in software.

### Nested virtualization through a snapshot

Every GPU VM in fcvm is a restored clone (the snapshot is taken without the device and the
GPU is attached at restore), so KVM inside a phone VM has to survive a snapshot. Linux 6.18
executes VMXON when kvm_intel loads (`kvm.enable_virt_at_load`), and Firecracker did not save
KVM's nested state, so a restored guest believed it was in VMX operation and KVM did not:
the guest's next VMX instruction faulted. The fork's commit on `agent/vfio-nested-state`
(based on the gpu profile's amd64 commit) saves `KVM_GET_NESTED_STATE` in the vCPU state,
header included (kvm-ioctls reports a header-only state as none, but the header holds
`vmxon_pa`), and restores it after `SET_SREGS` and `IA32_FEATURE_CONTROL` and before
`SET_REGS`, QEMU's order, so a vCPU saved while running a nested guest re-enters guest mode
first. `SNAPSHOT_VERSION` goes to 13.0.0. The unit test
`test_vmx_operation_survives_save_and_restore` fails on the base commit (`vmxon_pa` comes back
as `u64::MAX`) and passes with the change; `make phone-test` checks it end to end with
`kvmprobe` in a restored VM. The `phone` profile pins that commit; the wall's `gpu` profile
keeps its own.

### Snapshots: the phone restore

The target, from the owner: an fcvm snapshot of a VM with the phone booted and the app
running; a restored clone attaches its GPU, the phone carries on, and the app keeps running
on the clone's GPU. Firecracker cannot save a passthrough device, so fcvm snapshots a GPU VM
without its GPU (`--gpu-warm` parks it: CUDA processes move their GPU memory to RAM with
cuda-checkpoint, then the GPU leaves the guest's PCI bus). The emulator holds EGL and Vulkan
objects on the GPU, so today the park refuses (measured above).

**The mechanism: detach and re-attach the emulator's renderer, with gfxstream's own
snapshot code.** gfxstream already saves and restores everything a guest sees of the host
GPU for the emulator's quickboot snapshots: `FrameBuffer::onSave` and `onLoad`
(`hardware/google/gfxstream`, `host/FrameBuffer.cpp`) carry the display configuration, the
guest's EGL contexts and surfaces, every color buffer, EGLImages and texture contents
(`eglPreSaveContext`, `eglSaveAllImages`), and Vulkan objects under the `VulkanSnapshots`
feature. The spike shows that state reloads onto a T4 and the app carries on at 60 fps, in a
new emulator process and on a different T4 in a clone. Detach is: pause the vCPUs, `onSave`
into memory, tear the renderer down until no handle on `/dev/nvidia*` remains. Attach is:
initialize EGL and Vulkan on the GPU the clone has, `onLoad`, resume. The guest sees what it
sees after a quickboot load, which is nothing.

**Why not hotplug first.** The owner suggested making Android adjust as for a display change.
A display hotplug makes SurfaceFlinger rebuild its display surfaces, but the app's contexts,
textures and buffers live in gfxstream color buffers that a hotplug does not recreate; a
device-lost would need every process to recover from `EGL_CONTEXT_LOST`, and SurfaceFlinger
and HWUI abort on GPU errors rather than recover. The in-process `onSave`/`onLoad` keeps every
object, so it is the main path. A hotplug is the fallback for a renderer state `onSave` cannot
carry, and the app handles surface loss correctly either way (`onSurfaceDestroyed` and
`onSurfaceCreated`, and recreating its GL objects on `EGL_CONTEXT_LOST`).

**The flow.** Gold: boot the phone VM, boot Android, start the app, then
`fcvm snapshot create --gpu-warm`. fc-agent's park asks every GPU holder that is not a CUDA
process to let go through a hook (the emulator's console, `gpu detach`), waits until no
process holds an NVIDIA device, and continues as today. A clone restores, attaches its GPU
and loads the driver as any GPU restore does, then unpark calls `gpu attach` and waits for
the app's next frame.

| flow | click to the app's first frame (6 GiB phone VM) |
|---|---|
| A: clone restore, then the emulator loads its own snapshot | 12.8 to 13.1 s, measured |
| B: clone restore of a running phone, renderer re-attached | about 5.5 s, projected: ACK 4.7 to 5.0 s plus the renderer's start and GPU state load from the warm load above (0.13 s and 0.42 s) |

B is the design. Its largest part is the memory pin, so the phone VM's size and hugepages
are measured in the same PR as the hooks.

The emulator is built from source for this, forked like Firecracker: `ejc3/` forks of the
emulator (`external/qemu`) and of gfxstream. The SDK's 37.2 emulator was built from an
`emu-37-2-release` branch that AOSP does not publish; the public branches are `emu-main-dev`
(a manifest of 76 projects) and `emu-36-1-release`, so the fork starts from `emu-main-dev`.
The SDK's license forbids modifying its emulator (below), and a source build also leaves
out the SwiftShader libraries the spike masks.

#### Detaching the renderer, measured (2026-10-28)

Measured in the phone container on a g4dn.metal (T4, driver 580.196.07) with
`fleet/phone/gpurelease.c`, which takes up the GPU through EGL and through Vulkan, tears
each down, and counts the process's `/dev/nvidia*` descriptors and mappings:

| step | descriptors | mappings |
|---|---|---|
| EGL context current, drawn on the T4 | 11 | 21 |
| after `eglTerminate` and `eglReleaseThread` | 10 | 6 |
| after `dlclose` of libEGL | 0 | 0 |
| Vulkan device created on the T4 | 20 | 58 |
| after `vkDestroyDevice` and `vkDestroyInstance` | 0 | 0 |

A second round in the same process takes the GPU up again through both. So a process can
let go of the GPU without exiting, as detach needs: Vulkan on destroy, EGL only once its
libraries are unloaded.

The emulator's other GPU mode was ruled out for detach. With `-feature GuestUsesAngle` the
guest's GLES runs as ANGLE on Vulkan (`ANGLE (NVIDIA, Vulkan 1.3.0 (NVIDIA Tesla T4))`,
60 fps), so the guest's GPU objects are Vulkan objects on the host, saved with the
`VulkanSnapshots` feature. Of two quickboot loads in that mode the first resumed the app
on the T4 and the second killed the emulator (`pRenderPassBegin->framebuffer ... not found
in mFbInfo`, a framebuffer the snapshot did not restore). The default GLES translator path
loaded eight of eight times (three loads in the source VM, five in clones on other T4s). The
phone stays on the translator path.

The change in gfxstream, from its own entry points (`host/RendererImpl.cpp`,
`host/FrameBuffer.cpp`, `host/gl/glestranslator/EGL/EglGlobalInfo.cpp`):

- **detach:** pause the guest; `RendererImpl::pauseAllPreSave`; `RendererImpl::save` into
  memory; `RendererImpl::stop` and `FrameBuffer::finalize`; then what nothing tears down
  today: the `EglGlobalInfo` singleton (a static never freed) and its display, with
  `eglTerminate`, the EGL and GLES libraries closed (the library cache gains a close, since
  only unloading them releases the GPU), and the Vulkan emulation's device and instance.
  The detach fails, and the renderer is brought back, unless the process then holds no
  `/dev/nvidia*` descriptor or mapping.
- **attach:** load the libraries, `RendererImpl::initialize` on the GPU now present,
  `RendererImpl::load` from memory, `RendererImpl::resumeAll`, resume the guest.

Both are emulator console commands (`gpu detach`, `gpu attach`), which fc-agent's park and
unpark call through the hook (stack PR 3).

### The app: an Android-themed scene

DECIDED (owner, 2026-10-27): the phones render their own scene, not the wall's terrain: the
Android robot and friends in a playful world in Material You colours. The plan: a park of
robots walking across the phones, a crowd that dances, robots that jump from one phone's
screen to the next, a viewer's word on a robot's sign, with lighting, shadows, thousands of
instances and particles. GLES 3 first (the path measured above); Vulkan once the emulator's
Vulkan path is measured on the T4.

- **One scene across the phones.** Each phone renders its rectangle of the whole image with an
  off-axis projection, from the coordinator's scene: the shared clock `t`, camera keyframes,
  words and the take-over actions (the renderer's `POST /scene` shape, with the phone's
  fields added). Nothing in the scene has state: every robot's position, pose and colour is
  a function of `t` and its id, so a restored phone draws the current frame at once.
- **Deterministic.** The same `t` and rectangle give the same pixels on every phone: no frame
  counters, no random numbers that are not hashed from ids and `t`. Every phone runs the same
  GPU and driver. A seam test, like the renderer's, renders adjacent rectangles and the whole
  image at one `t` and compares the shared edges.
- **Built** as Robot Park (`fleet/phone-app`, Kotlin and GLES 3, built with Gradle in a
  container; its control socket and scene fields are "Phone app" in `fleet/PROTOCOL.md`).
  Every robot's pose, every particle and sign is computed on the GPU from `t` and the robot's
  table entry. Measured on a T4 through the emulator (720 x 1280, 4x MSAA): 60 fps with 2,400
  robots, shadows, particles and a sign; the in-app benchmark draws a frame in 4.2 to 5.7 ms
  at 2,400 robots, 6.8 to 7.8 ms at 9,600 and 23.1 to 23.6 ms at 38,400.
- **How the tiles stay seamless.** A phone draws the whole image into a framebuffer of the
  whole image's size, scissored to its own part, so the GPU rasterizes the same window
  coordinates on every phone. Moving the viewport instead (the obvious off-axis approach) left
  10 to 116 pixels in 921,600 a level or two apart at a seam, because the GPU's attribute
  interpolation rounds differently at other window positions, even with vertices snapped to a
  subpixel grid. Blended particles are drawn unscissored (scissored, a few glow pixels blended
  1 level differently), and no two particles start at one point (a leaper's coincident
  sparkles made the same frame come out two ways from run to run). `scripts/phone/seam.py`
  allows up to 9 pixels a tile 1 level apart for one residual it has seen in 3 of 7 runs, the same
  pixel in a light's glow.
- **The emulator crashes when a guest GL client exits during composition.** Emulator 37.2.12
  logs `Failed to make display surface context current: 12290` (EGL_BAD_ACCESS) as a guest
  process with a GLES connection exits, then calls GL with no context and dies. Seen three
  times: after `screencap`, after the app was killed for a reinstall, and after a restart
  that exited without releasing EGL. The app now releases EGL before it exits for a restart (5
  of 5 restarts survived), and `make phone-test` installs onto a freshly booted phone. The
  emulator built from source (stack PR 2) is where to fix it.
- **Branding.** The robot may be reproduced or modified under CC BY 3.0 with Google's
  attribution line ("The Android robot is reproduced or modified from work created and shared
  by Google and used according to terms described in the Creative Commons 3.0 Attribution
  License."), shown on the info screen. "Android" may not be in the app's name, may only be a
  descriptor followed by a generic term, carries "Android is a trademark of Google LLC.", and
  the Android wordmark and Google's logos are not used. Nothing says or suggests Google made
  or endorses it.

### Getting the screen to the page

The page's frame protocol stays: FCVF frames of H.264 over a WebSocket, each stamped with
its `t`. A phone streamer in the container reads the emulator's screen (its gRPC screen
stream or the shared-memory video bridge it writes with `-share-vid`, whichever measures
faster), encodes with NVENC on the same GPU (the driver image already carries
`libnvidia-encode`), and serves the renderer's `/frames` route with its encoder and socket
code. The app writes the `t` it drew into a strip of pixels at the edge of its surface, which
the streamer reads and crops, so each frame carries the exact `t` it shows and the page's
seam sync works unchanged. In seamless mode the app is full screen (system bars hidden) and
the page shows the scene rectangle; zooming out is a scene field that tells every phone to
show its interface from a stated `t`, so all phones change together.

### Taking over

`claim_phone` borrows a phone with `claim_gpu`'s rules (one per viewer, a cooldown, a cap on
phones lent at once, refused to a broadcast-only viewer). The take-over panel sends one of a
fixed set of actions, which the coordinator validates as an enum and the agent passes to a
controller in the phone container that maps each to one fixed command: the info tab or the
scene tab, rotate (`adb emu rotate`), dark mode (`cmd uimode night`), robots wave or dance
(an intent to the app), a GPU benchmark run inside the app, restart the app
(`am force-stop`, then start), and reboot the phone, which restores the phone VM from its
snapshot and shows the restore time. The controller takes nothing but the action's name;
adb and the emulator console listen only inside the container.

### Invariants (each becomes a test)

1. A phone renders on its VM's GPU: SurfaceFlinger's renderer and the app's both name the
   GPU `nvidia-smi` reports, and a phone that cannot is reported failed, never shown in
   software (`make phone-test`; the streamer checks the same at start).
2. A restored phone VM can run a VM (`kvmprobe`), and Firecracker restores the nested state
   it saved (`test_vmx_operation_survives_save_and_restore`).
3. A phone VM is snapshotted only when no process holds its GPU (the park already refuses).
4. After a restore the app draws on the clone's GPU within the restore deadline.
5. The same `t` and rectangle give the same pixels on every phone (the seam test).
6. Nothing a viewer sends reaches a phone except scene fields and the enumerated actions.

### Failure states

| failure | what happens |
|---|---|
| the host's `kvm_intel` has no nested virtualization | the phone slot reports failed at the agent's inventory; `make phone-test` refuses to start |
| the renderer cannot reach the GPU (EGL or Vulkan init fails, or the renderer string is not the GPU) | the slot is failed and dark; never a software fallback |
| detach does not release the GPU within its deadline | the park is undone (as today) and the snapshot is refused |
| attach fails in a clone | the clone fails closed, as a failed GPU attach does, and the slot is relaunched with backoff |
| the emulator exits | the container exits, the VM reports it, and the agent relaunches the slot |
| the app stops drawing | the streamer's frame age passes its deadline and the tile is drawn dim, then failed |

### Licensing

- **The SDK's emulator and system images** are under the Android SDK License Agreement
  (developer.android.com/studio/terms): 3.1 grants use "solely to develop applications for
  compatible implementations of Android"; 3.4 forbids copying except for backup, modifying,
  redistributing or creating derivative works. The spike and `make phone-test` use them to
  develop and test the app, on our own hosts, unmodified, fetched from Google with their
  checksums pinned, and `scripts/phone/sdk.sh` refuses to fetch until
  `ACCEPT_ANDROID_SDK_LICENSE=1` says the agreement was read. A public wall of phones is not
  app development, so what ships is built from AOSP source: the emulator (`external/qemu`,
  QEMU's parts under the GPL; we serve video, not binaries) and gfxstream (Apache 2.0) from
  our forks, and an AOSP-based system image without Google's apps or services, whose
  components keep their own licenses and notices (the Linux kernel under the GPL, most of
  the platform under Apache 2.0). Building them from source is also what the detach change
  needs.
- **The Android robot** is CC BY 3.0 with the attribution line above; "Android" and Google's
  marks follow Google's brand guidelines, as under "Branding".

### Stack of PRs

1. Merged (#1400): the phone profile: the Firecracker fork's nested state, the guest kernel
   with KVM, the phone container (emulator with `-gpu host` and EGL-on-EGL), the probe app,
   and `make phone-test`. Then this design's "Detaching the renderer, measured".
2. The emulator from source: forks of the emulator and gfxstream from `emu-main-dev`, built
   with the AOSP system image into the phone image; `gpu detach` and `gpu attach` on a
   running phone, tested by detaching (no `/dev/nvidia*` descriptor or mapping left in the
   emulator), attaching, and checking the app's renderer and frames.
3. Snapshots of a running phone: fc-agent's park and unpark hooks for GPU holders that are
   not CUDA processes, `--gpu-warm` of a running phone, and a test that a clone's app renders
   on the clone's GPU; restore-to-first-app-frame measured, with the VM's size and hugepages.
4. The app's scene (Robot Park, `fleet/phone-app`): the Android-themed scene, deterministic
   by rectangle, its info tab, the action intents and socket, surface-loss handling; the seam
   test in `make phone-test`.
5. The phone streamer: screen capture, NVENC, FCVF frames stamped with `t`.
6. Fleet: phone slots in the agent and coordinator (`--phones`), the phone scene fields,
   `claim_phone` and its actions, phone restore timings in the state.
7. Web: `?mode=phones`, the zoom out, the bezels, the take-over panel.
8. ejc3/aws: the host AMI with the phone image and assets.

## Invariants (each becomes a test)

1. At most one VM runs per slot, and at most one slot holds a GPU.
2. A relaunch starts only after the slot's previous Firecracker process has exited.
3. A slot's address, name and GPU are the same after every relaunch.
4. An intent id is applied at most once; a repeated id returns its first receipt.
5. Every viewer that has accepted version v has the same state for v.
6. A browser never sends a scene change to a renderer; only the agent does.
7. An agent that loses its coordinator, and a coordinator that loses an agent, leave no VM
   running that nothing supervises.
8. A connection admitted while every seat is held never receives a direct token, and a
   renderer or aggregator that holds an admission key streams to no request without a valid
   one.
9. The broadcast overlay draws the state in effect at the shown frame's `t`, never a later
   one.

## Infrastructure

All of it is Terraform in ejc3/aws, applied by the jumpbox:

- **Network:** one VPC per region with an IPv6 /56, public subnets in every AZ, and a
  security group allowing TCP 443 from `::/0` and nothing else inbound (no SSH; SSM for
  access).
- **Hosts:** a launch template per region and architecture (the AMI, IOMMU kernel
  arguments, `Ipv6PrefixCount = 1`, the instance role), which the coordinator launches
  from as above.
- **AMI:** built by a pipeline from the fcvm release (fcvm, the gpu kernel and Firecracker,
  the driver image), one per architecture, copied to each region.
- **Coordinator:** a dedicated ECS cluster, one Fargate service with the coordinator and a
  `cloudflared` container, its S3 state object, and the Secrets Manager entries (the
  certificate, the Cloudflare DNS token, the Access service token for agents).
- **Coordinator releases:** `.github/workflows/fleet-coordinator-deploy.yml` builds and tests
  the ARM64 image after relevant changes reach main. Its main-only `fleet-stage-deploy`
  environment admits OIDC credentials that can publish that image and invoke one image-only
  release function. The function takes Terraform's backend lock, preserves the task's
  infrastructure settings, rolls out the digest and records it in the release parameter.
  A failed ECS rollout restores the old task. CI then checks Access, relaunches one tile in
  each half, requires new names, and receives actual renderer frames over IPv4/TLS. It can
  wake the fleet; lack of capacity fails the check. Terraform provisions the environment's
  variables and staging-only Access token. CI cannot read Terraform state or call ECS.
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
- **Directed camera with nudges. DECIDED.** The camera flies a choreographed path of
  stops and flights; viewers vote where the next stop goes instead of steering
  continuously. REVISED (2026-10-27): the path never stops moving (slow moves at stops,
  several flight styles, the day cycling through golden hour, dusk, night and dawn), and
  its order is drawn per stop rather than a fixed loop.
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
   a noisy patch healing (the wall's directed camera never holds still since 2026-10-27;
   its moving frames rely on the reprojected history). Viewers share the camera, the time of day, and lights they drop
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
