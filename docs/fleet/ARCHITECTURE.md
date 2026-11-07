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
  viewer's input to the first frame that reflects it. In a burst of drags that is the
  last drag whose camera key reached the page: the page hears its claim every 200 ms,
  and a drag replaces the keys of the drags before it.
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
  through the agent, never from a browser, and only with the scene token), so a camera move
  is ordered like any other action.
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
fcvm place whose host has not connected or was reclaimed. A GPU a connected host runs a
phone on is not a wall place, so it comes off `--fcvm-tiles`: one host of 8 GPUs with
`--phones 2` gives the fcvm side six places, not eight with two that no GPU will fill
(before that host's hello the side still holds eight). Sizing the sides by the live GPUs
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
   |  one 1920x1080 H.264 stream at 30 fps (1280x720 on a task under 4 CPUs), every frame's SEI carrying its scene t
   |  FLV, copied by an ffmpeg child over RTMPS
   v
Amazon IVS low-latency channel, standard type: the 1080p source, 720p, 480p, 360p and 160p renditions
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
  ticket (its first `/live` message) keeps it. A hidden tab keeps its seat while its socket is
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

**Enforcement is server side.** A renderer's or aggregator's `GET /frames` needs a direct
token as the socket's first message once it holds an admission key (never in the URL, which
proxies and access logs keep): `v1.<expiry, unix s>.<viewer>.<mac>`, the mac an
HMAC-SHA256 of the first three fields with a 32-byte key the coordinator makes at start (it sends
keys only when it runs a broadcast, so a fleet without one checks nothing). The
key reaches renderers in the scene the agent posts (the coordinator posts the EC2 tiles'
scenes) and aggregators in their layout, and never appears in a snapshot. A renderer
accepts the current key and the one before it, so a new key does not cut off a viewer between
two token refreshes. Tokens last 10 minutes and each seated connection gets a fresh one every
5 minutes; a token is checked when a stream opens, not while it runs. A late viewer that
flips its page into direct mode opens sockets that are closed with 1008 before a frame,
because nothing it holds is a token. A fleet renderer streams nothing before its first scene,
which is what says whether it needs a token. The coordinator mints tokens for the streams it reads itself (the EC2
aggregator, the broadcast, the poster), and an agent's aggregator mints its own from the
key. Only a holder of the scene token can replace the key: a fleet renderer takes
`POST /scene` and `/budget` only with it as the bearer token (fleet/PROTOCOL.md, renderer
"Access"). The token is a field of the certificate secret, so the host agents and tile
instances read it with the certificate, and the coordinator adds it when the secret has none.

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
draws a tile with nothing to send black, and encodes the image once: 1920x1080, libx264
`veryfast`, zero-latency, High profile, no B-frames, a keyframe every 2 s, CRF 18 capped at
7.5 Mbit/s (under the standard channel's 8.5 Mbit/s input limit). IVS's standard channel
passes a 1080p input through as its top rendition and transcodes the 720p (2.4 Mbit/s),
480p, 360p and 160p renditions players switch between, so the fleet encodes one stream, not a
ladder. On a machine with fewer than 4 CPUs (`coord::broadcast::default_args`) the image is
1280x720 at CRF 21 capped at 4.5 Mbit/s, which IVS does not pass through: its top rendition
is then a 720p transcode at 2.4 Mbit/s. The aggregator runs only while a tile is placed:
asleep, it is stopped and nothing is pushed, and every viewer sees the wake reel.

1080 lines, measured 2026-11-03 (the owner, at full screen on a 3456 x 1944 display, watching
the 720p broadcast: "still too dirty"). Three poses saved from the staging wall, every tile's
`q=std` stream at the broadcast's tile height (`fleet-renderer quality --h 432 --encode 270`
or `--encode 180`, 90 frames) composed as the aggregator composes them, encoded with its
command line, decoded (`fleet/tools/broadcast-quality.py`), scaled bilinearly to 3072 x 1728 as a page scales it, and compared
with the same pose held to 1024 samples at 432 lines a tile. FLIP, lower is closer:

| broadcast | heavy terrain | snow and lakes | snow and meadow |
|---|---|---|---|
| 720 lines, 4.5 Mbit/s, as IVS's 2.4 Mbit/s 720p rendition (before; approximated with libx264) | 0.0944 | 0.0552 | 0.0853 |
| 720 lines, 4.5 Mbit/s, as encoded | 0.0890 | 0.0531 | 0.0795 |
| 1080 lines, CRF 21, 6 Mbit/s | 0.0821 | 0.0482 | 0.0680 |
| 1080 lines, CRF 18, 7.5 Mbit/s (7.4 to 7.8 Mbit/s used after the first second) | 0.0799 | 0.0462 | 0.0637 |
| the 1080-line composite itself, before the encode | 0.0671 | 0.0392 | 0.0506 |

What it costs. The encode and the 16 tile decodes, `taskset` on one c8g.8xlarge (Graviton4,
nothing else on it), per frame: 720 lines 14.9 ms encode and 24.1 ms decodes (39 ms), 1080
lines 33.9 and 42.4 ms (76 ms), 1.96 times as much. The 2 vCPU coordinator task ran at 69% to
71% of its CPU (maximum 77%) with the 720-line broadcast (CloudWatch `CPUUtilization`,
2026-11-03), so 1080 lines need about 2.7 cores: the task grows to 4 vCPU and 8 GB (ejc3/aws),
about $58 a month more on ARM64 Fargate in us-east-2 ($0.158 an hour against $0.079). IVS: the
channel's input price is unchanged ($2.00 an hour live, standard), and a viewer on the 1080p
rendition is billed at Full HD, $0.144 a viewer-hour against HD's $0.072 (North America, first
10,000 hours a month, 2026-11-03); a viewer whose player picks 720p or lower is billed as
before. Each renderer encodes its broadcast stream at 270 lines instead of 180.

The aggregator writes FLV itself, so each frame's timestamp is its scene time (a skipped
slot is a gap, never a drift). It opens the stream with an `onMetaData` tag naming the size,
so the copy probes nothing (with ffmpeg's default probing a copy held frames back 0.9 s at
the median), carries a silent AAC track beside the video (without audio IVS served every
frame about 3.4 s later), and puts an SEI message before each frame's first slice: user data unregistered under a fixed UUID
(`push::SEI_UUID`), payload `{"t": <scene seconds>, "e": <scene epoch, unix ms>}`. An `ffmpeg`
child copies it to `rtmps://<ingest>:443/app/<key>`. A child that exits is started again after 1 s, doubling to 30 s, at the next
keyframe. The ingest URL comes from the broadcast secret through the environment, so the
only command line holding it is that ffmpeg's, inside the coordinator's task.

The coordinator task grows from 0.5 vCPU and 1 GB to 4 vCPU and 8 GB (ARM64 Fargate). The
720-line composite took 1.7 cores on the first measurement box's Xeon (encode 0.9, decode of
one 1280x720 source 0.5, composing and the copy the rest); 16 small tile streams decode the
same pixels. The 1080-line composite takes about twice that (above).

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
latency after the coordinator accepts it (the drawer's status line says so at once).
A click on a tile (or Enter on it) opens it as in direct mode: the zoom, its details, its
restart timeline and GPU readings. The zoom starts at once from the broadcast itself, the
tile's region of the video scaled up, with the layout of the frame on screen. A seated viewer
(one holding a direct token) then opens that one tile's own stream at its detail height, and
the page shows it in the zoomed box from its first frame; Escape or Back closes the stream. A
late viewer holds no direct token, so its zoom stays on the broadcast, and the panel says so
in one line. Gone in broadcast mode: "Your own GPU", the other tiles' streams and the stream
choice; the broadcast toggle is the only stream control.
`?broadcast=1` on the page's URL asks for the broadcast when the coordinator offers one.

### Staging access

The channel is private (authorization required): IVS refuses a playback request without a
token signed by the channel's playback key pair. The coordinator holds the private key (in
the broadcast secret) and gives each `/live` connection a playback URL with a token in its
admission message: an ES384 JWT with `aws:channel-arn`, `aws:access-control-allow-origin`
set to the page's origin, and a 1-hour expiry, renewed with the direct token every 5
minutes. On staging `/live` is reached only through Cloudflare Access, so only
Access-authenticated viewers ever get a token; the playback URL alone returns 403. A public
launch keeps the tokens, with `/live` open.

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
  actions; nothing typed reaches the phone. While Robot Park is in front, a click on the
  phone's screen is a tap there ("Taps in Robot Park" below).

### The phone

```
fcvm VM, `phone` profile: one GPU (VFIO), KVM inside (nested), restored from a snapshot
  container fleet/phone (privileged: /dev/kvm; the GPU as CDI devices)
    Android Emulator: QEMU with KVM, Android 15 (x86_64 image built from AOSP source)
      gfxstream, the emulator's host-side renderer: EGL-on-EGL through glvnd to NVIDIA's
      EGL (no X display), and Vulkan through NVIDIA's ICD, both on the passed-through GPU
    the app (guest GLES 3 / Vulkan through goldfish -> gfxstream -> the GPU)
    phone streamer: the screen -> NVENC H.264 -> FCVF frames over wss, stamped with t
```

Phones run on the x86 hosts (g4dn.metal): the emulator's x86_64 Android needs x86 KVM, and
the fleet's host AMI already loads `kvm_intel` with `nested=Y`. The arm64 hosts keep
rendering the wall.

The spike rig: g4dn.metal (T4, driver 580.196.07), host `kvm_intel nested=Y`, emulator
37.2.12, the SDK's `default` x86_64 Android 15 image, a 720x1280 phone with 4 cores. Every
number below is from that box (the phone has since moved to an emulator and an image built
from source).

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
the guest's next VMX instruction faulted. The fork's change (ejc3/firecracker#23, merged
into `agent/vfio-uffd-populate`, the gpu profile's amd64 branch) saves
`KVM_GET_NESTED_STATE` in the vCPU state, header included (kvm-ioctls reports a header-only
state as none, but the header holds
`vmxon_pa`), and restores it after `SET_SREGS` and `IA32_FEATURE_CONTROL` and before
`SET_REGS`, QEMU's order, so a vCPU saved while running a nested guest re-enters guest mode
first. `SNAPSHOT_VERSION` goes to 13.0.0. The unit test
`test_vmx_operation_survives_save_and_restore` fails on the base commit (`vmxon_pa` comes back
as `u64::MAX`) and passes with the change; `make phone-test` checks it end to end with
`kvmprobe` in a restored VM. The release build's vCPU seccomp filter allows
`KVM_GET_NESTED_STATE` (`test_vcpu_filter_allows_saving_nested_state`). The `phone` profile
pins the merge commit; the wall's `gpu` profile keeps its own.

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
process to let go through a hook (the emulator's console, `gpu release`), waits until no
process holds an NVIDIA device, and continues as today. A clone restores, attaches its GPU
and loads the driver as any GPU restore does, then unpark calls `gpu retake` and waits for
the app's next frame.

| flow | click to the app's first frame (6 GiB phone VM) |
|---|---|
| A: clone restore, then the emulator loads its own snapshot | 12.8 to 13.1 s, measured |
| B: clone restore of a running phone, renderer retaken | 5.75 to 6.19 s, measured (projected about 5.5 s) |

B is the design and is what fcvm does now (below). Its largest part is the memory pin; the
phone VM's size and hugepages are the next thing to measure.

#### Flow B, measured (2026-10-28)

The container's image names a GPU hook in the `dev.fcvm.gpu-hook` label
(`fleet/phone/gpu-hook`). fc-agent's park runs it with `release` before it reads which
processes hold an NVIDIA device, so the emulator, which has let go of the GPU, is not taken
for a CUDA process; a later park step that fails runs `retake` to undo it. The parked record
names the hook, and unpark runs `retake` once the GPUs are up and the CUDA processes are
restored, in the source VM after the snapshot and in a clone inside the restore's GPU attach,
before the clone is acknowledged; a failed retake fails the clone. The hook's `retake`
returns once the phone has posted a frame on the new GPU, so the acknowledgement follows
the app's first frame. The phone VM maps its volumes with `--portable-volumes`: the
emulator holds its system image and libraries open on them, and only portable inodes let a
clone find those files again.

On a g4dn.metal (T4, driver 580.196.07), a 6 GiB phone VM (2 GiB phone, the app drawing
25,600 cubes at 60 fps), one warm snapshot and three clones restored one at a time onto
three other T4s (`snapshot run --snapshot --gpu`, File backend), 1-minute load 1.6 at the
start:

| | clone 1 | clone 2 | clone 3 |
|---|---|---|---|
| click to the app's first frame on the clone's GPU | 6140 ms | 5751 ms | 6193 ms |
| restore ACK | 6138 ms | 5743 ms | 6184 ms |
| of which the guest's GPU phase (`gpu_ms`) | 1930 ms | 1912 ms | 2191 ms |
| of that, `gpu retake` | 922 ms | 816 ms | 916 ms |
| retake to the first frame | 987 ms | 912 ms | 1226 ms |

The GPU phase is the driver bring-up of any GPU restore (about 950 ms, mostly
nvidia-persistenced and the memory scrub) plus the retake and the first frame; the rest of
the ACK is mostly the VFIO memory pin, about 3.8 s for 6 GiB. The projection counted the
renderer's start and the state load of an emulator quickboot load (0.55 s); the retake
measures 0.8 to 0.9 s, since it also loads the host driver and initializes EGL, and
recreates the 22 render threads. The warm snapshot of the running phone took 12.6 to 13.0 s
(the park's release about 0.5 s, the source's retake about 0.8 s), and the source phone
kept drawing. In the first 3 s after a clone's restore the app's lowest fps sample was 36
to 38; four seconds in it was 56. `make phone-test GPU= GPU2=` checks all of it; without the
label the park refuses ("every process with a GPU open must be a CUDA process") and the
check fails.

The emulator is built from source for this, forked like Firecracker: ejc3/platform_external_qemu
(the emulator, `external/qemu`) and ejc3/gfxstream, each with AOSP's `emu-main-dev` on a
branch of that name and the fcvm change on `fcvm-main`. The SDK's 37.2 emulator was built
from an `emu-37-2-release` branch that AOSP does not publish; the public branches are
`emu-main-dev` (a manifest of 76 projects) and `emu-36-1-release`, so the forks start from
`emu-main-dev`. The SDK's license forbids modifying its emulator (below). The source build
still ships SwiftShader's libraries, and `phone.sh` masks them as it did the SDK's.

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

Both are emulator console commands (`gpu release`, `gpu retake`), which fc-agent's park and
unpark call through the hook (stack PR 3).

#### Releasing and retaking the GPU in the emulator (2026-10-28)

Built as above, with what the measurement left out:

- **The transport is the GLES pipe.** The AOSP x86_64 Android 15 image renders through
  `opengles` pipe channels (`ro.boot.qemu.gltransport.name=pipe`), one render thread each
  (22 on a booted phone with the app running), not address space graphics. The release
  pauses the guest and the render threads, saves the frame buffer into memory, has every
  pipe save its channel's state (the render thread's and its queues) and drop the channel,
  and then makes each paused render thread exit instead of resuming: it unbinds its context
  without draining it, since the contexts are in the saved frame buffer. The retake loads the
  frame buffer into a new one and gives every pipe a channel loaded from its saved state, the
  path a quickboot load takes for a new emulator process. A renderer with address space
  graphics consumers refuses.
- **Teardown order.** The translator's displays go first (an EGL image a load restored is the
  display's, and destroying it calls the GLES dispatch), then the dispatch, then the
  EGL-on-EGL host engine, the host display (`eglTerminate`) and the host `libEGL` and
  `libGLESv2`. A retake's load restores textures on a background thread, which a release
  stops first.
- **Vulkan.** With Vulkan emulation on and no `VulkanSnapshots`, the saved state holds no
  Vulkan objects, so `gpu release` refuses while the guest has a Vulkan instance. HWUI runs
  on Skia GL here (`debug.hwui.renderer=skiagl`), and the phone has none.
- **Snapshots while released.** The emulator's own snapshot save and load refuse with an
  error naming `gpu retake`.
- **A retake without Vulkan stays released.** The new frame buffer creates a Vulkan instance,
  and when that fails gfxstream carries on without Vulkan. The saved render threads still
  hold Vulkan thread state, and loading them aborted the emulator ("VkDecoderGlobalState not
  initialized"). That is how the static TLS exhaustion below ended a phone; any other reason
  for `vkCreateInstance` to fail would end it the same way. A retake whose frame buffer has no
  Vulkan when the released one had it now tears it down, stays released with the saved state,
  and says so (ejc3/gfxstream#6); `make phone-test` checks it by moving the ICD manifest away
  for one retake.

On the phone above, in its VM: `gpu release` takes 400 to 590 ms and leaves the emulator
with 0 of its 80 `/dev/nvidia*` descriptors, 0 of its 291 mappings and none of the NVIDIA,
EGL and GLES libraries mapped (the Vulkan loader stays, with no driver loaded); `gpu retake`
takes 600 to 830 ms, the guest posts a frame within 50 to 100 ms of it, and the app draws at
60 fps again. `make phone-test` checks the release and the retake.

`scripts/phone/emulator.sh` builds the emulator from `fleet/phone/emulator.xml`, the
`emu-main-dev` manifest with every project pinned and the two forks moved, into
`/mnt/fcvm-btrfs/android-emulator/<hash>` (on a g4dn.metal about 14 minutes to fetch 95 GB
of sources and 8 to build; 606 MB installed); `phone.sh` runs it from `/emu`, and the SDK
no longer supplies an emulator.

#### The system image from AOSP source (2026-10-28)

The phone boots an Android 15 image built from AOSP, not the SDK's (Licensing, below):
AOSP's `android-15.0.0_r36` (build BP1A.250505.005.D1, security patch 2025-05-05), the
target `sdk_phone64_x86_64-bp1a-userdebug`, which is the product the SDK's image was built
as (its build.prop: `sdk_phone64_x86_64-userdebug 15 AE3A.240806.019`). `fleet/phone/aosp.xml`
pins each of the tag's 1030 Linux projects to the commit its superproject records.
`make phone-system-image` (`scripts/phone/aosp-build.sh`) syncs them, builds the image with
`m emu_img_zip`, and packages two archives into `/mnt/fcvm-btrfs/aosp-build/<id>/dist`,
`<id>` a hash of the script and the manifest:

- `aosp-system-<id>.zip`: the system image in the layout of the SDK's
  `system-images/android-35/default/x86_64` (system, vendor, ramdisk, the kernel, and the
  image's NOTICE).
- `aosp-tools-<id>.tar.gz`: `adb`, and what builds an app without the SDK: `aapt2`, `d8`,
  `zipalign`, `apksigner`, and an `android.jar` of the public API's stubs and
  `framework-res`'s resource table, with each tool's license.

The build fixes the stamps a build takes from its host (date, user, host name and build
number; the fingerprint is `Android/sdk_phone64_x86_64/emu64x:15/BP1A.250505.005.D1/<id>`),
and the tools archive is written with fixed owners, modes and times; a second build from
the same pins has not been compared byte for byte. AOSP's toolchains are
x86_64 Linux prebuilts, so it runs on an x86_64 host only; every pbox compute flavor is
Graviton, and the build ran on a `kvm-gpu-96` (g4dn.metal, 96 vCPU, its NVMe): 32 minutes to
sync about 160 GB of sources (16 fetches at once; at 64, android.googlesource.com answered
`RESOURCE_EXHAUSTED`), and 42 minutes (2532 s) to build from an empty `out/`. The archives are the
GitHub release `aosp-<id>` on ejc3/fcvm; `scripts/phone/aosp.sh` pins the build id and each
archive's SHA-256, fetches and checks them into `/mnt/fcvm-btrfs/android-aosp/<hash of the
pins>` (mapped at `/android` in the phone container), and builds the probe app with the
fetched tools. It refuses when the build id it pins is not what this checkout's recipe and
manifest build.

The kernel and its modules are the tree's prebuilt GKI build (`kernel/prebuilts/6.6/x86-64`
and the virtual-device modules: `6.6.57-android15-8-g8b48c9979699`, Android CI build
12748506), as the SDK's image carries the same branch's (`6.6.30-android15-8`). One
difference from the SDK's image needed a change: this image's `advancedFeatures.ini` turns UWB on, and with no emulated UWB peer
the guest's UWB stack reports a device error at boot (`am_wtf UwbServiceCore: Error device
status received`), on which system_server takes a full bug report; dumpstate then holds a
core for about 30 s, and the app's lowest fps sample after the warm snapshot read 27.9 and
24.0 in two runs. `phone.sh` starts the emulator with `-feature -Uwb`, which takes the UWB
HAL away but not the UWB service: it still turns UWB on at boot, waits 20 s for the HAL,
retries every 10 s, and a later failure asks for the same bug report ("UWB Bugreport: error
enabling UWB"), about 50 s after boot (dumpstate 30 to 37 s). So `phone.sh` also turns UWB off
(`cmd uwb disable-uwb`) once booted, and `make phone-test` fails when the phone has taken a
bug report (`dumpstate.last_id`) at any point in the run.

The emulator also preloads the NVIDIA driver's `libnvidia-tls`. Every `gpu retake` loads the
driver's libraries again, and a library loaded at run time takes its initial-exec TLS from
glibc's static TLS surplus, which an unload gives back only when it was the last block handed
out: on five phones, releases and retakes back to back, the 29th retake could not load the
Vulkan driver ("cannot allocate memory in static TLS block") and the emulator aborted.
Preloaded, four phones ran 60 each with no failure.

`make phone-test` passes on the AOSP image, and the two images measure alike. Four
runs of each, alternating, on one g4dn.metal (T4, driver 580.196.07, 6 GiB phone VM, 2 GiB
phone; the source-built emulator for both):

| | SDK image (AE3A.240806.019) | AOSP image (BP1A.250505.005.D1) |
|---|---|---|
| SurfaceFlinger's GLES line | `Google (NVIDIA Corporation), Android Emulator OpenGL ES Translator (Tesla T4/PCIe/SSE2), OpenGL ES 3.1 (OpenGL ES 3.2 NVIDIA 580.196.07)` | the same |
| the app's GLES renderer | `Android Emulator OpenGL ES Translator (Tesla T4/PCIe/SSE2)` | the same |
| cold boot to `sys.boot_completed` | 30.8 to 31.6 s | 31.1 to 32.3 s |
| the probe app's fps, 24 samples a run: median, mean | 60.0, 59.1 | 60.0, 59.3 |
| `gpu release`, `gpu retake` | 496 to 540 ms, 671 to 826 ms | 425 to 495 ms, 587 to 738 ms |
| warm snapshot of the running phone | 15.4 to 15.8 s | 15.6 to 15.8 s |
| flow B: click to the app's first frame on the clone's GPU | 6180 to 6579 ms | 5843 to 6260 ms |
| of which the guest's GPU phase (`gpu_ms`) | 1682 to 1851 ms | 1580 to 1698 ms |
| the app on the clone's GPU, lowest fps sample in 4 s | 44.0 to 52.0 | 54.3 to 56.4 |

The first `make phone-test` on that box, right after the fetch, measured flow B at 8973 ms
with a `gpu_ms` of 1633; the other 2.8 s are outside the guest's GPU phase and were not
broken down.

### The app: an Android-themed scene

DECIDED (owner, 2026-10-27): the phones render their own scene, not the wall's terrain: the
Android robot and friends in a playful world in Material You colours. The plan: a park of
robots walking across the phones, a crowd that dances, robots that jump from one phone's
screen to the next, a viewer's word on a robot's sign, with lighting, shadows, thousands of
instances and particles. GLES 3 first (the path measured above); Vulkan once the emulator's
Vulkan path is measured on the T4.

- **One scene across the phones.** Each phone renders its rectangle of the whole image with an
  off-axis projection, from the coordinator's scene: the shared clock `t`, words and the
  take-over actions (the renderer's `POST /scene` shape, with the phone's
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
  robots, shadows, particles and a sign; the in-app benchmark draws a frame in 4.1 to 5.7 ms
  at 2,400 robots, 6.8 to 7.9 ms at 9,600 and 23.1 to 23.8 ms at 38,400.
- **How the tiles stay seamless.** A phone draws the whole image into a framebuffer of the
  whole image's size, scissored to its own part, so the GPU rasterizes the same window
  coordinates on every phone. Moving the viewport instead (the obvious off-axis approach) left
  10 to 116 pixels in 921,600 a level or two apart at a seam, because the GPU's attribute
  interpolation rounds differently at other window positions, even with vertices snapped to a
  subpixel grid. Blended particles are drawn unscissored (scissored, a few glow pixels blended
  1 level differently), and no two particles start at one point (a leaper's coincident
  sparkles made the same frame come out two ways from run to run). `scripts/phone/seam.py`
  allows no pixel apart. It allowed 9 a tile 1 level apart for one glow pixel seen 1 level
  apart in 3 of the first 10 runs of the final rendering; on the emulator built from source
  that pixel came out identical in every later phone-test and in 60 app restarts in a row
  (#1417).
- **The emulator crashes when a guest GL client exits during composition.** Emulator 37.2.12
  logs `Failed to make display surface context current: 12290` (EGL_BAD_ACCESS) as a guest
  process with a GLES connection exits, then calls GL with no context and dies. Seen three
  times: after `screencap`, after the app was killed for a reinstall, and after a restart
  that exited without releasing EGL. The app now releases EGL before it exits for a restart (5
  of 5 restarts survived), and `make phone-test` installs onto a freshly booted phone. The
  emulator built from source does not show it: 60 iterations of killing the app with
  `am force-stop` while SurfaceFlinger composed (every tenth a restart through the app), each
  followed by a `screencap`, left the emulator running with no such line in its log (#1417).
- **Branding.** The robot may be reproduced or modified under CC BY 3.0 with Google's
  attribution line ("The Android robot is reproduced or modified from work created and shared
  by Google and used according to terms described in the Creative Commons 3.0 Attribution
  License."), shown on the info screen. "Android" may not be in the app's name, may only be a
  descriptor followed by a generic term, carries "Android is a trademark of Google LLC.", and
  the Android wordmark and Google's logos are not used. Nothing says or suggests Google made
  or endorses it.

### Getting the screen to the page

The page's frame protocol stays: FCVF frames of H.264 over a WebSocket, each stamped with
its `t`. A phone streamer in the container reads the emulator's screen from its gRPC
screen stream (`streamScreenshot`), encodes with NVENC on the same GPU (the driver image already carries
`libnvidia-encode`), and serves the renderer's `/frames` route with its encoder and socket
code. The app writes the `t` it drew into a strip of pixels at the edge of its surface, which
the streamer reads and crops, so each frame carries the exact `t` it shows and the page's
seam sync works unchanged. In seamless mode the app is full screen (system bars hidden) and
the page shows the scene rectangle; zooming out is a scene field that tells every phone to
show its interface from a stated `t`, so all phones change together.

The shared-memory bridge (`-share-vid`) was tried first and dropped: after a cold boot it
carried black frames (the emulator fills it from a readback worker that the gfxstream
display never fed), and the emulator crashed reading it back after a snapshot load or a GPU
retake. The gRPC stream composes the same screen the emulator window shows.

Measured on a g4dn.metal (T4, driver 580.196.07, the emulator built from source), Robot Park
full screen at 720x1280, from the streamer's own `/health` counters:

| step | p50 | p90 |
|---|---|---|
| app draw (its stamped `t`) to the frame reaching the streamer | 36.3 ms | 36.6 ms |
| frame reaching the streamer to its FCVF frame sent | 14.0 ms | 15.0 ms |
| of which the NVENC encode | 4.6 ms | 4.7 ms |

The stream carries 30 scene frames a second (the app draws 60; the streamer sends the even
`k`), 83.6 KB a frame at `q=std` and 29.3 KB at `q=lo`. A cold phone VM answered `/health`
23 s after the run, Android booted at 34.6 s, and the first stamped scene frame was sent at
61 s.

### Taking over

`claim_phone` borrows a phone with `claim_gpu`'s rules (one per client, a cooldown, a cap on
phones lent at once, refused to a broadcast-only viewer). Unlike a GPU, a borrowed phone keeps
its place on the wall and its stream; when it comes back (after `phone_claim_secs`, on
`release_phone`, or when its viewer leaves) it is restored from its snapshot, so the next
viewer finds it as every phone starts. The take-over panel sends one of a fixed set of
actions (`phone_action`), which the coordinator validates as an enum and hands to the agent
in the phone's assignment; the agent runs `phone.sh action <name>` in the phone container,
which sends Robot Park the control requests the name maps to (PROTOCOL.md, "Phones
mode"): the info tab or the scene tab, rotate, dark or light, the robots dance or wave, the
app's GPU benchmark, restart the app. Reboot the phone is the coordinator's: it relaunches the
phone's VM (a clone of its warm snapshot, flow B) and the page shows the restore time. The
controller takes nothing but the action's name; adb, the emulator console and the app's
control socket listen only inside the container.

**What the held phone shows each action do.** The owner, 2026-11-04, on staging: "some of
those things don't do anything because the scene isn't focused on them. make it so". Every
action has to change what the viewer sees on the phone they hold, within about a second,
whatever it showed before. Two things hid the effect:

- *No robot on the phone's screen.* A phone draws its slice of the phones' image with the
  shared camera, and the shared camera is the wall's director: keyframes in kilometres of the
  wall's terrain (eye at z = -60 to -140 looking further down -z) that Robot Park reads as
  metres of the park, whose robots walk between z = -46 and 16. Every one of the wall's stops
  looks away from every robot, so the phones show lawn, trees and hills, and a phone that
  danced showed no change at all: Robot Park shows no banner for dance or wave
  (`aHeldPhoneFollowsItsOwnRobotUpClose` asserts that slice holds no robot). A slice of a park camera would still leave the robots at a phone's
  edge small or outside it.

  The phones the viewer does not hold had the same lawn, and no unit conversion fixes it: in
  metres the wall's eye is 100 km from the park, past Robot Park's 900 m far plane, so a phone
  shows no robot either way. The agent gives a phone no camera keyframes (`renderer_body` in
  `src/fleet/agent/slot.rs`, pinned by `a_phone_is_given_no_wall_camera`), and Robot Park draws
  the phones' image with its own camera over the park (PROTOCOL.md "Phone app", `camera`): a
  slow sway, the same on every phone, so the phones still stitch. Six phones side by side show
  at least 125 robots each at every 50 s of the crowd's 1200 s period
  (`everyPhoneOfARowShowsRobotsWithTheParksOwnCamera`).
- *The info tab in front.* The info screen covers the scene, so dance, wave and night were
  behind it.

The held view. While a viewer holds a phone, its scene carries `held` (the phone streamer
adds it while the coordinator's scene names the holder, `mirror_viewer`), and Robot Park
leaves the shared image as a turned phone does: it draws the whole image on its own, at its
own aspect, with a camera 5.5 m in front of the phone's own robot, following it along the lawn
on the crowd's walking clock (a grown walker near the front, a different one for each column
of phones). That robot is at least 250 px tall on a 720 x 1280 screen and 140 px turned, with
at least 20 robots on the screen behind it, at every 5 s of the crowd's 1200 s period. When
the phone is given back its scene no longer carries `held` and it draws its slice again
(and the return restores it from its snapshot in any case). The other phones draw their slices
as before; the held phone's place in the seamless image shows its held view, as a turned
phone's does.

Then each action shows its effect on the screen:

| action | the held phone, before | now |
|---|---|---|
| Switch screen | the other tab | the same |
| Rotate | turned, but no robot in the turned view either; on the info tab the info turns | the scene tab, turned, its robot followed |
| Dark mode | the night sky; behind the info tab only the banner and the info's colours | the scene tab first, then night or day |
| Dance, Wave | nothing: no robot on the lawn, or the info tab in front | the scene tab first; the robot in front and the crowd dance or wave |
| GPU benchmark | the banner, the screen held while it runs, the result banner | the same |
| Restart app | the app starts again | the same |
| Open nvidia.com | the browser in front, the page loaded again | the same |
| Robot Park | nothing when Robot Park was already in front | the scene tab; the page offers it only while the browser is in front |
| Reboot phone | the panel says Restoring, the phone restores | the same |

Measured on a g4dn.metal (2026-11-04), phone 6 of a six-phone scene with the wall's camera,
held, on the info tab before each Robot Park action, recording the streamer's `/still` every
130 ms or so: from the start of `phone.sh action` (as the agent runs it, through `fcvm exec`)
to the first frame that differs from the one before by more than the scene's own motion, 145 to
180 ms for the actions inside Robot Park, 280 to 290 ms for the two that bring it from the
browser, 422 ms for Open nvidia.com and 720 ms for Restart app. On the current host image, dance
and wave from the info tab changed no frame in 3 s.

Dance and wave were already this phone's own moves (`dance`, `wave`, PROTOCOL.md "Phone
app"); only the coordinator's scene moves stop the crowd on every phone together.

**Taps in Robot Park.** The owner, 2026-11-04, after clicking the app's Scene and Info tabs
in a take-over panel that sent nothing: "If our simulator app is up we should let there be
pass through". While a viewer holds a phone and Robot Park is in front, a click on the
phone's screen in the panel (its video, or its GPU mirror, whose box maps the same way) is a
`tap` at the click's fractions of the screen; a click on the bezel is nothing. The fixed
actions stay as they are for everything else, and the browser in front takes no taps: the
page says "Taps reach the phone only in Robot Park." under the screen and the coordinator
refuses one. Which app is in front is the coordinator's record of the actions it accepted
for the phone's current launch (`front` in the phone's tile): the container keeps its own
record (`$RUN/front`, what `phone.sh` last brought to the front) and refuses a tap the
coordinator let through when the two differ. The agent passes the point as two decimals, and
`phone.sh` maps it to Robot Park's surface size, which turns with the screen, and runs adb's
`input tap`. Taps have a rate limit of their own, one every 200 ms, and stay in the
assignment for 3 s.

Measured on a g4dn.metal with a clone of the host image's warm phone (2026-11-04, 20 of
each): adb's `input tap` in the container took 35 to 69 ms; `phone.sh action tap` there, with
its `status` request, 49 to 61 ms; `fcvm exec` alone 95 to 137 ms; the agent's whole command
112 to 256 ms (median 176). The agent carries out one action at a time, so one tap every
200 ms is what it keeps up with; at 10 a second a quick clicker would queue up to 3 s of taps
behind it. From the start of the agent's command to the first decoded frame of
the stream showing the tapped tab: median 284 ms, 90th percentile 325 ms (218 to 349 ms; Info
218 to 298, Scene 283 to 349).

Phones are their own slots (`phone-<n>`), never wall tiles: an agent run with `--phones N`
runs phones on the last N GPUs of its inventory and names them in its hello. The
coordinator lays them out in their own image (a row of up to six, then rows of equal length)
and the page shows it with `?mode=phones`.

### The GPU mirror

Status: **built**; a viewer joins at a snapshot of the phone's color buffers and GL state (point 3), with no app restart. The owner, 2026-10-29: "when
you take over a phone, you can have this mirror gpu mode and streaming video mode and both
shown with bandwidth of each", and "can we have the server gpu mode that can serve as that
mirror to prevent blocking calls?"

The viewer holding a phone picks one of three views in the take-over panel:

| view | what reaches the viewer | what the panel shows beside it |
|---|---|---|
| Video | today's NVENC H.264 frames (`GET /frames`) | Mbit/s over the last second, MB since take-over, fps |
| GPU mirror | the guest's GL command stream (`GET /mirror`), drawn by the viewer's WebGL2 | the same three, and the blocking calls per second the phone's T4 answered |
| Both | both streams, side by side | each stream's own readout |

**The T4 stays the phone's GPU.** It renders, answers every call that waits for a reply
(`glGen*`, `glGet*`, `glReadPixels`, `glFinish`, fences, `rcCompose`, `rcMakeCurrent` and the
rest of renderControl's) and composes the screen, exactly as with no mirror. The browser only
replays: it decodes the same stream and never replies, so no guest call ever waits on the
network. The blocking-call counter is what the server GPU saves the phone: each would be a
round trip to the viewer if the viewer's GPU were the phone's GPU.

In GPU-mirror-only mode the page closes that phone's video stream, so the readout is the
mirror's whole cost; the T4 keeps rendering (it answers the calls, and every other viewer
of the wall still gets the phone's video).

```
guest app / SurfaceFlinger / HWC (GLES 3.0, renderControl)
  -> goldfish pipe -> gfxstream render thread: ReadBuffer::getData  --tap-->  bounded ring (32 MiB)
                      decoders -> translator -> T4 (answers, draws, composes)       |
                      ChannelStream::commitBuffer (reply) --tap (first 256 B)--> ring
                                                                                     | sender thread
  phone streamer  <- Unix socket /tmp/gfxstream-mirror.sock (one consumer) ----------+
    GET /mirror: admitted for the holder's token only; zstd -3, 1 MiB window (ZSTD_e_flush per message)
    bounded queue (8 MiB): a viewer behind it is dropped and joins again
  -> wss -> page worker: zstd's decoder in WebAssembly -> tap records -> per-channel packets
            -> decoder (emugen table) -> one WebGL2 context, a virtual context per guest context
            -> the posted color buffer drawn to the mirror's canvas
```

**1. What is copied.** Every render channel's guest-to-host bytes in the order they arrived,
and the start of every reply, tagged with the channel. That is the GLES 2/3 stream, the
renderControl stream (contexts, window surfaces, color buffers and their CPU uploads, posts,
and HWC's `rcCompose`, which is how this image composes the screen) and, unused, any GLES 1
or Vulkan packets. The copy is taken where the render thread reads (`ReadBuffer::getData`), not
where it decodes, so the order across channels is causal: a channel that waited on another's
fence did so through a reply, and the reply is copied after the bytes that preceded it on the
other channel. Replies are needed because the server, not the guest, makes object names
(`glGenTextures`, `glCreateProgram`, `rcCreateColorBuffer`, `rcCreateContext`, uniform
locations): the mirror reads each name from the reply instead of making its own, so it never
depends on two threads' names coming out in the same order. A reply past 4096 bytes (a
readback, a read map) is cut to its first 4096 bytes, which every reply that returns names
fits in; the mirror needs none of a readback's data, and the counter still counts it. The tap
copies replies in both transports' `commitBuffer`: the pipe's `ChannelStream`, which this image
uses, and address space graphics' `RingStream`. DMA uploads (`rcUpdateColorBufferDMA`) would carry a guest address, not
the bytes; the tap does not follow it. The captures below hold none (this image creates its
color buffers with `rcCreateColorBufferDMA`, which carries no pixels, and uploads none by
DMA), and the mirror counts any it meets.

**Vulkan.** WebGL2 cannot replay Vulkan, so the whole screen must be GL. It already is: HWUI
runs on Skia GL in this image (`dumpsys gfxinfo` reports `Pipeline=Skia (OpenGL)`; "Releasing
and retaking the GPU", above), and `gpu release` refuses while a guest holds a Vulkan instance,
which the phone never does. Phones mode needs no `debug.hwui.renderer` change, so forcing GL
costs the phone nothing. The captures below hold no GLES 1 or Vulkan packet; the mirror counts
any it meets as skipped.

**2. The browser renderer: a TypeScript decoder from gfxstream's own specs.** Two options
were weighed:

| | gfxstream's host GLES decoder and translator in WebAssembly | a TypeScript decoder generated from the emugen specs |
|---|---|---|
| code to port or write | 41,600 lines of translator, 79,400 in `host/gl`, 13,500 of `common/base`, plus `frame_buffer.cpp` (5,300) and its threads, EGL and dlopen of the host libraries | a 150-line generator and a table of 513 calls (439 GLES 2/3, 74 renderControl); an executor for the calls this image sends |
| contexts | WebGL2 has no share groups: a translator that dispatches to one WebGL2 context still needs a virtual-context layer under its dispatch | the same virtual-context layer, at the guest's API, where the state is the guest's own |
| names | the translator makes names; replay needs its namespace patched to take the server's | names come from the replies |
| fidelity risk | the same decode and composition code as the server | a reimplementation of the translator's GLES-on-GLES mapping, which for GLES 3.0 on WebGL2 is mostly 1:1 |
| join snapshot | gfxstream's own `onLoad` | a loader of the same `onSave` stream (point 3) |
| download | several MB of WebAssembly | tens of KB of script |

Chosen: the TypeScript decoder. `fleet/web/scripts/gen-mirror-table.ts` reads
`codegen/gles2` and `codegen/renderControl` (copied into `fleet/web/core/mirror/spec`) and
writes the wire layout of every call. Against gfxstream's generated decoders
(`host/gl/gles2_dec/gles2_dec.cpp`, `host/renderControl_dec/renderControl_dec.cpp`, parsed for
each case's parameter widths and buffers), all 513 layouts match; the generator first got 18
wrong (an attrib block given twice, and a DMA pointer's length that travels as the pointer's
own count) and the comparison found each. The fidelity question is answered by the pixel test
(point 6).

**Caps.** WebGL2 is GLES 3.0. `GFXSTREAM_MAX_GLES_VERSION=3.0` (ejc3/gfxstream#4) caps what
gfxstream offers the guest through the path its Intel workaround already uses; `phone.sh serve`
sets it, SurfaceFlinger then reports `OpenGL ES 3.0 (OpenGL ES 3.2 NVIDIA 580.196.07)`, and the
guest sends no 3.1 call or `#version 310 es` shader. The guest reads it at boot, so the warm
snapshot carries it. Extensions beyond WebGL2's are the remaining divergence; the executor
counts every call it does not draw. In the captures the only ones are queries it need not
draw (`rcGetGLString`, `rcChooseConfig` and the like). Program binaries are hidden from a guest
the tap can mirror (`GFXSTREAM_MIRROR_SOCKET` set): `GL_NUM_PROGRAM_BINARY_FORMATS` is 0, so
Skia keeps its shader sources instead of saving binaries, and no process loads a program with
`glProgramBinary`, which would send the mirror no source to compile.

**Color buffers.** A color buffer lives in the mirror until the host destroys it: the tap
sends a `FREE` record (type 7, its handle in `aux`) from each place gfxstream's `FrameBuffer`
erases one, whichever way its last reference went (`rcCloseColorBuffer`, the guest's refcount
pipe, the cleanup of a process that ended). The guest's own open and close calls are only part
of the count, and the host keeps a closed buffer for a second in case it is opened again, so
the mirror ignores them. A freed buffer's texture stays while a guest texture, renderbuffer or
window surface still draws with it.

The cap costs the T4 path nothing measurable. Two boots at each setting, alternating, on one
g4dn.metal (1-minute load 1.3 to 3.0):

| | GLES 3.1 (uncapped) | GLES 3.0 (capped) |
|---|---|---|
| Robot Park's GPU benchmark, ms a frame at 2,400 / 9,600 / 38,400 robots (median of 6) | 4.05 / 7.08 / 23.57 | 4.09 / 6.93 / 23.55 |
| Robot Park's fps (10 s of samples each boot) | 58.1 to 60.2 | 56.8 to 60.8 |
| Settings scrolled, HWUI GPU time p50 / p90 (`dumpsys gfxinfo`) | 12 / 17 to 18 ms | 12 to 13 / 17 to 18 ms |

**3. Joining a phone that has been running for hours.** The mirror needs the state every
later command builds on: every live context, texture, buffer, program, framebuffer, EGL image
and color buffer. The streamer starts the tap with `J` instead of `S`, and gfxstream's join
handler (`RendererImpl::saveMirrorJoin`, ejc3/gfxstream `fcvm/mirror-join`) does what `gpu
release` does up to its save and stops there: it pauses every render thread at a read boundary,
starts the copy while they are paused (so each channel's first copied bytes are the ones its
paused thread had not consumed yet, where the snapshot leaves off), saves, and resumes. The
saved state reaches the page as `JOIN` records between the HELLO and the first live record,
outside the tap's ring (which does not bound them):

| part | what it holds |
|---|---|
| BEGIN | the format's version |
| COLOR_BUFFER | one color buffer: handle, size, format, and its pixels read back as RGBA8 |
| GL_STATE, TEXTURE, CHANNEL | the translator's state as `FrameBuffer::onSave` writes it for quickboot, the textures that save writes, and each paused channel's saved thread state (its current context and surfaces) |
| END | whether the snapshot is whole, the pause, and the color buffer posted last |

The page makes every color buffer with its pixels and draws the one posted last at END, so the
screen is right at once and every layer the composer draws (status bar, wallpaper, launcher)
is right in the next composition. The GL parts then rebuild every context, its share group's
objects (textures from their TEXTURE parts, buffers, shaders, programs with their uniform
values, samplers, framebuffers, vertex arrays) and its state, the window surfaces, and each
paused channel's current context (`app/_wall/mirror/gl-join.ts`), so every process that was
running at the join (SystemUI, the launcher, the browser, an app) keeps drawing, and the
panel restarts nothing; it restarts the app only after a join without the GL state (`joined`
says which). What the mirror cannot rebuild is counted in its join stats (renderbuffer
contents, client-memory vertex pointers, programs that do not link in WebGL2). The streamer queues the `JOIN`
records under a bound of their own (256 MiB) apart from the live 8 MiB, and the page bounds
what it has not decoded at 256 MiB while a join is being fed. A join is refused (the copy starts with a refused END) while the emulator holds the render threads paused for its own snapshot or
a GPU release, and with address space graphics.

Measured on a g4dn.metal (T4), the AOSP phone in its 6 GiB VM, eight joins in one session on
the emulator this change pins (`scripts/phone/mirror-capture.sh`, a client on the host): six on
the home screen (three of them after a `restart_app` that left the home screen in front), two
with the browser on www.nvidia.com in front.

| | home screen | browser in front |
|---|---|---|
| color buffers | 47 to 51 | 55 and 58 |
| render threads paused (the phone's cost) | 151 to 500 ms | 220 and 488 ms |
| join before compression: pixels + GL state | 151 to 201 MB (46 to 53 MB + 105 to 147 MB) | 244 and 264 MB (60 and 71 MB + 183 and 193 MB) |
| join on the wire (zstd) | 12.4 to 17.2 MB | 9.8 and 11.0 MB |
| END at the client, from the stream's open | 0.52 to 1.19 s | 0.76 and 1.06 s |

The median pause over all eight is 254 ms. Every join ended with status 0 and SurfaceFlinger
ran after each (the capture checks both). The page decodes and draws a 49-buffer join in about
160 ms in headless Chrome (SwiftShader). An earlier gpubox run, before the join left the
guest's reads alone, lost SurfaceFlinger after its first join: a paused channel's
host-to-guest queue failed every guest read, which ended the guest processes' connections.

**Pixel match at the join.** `e2e/fixtures/mirror/home` holds one of those joins, cut to the posted
color buffer and 400 live records: the mirror's first frame, drawn from the snapshot before any
live record, against the phone's screenshot: 0.07 percent of pixels more than 16 levels off,
47.6 dB. Without the loader the mirror draws nothing (99.9 percent off).

`e2e/fixtures/mirror/browser` holds a join with the browser on www.nvidia.com in front and the
9.4 s of live records after it, in which the page's carousel turns to its next slide. Every
record is kept; the TEXTURE parts of textures no live draw samples, and of color buffers'
own textures, are dropped, and the pixels of color buffers nothing reads are zeroed (1.9 MB
gzipped, 92 MB inflated). The mirror's last frame against the phone's screenshot of the page:

| | pixels more than 16 levels off | PSNR |
|---|---|---|
| the mirror | 0 | 69.5 dB |
| rows as the mirror had them | 28.7 percent | 7.5 dB |
| one buffer per guest buffer | 9.5 percent | 12.2 dB |
| neither fix | 27.5 percent | 7.6 dB |
| without the GL loader | 39.7 percent | 5.7 dB |

**4. Backpressure, compression, GPU moves.**

- The tap never blocks a render thread: a copy into the ring is a `memcpy` under a mutex held
  for that copy; a full ring stops the copy with an `OVERFLOW` record, and the consumer starts
  it again. The sender thread alone waits on the socket.
- The streamer's queue toward the viewer is bounded (8 MiB). A viewer behind it is closed with
  1013 ("join again"), and the page rejoins, at most once every 10 s. The page bounds its own
  side too: 16 MiB received and not yet decoded ends the stream the same way, as does an
  `OVERFLOW` record from the tap (the stream then has a gap).
- The wire is one zstd stream per connection (level 3, a 1 MiB window), each message flushed
  to a block boundary (`ZSTD_e_flush`), so the page's streaming decoder gives back every byte
  as it arrives. Browsers have no zstd `DecompressionStream`, so the page's worker runs zstd's
  own decoder (1.5.7's single-file decoder, built for wasm32 with no libc by
  `fleet/web/scripts/build-zstd-wasm.sh`: 59 KB, 22 KB gzipped) in a fixed workspace that
  refuses a larger window. The
  bandwidth shown is the WebSocket messages' bytes; the readout also says the encoding, how
  many times smaller than the commands the wire is, and the share of a core the worker spends
  decoding.
- On the committed Robot Park capture (8.27 MB of tap records), cut where the phone's clock
  passes 16 ms as the streamer's reads would cut it: deflate level 1 sends 2.19 MB (3.8 times
  smaller), zstd -3 1.51 MB (5.5 times). Splitting the stream by channel before compressing
  gives nothing here (1.53 MB): a tap record is already a long run of one channel's bytes.
- `gpu release` (a warm snapshot of the phone) ends every render thread (CLOSE records) and
  the GPU hook stops the streamer, so the mirror ends; after `gpu retake` the page rejoins. A
  VM restore (the phone returned, `reboot_phone`) ends the borrow or relaunches the phone, and
  the page rejoins the relaunched one.

**5. Transport and admission.** `GET /mirror` on the phone streamer, beside `/frames`, on the
same listeners. The first message is `{"admit": "<direct token>"}`, as for `/frames`, and the
URL carries none. It opens only when the coordinator's keys made the token and the token
names the viewer holding the phone: the coordinator puts the holder in the phone's scene
(`mirror_viewer`) and its key in `mirror_keys`, which it sends with or without a broadcast
(without one, `admit_keys` is empty and direct streams stay open, but the mirror is still the
holder's alone); the agent passes both to the phone's streamer, and the streamer ends an open
mirror, its writer included, when the holder changes. No keys, no holder or another viewer's token: closed with 1008. One
mirror per phone at a time (the tap has one consumer).

**6. Test plan.**

| what | test |
|---|---|
| the tap: off copies nothing; data, replies, lifetimes; an older channel starts at a packet boundary; overflow | `host/mirror_tap_unittest.cpp` (the fork), ASan, UBSan and TSan |
| the wire table is what emugen's specs give | `scripts/gen-mirror-table.ts --check`, run by the web tests |
| record and packet framing, parameter decoding | `core/test/mirror.test.ts` |
| the meter's rolling second and totals | `core/test/mirror.test.ts` |
| admission: holder only, keys required, expiry, forgery; the holder kept from the app; zstd round trip within a 1 MiB window | `fleet/phone/streamer/test_host.cpp` |
| the page's zstd decoder: each flushed message whole, a larger window and a non-zstd stream refused, built from the tree's sources | `core/test/zstd.test.ts` |
| the worker: a retired connection's bytes never reach the next, the 16 MiB bound, `OVERFLOW`, no WebGL2 | `core/test/mirror-worker.test.ts` |
| the holder reaches the streamer's scene | `tests/test_fleet_agent.rs` |
| the join: JOIN parts after HELLO and ahead of what render threads copied meanwhile; no handler, a refusal, a failure | `host/mirror_tap_unittest.cpp` (the fork) |
| the join's bytes told from the live stream's, each under its own bound | `fleet/phone/streamer/test_host.cpp`, `core/test/mirror-worker.test.ts` |
| a join's color buffers made with their pixels, the screen drawn at END | `core/test/mirror-gl.test.ts`; `e2e/mirror.spec.ts` against the phone's screenshot (above) |
| the apps running at a join keep drawing from its GL state | `e2e/mirror.spec.ts`, the browser fixture's last frame against the phone's screenshot (above) |
| the phone keeps composing after a join | `scripts/phone/mirror-capture.sh` checks SurfaceFlinger after its join |
| replay: a Robot Park stream captured on a gpubox, with the T4's stamped frames, replayed in headless Chrome on WebGL2; each mirror frame compared with the T4 frame of the same stamp | `e2e/mirror.spec.ts` (`playwright.mirror.config.ts`; threshold below) |
| the panel: three views, video closed in mirror-only, the readouts, the mirror drawing with WebGL2 | `e2e/phones.spec.ts` against the mock coordinator, whose phones serve a synthetic `/mirror` (`mock/mirror.ts`) |

The replay test captures both streams with `GET /mirror` and `GET /frames` exactly as the page
receives them, while `adb screencap` takes the phone's own screenshots (lossless, the screen
HWC composed). Each screenshot carries Robot Park's stamp, its frame number; the mirror frame
of the same number is compared with it, the stamp band left out. Headless Chrome draws WebGL2
with SwiftShader on the CPU, about a second a Robot Park frame here, so the harness rasterizes
only the frames near each screenshot: Robot Park draws every frame whole, and between
screenshots the harness skips draws, clears and blits and replays every other call
(`GlMirror.skipDraws`, which the page never sets).

#### Measured

On a g4dn.metal (T4, driver 580.196.07), the emulator built with ejc3/gfxstream#4, the AOSP
image, a 6 GiB phone VM with a 2 GiB phone, Robot Park at 720 x 1280; both streams captured at
once by a client on the host, as the page receives them (`scripts/phone/mirror-capture.sh`:
the streamer's `/frames` at `q=std` and `/mirror` with a token for the scene's holder). The mirror
joined with an app restart 1 s in. 1-minute load 1.3 to 2.0 on 96 cores.

| | Video (NVENC H.264) | GPU mirror (deflated command stream) |
|---|---|---|
| Robot Park's scene, 30 s | 19.2 Mbit/s, 28.7 fps | 1.74 Mbit/s (7.6 Mbit/s before deflate); 269 blocking calls a second answered by the T4 |
| Settings, a fresh process after the join, scrolled once a second, 22 s | 8.7 Mbit/s, 16.0 fps (a still screen sends less) | 3.38 Mbit/s (11.6 before deflate); 288 blocking calls a second |

The guest posts a composed frame about 59 times a second for Robot Park (`rcCompose`, 1,177
in 20 s), so the mirror can draw at the phone's own rate where the video carries 30. Of the
Robot Park stream, 87 percent is the app's own channel, and most of that is uniforms
(`glUniform3fv` 9.6 MB of 19.4 MB in 20 s). The phone's blocking calls are mostly fences:
`rcCreateSyncKHR` once per app frame (eglSwapBuffers' native fence), and HWC's
`rcCreateSyncKHR`, `rcDestroySyncKHR` and `rcCompose` per composed frame. An independent
count on the decoder side (`docs/fleet/phone-gpu-remoting.md`) measured 0.90 MB/s from the
app, 240 replies a second for the whole system, and matched `strace` in the guest to the
byte (26,191,079 bytes and 1,739 replies over 29 s).

The mirror is not always the cheaper stream. The same measurement over ten workloads
(ejc3/fcvm#1438) found UI at 0.1 to 1.3 Mbit/s of compressed commands against 0.47 to 4.3
Mbit/s of video, but Firefox at 15 to 240 Mbit/s (it unmaps whole buffer ranges, about 1 MB
each, 105 to 117 MB/s raw) and SuperTuxKart at 168 Mbit/s (client-side vertex arrays) against
1 to 2.7 Mbit/s of video, and VLC's video does not pass through GL at all (the host decodes
it), so a mirror of it shows no picture. The panel's readouts show which it is for the phone
in front of the viewer; an app like those two belongs in the video view.

**Pixel match.** The capture in `fleet/web/e2e/fixtures/mirror/robotpark` (6.5 s, the mirror
joined by an app restart 1 s in) replayed in headless Chrome, against the T4's screenshots of
the same stamp, the stamp band left out: frames 315 and 470 (of the capture this one was cut
from) each differ from the T4's in 0.41 percent of pixels by more than 16 levels, by 0.25 levels
on average, at 44.5 dB PSNR. No call failed in WebGL2. The test's bar is 1 percent and 38 dB.
With `glBlendFunc` ignored (the particles' and the glow's blending
drawn opaque) the test fails: 4.1 percent of pixels more than 16 levels off, 17.8 dB.

**Freshness.** With both views open the panel reads Robot Park's stamp from the mirror's frame
and the video's newest frame and shows the mirror's lead. The command stream leaves the phone
when the app submits a frame, before the T4 draws it, where the video waits for the draw, the
screen capture (36 ms p50) and the encode (14 ms to the socket). On staging (2026-11-01,
Chrome on a Mac, 70 to 75 ms from the coordinator) the mirror ran 15 to 43 ms ahead of the
video in 11 of 12 readings over two take-overs, and 98 ms behind once.

The stamp has no checksum, so the page takes the mirror's reading (every 15 posts) as a stamp
only when it names a `t` within 5 s of the scene clock, the streamer's own rule
(`stamp_plausible`): Settings' bottom rows read as a frame number about 1.26 million s off. The
lead also needs a reading from the last second. The streamer crops the 8 stamp rows from the
video; the mirror draws the whole surface, so while its last reading found a stamp its canvas is
8 rows taller than its box and the box clips the band.

### The browser

The owner, 2026-11-03: "I want an option to switch to chromium or a web browser on the phone
so you see it's actually the phone", and then: "load nvidia home page in the phone's mobile
browser as one of the actions you can take".

The take-over panel's **Open nvidia.com** (`open_browser`) brings the phone's web browser to
the front on https://www.nvidia.com/ and loads the page again while it shows; **Robot Park**
(`open_app`) brings the app back. An action for Robot Park (rotate, dance and the rest) brings
it to the front first, and `restart_app` restarts whichever of the two is in front. When the
borrow ends the phone is restored from its snapshot as before, so the next viewer finds Robot
Park in front and the browser behind it on the page the warm snapshot holds (its cookies and
cache are the snapshot's, the same on every phone).

**Which browser.** Cromite 153.0.8010.37, the x86_64 phone build of a Chromium fork
(github.com/uazo/cromite), run as released. `scripts/phone/browser.sh` fetches it and checks
its SHA-256 into the AOSP directory's `phone-work/`, beside Robot Park, so the phone container
installs it from `/work`; the host image's build runs it. Chromium itself was tried first:

| build | on the phone (T4, gfxstream, GLES 3.0 cap) |
|---|---|
| Chromium snapshots `Android`, `Android_Arm64` | arm only; the AOSP image has no ARM translation |
| Chromium snapshot `AndroidDesktop_x64` r1734151 (159.0.8142.0), the project's only x86_64 Android build | its GPU process brings up Skia Graphite on Dawn over Vulkan, which fails (`Requested feature FeatureName::TextureCompressionETC2 is not supported`); with Vulkan and Graphite turned off the GPU process does not start, and the browser aborts (`GPU process isn't usable. Goodbye.`) |
| Cromite 153.0.8010.37 x64, Vulkan features off | GLES on the T4 (`EGL_emulation`, ES 3.0), the page draws, `gpu release` and `gpu retake` work with it running |
| the same, Vulkan left on | it creates a guest Vulkan instance, and `gpu release` refuses (`the guest has 1 Vulkan instance(s)`), so no warm snapshot could be taken |

Building Chromium for x86_64 phones from source would take the place of Cromite; it was not
done here. The browser reads its flags from `/data/local/tmp/chrome-command-line`, which a
userdebug image allows: `--disable-fre`, `--no-first-run`, `--no-default-browser-check`,
`--proxy-server=http://10.0.2.2:3128` and `--disable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE`.
Cromite shows a toast for a flags file at each start ("Cromite is loading flags from a
command-line file"); a start from the warm snapshot shows none.

**In the warm snapshot.** `phone.sh serve` opens the browser on the page before it starts
Robot Park, and waits until its DevTools list (`chrome_devtools_remote`, read over adb inside
the container) names an NVIDIA page; a browser that does not get there fails the phone. So
`open_browser` is the browser's activity coming to the front (`am start -n` of
`ChromeTabbedActivity`; launched as from the home screen after time away, and a clone's clock
has moved on since the snapshot, it opens a new tab page instead), then Ctrl+R. On a gpubox
(g4dn.metal, T4), rootless phone VMs, the page cache warm:

| | the browser in front (`am start -W` `TotalTime`) | the action through `fcvm exec`, as the agent runs it | the reload, navigation start to first contentful paint |
|---|---|---|---|
| a clone of the warm snapshot, the first time | 2653 to 3495 ms | 3311 to 3831 ms | 748 ms |
| the same clone, again | 70 to 263 ms | 379 ms | 1556 ms |
| the phone that was never snapshotted | 136 to 599 ms | 473 to 971 ms | 524 ms |
| `restart_app` with the browser in front (its tabs closed, started on the page) | | 846 to 1145 ms | |

The first time is gfxstream restoring the browser's GL state: after a snapshot load it restores
a share group when one of its contexts is first made current (`ShareGroup::postLoadRestore`),
and the browser's first frame waits for it (HWUI reports one frame of 2672 ms). The source
phone pays it too, once, after its own warm snapshot (3138 ms). Restoring the share groups
before a viewer asks is tracked in #1492.

**Network.** A phone VM is routed over IPv6 only (`--network routed`: no IPv4 leaves the
VM), and www.nvidia.com has no AAAA record (a CNAME to Akamai's edgekey with A records only),
so the phone cannot reach the page by itself. The browser's only way out is the phone web
proxy (`src/fleet/agent/webproxy.rs`), which the agent serves on the host's 127.0.0.1:3128:

```
Cromite (--proxy-server=http://10.0.2.2:3128)
  -> the emulator's user network: 10.0.2.2 is the container's loopback
  -> fc-agent's --forward-localhost 3128 listener in the guest
  -> fcvm's relay in the VM's namespace, to the host's 127.0.0.1:3128
  -> the phone web proxy: CONNECT <name>:443 for a name under nvidia.com only
  -> the site, over the host's own IPv4 or IPv6
```

The proxy answers `CONNECT` and nothing else, for `nvidia.com` and its subdomains on port 443,
and connects only to a global unicast address the name resolves to (not loopback, a private or
link-local network, or the metadata service). Everything else gets 403 and the connection is
closed. The browser speaks TLS to the site through the tunnel, so the proxy sees names and byte
counts. The page's first screen loads from www, images, webassets and brand-assets.cne.ngc
under nvidia.com; the third-party hosts it asks for (`cdn.cookielaw.org`,
`geolocation.onetrust.com`, `assets.adobedtm.com`) and the browser's own (`uazo.github.io`,
`raw.githubusercontent.com` for its filter lists) are refused, and the page draws without them.
What this opens: from a phone VM, HTTPS to names under nvidia.com. A phone VM still has no IPv4
route, Android's own traffic goes nowhere new, wall VMs do not forward the port, and nothing
listens beyond the host's loopback. `--forward-localhost 3128` is in the phone's launch
(`VmSpec::for_phone`, `warm-phone.sh`), so the warm snapshot carries it to every clone.

**With the GPU mirror.** The mirror draws only windows of processes that started after its copy
did (point 3 above), so while the holder's mirror is open (`/health`'s `mirror.open` on the
streamer) `open_browser` starts the browser again on the page instead of bringing the old
process to the front, Robot Park back from the browser starts again too (the mirror may have
joined while the browser was in front), and the mirror's own join (`restart_app` once the
stream opens) restarts whichever app is in front. The browser's command stream, captured as the page receives it
while the browser started on the page and then scrolled it ten times (40 s, the same gpubox):

| | Video (NVENC H.264) | GPU mirror (zstd) | the mirror before compression | blocking calls a second |
|---|---|---|---|---|
| GPU rasterization (Chromium's default) | 2.73 Mbit/s, 16.0 fps | 0.67 Mbit/s | 8.5 Mbit/s | 410 |
| `--disable-gpu-rasterization` | 3.00 Mbit/s, 17.7 fps | 1.96 Mbit/s | 78 Mbit/s | 468 |

Rasterized on the GPU, tiles are drawn with GL calls and the mirror stays below the video; on the
CPU they are uploaded as textures. Firefox's 15 to 240 Mbit/s (`docs/fleet/phone-gpu-remoting.md`)
comes from GeckoView resending whole mapped buffers; Chromium's GPU process does not. The phone
keeps GPU rasterization.

Two things the browser showed wrong in the mirror, both fixed in `app/_wall/mirror/gl.ts`:

- **Rows.** gfxstream's color buffers hold the top row at texture row 0: a window surface
  draws into a pbuffer of its own and `rcFlushWindowColorBuffer` copies it into the color
  buffer turned over (`blitFromReadBufferToTextureFlipped`), and HWC composes with the top row
  first. The mirror drew a window surface straight into its color buffer, so those buffers were
  turned over and every other one (gralloc uploads, Chromium's own render targets, the join's
  pixels) was not. A full-screen app looked right; anything placed by its display frame did
  not: the web content sat 48 pixels low, and after a join the status bar and toolbar, from the
  snapshot, were drawn upside down at the bottom. A surface now has its own framebuffer and the
  flush copies it over turned, as the host does; a post draws the screen turned over.
- **Buffer types.** WebGL2 fixes a buffer's type (element array or other) at its first bind
  and refuses the other kind after; GLES does not. Chromium's GPU process fills an index
  buffer through `ARRAY_BUFFER` and draws with it as `ELEMENT_ARRAY_BUFFER`, and the join made
  Skia's quad index buffer as a vertex buffer, so the bind failed and text drew with another
  draw's indices (glyphs in pieces). A guest buffer now has a WebGL2 buffer of each type it is
  bound as; writes the mirror holds the bytes of go to both, and the first bind of the second
  type copies the bytes through `getBufferSubData`.

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
7. Only the viewer holding a phone receives its GPU mirror, and the mirror never answers a
   call (`test_host.cpp`'s admission checks; the decoder has no reply path).
8. The mirror never makes the phone wait: a render thread's copy is bounded by its own bytes,
   and a full ring or a slow viewer ends the copy, never the phone's work (`mirror_tap_unittest.cpp`).
9. A phone's browser reaches only names under nvidia.com, over HTTPS, through the phone web
   proxy (`webproxy.rs`'s tests; `make phone-test` checks a name off the list is refused), and
   a guest Vulkan instance never blocks the warm snapshot (`make phone-test` releases the GPU
   with the browser running).

### Failure states

| failure | what happens |
|---|---|
| the host's `kvm_intel` has no nested virtualization | the phone slot reports failed at the agent's inventory; `make phone-test` refuses to start |
| the renderer cannot reach the GPU (EGL or Vulkan init fails, or the renderer string is not the GPU) | the slot is failed and dark; never a software fallback |
| detach does not release the GPU within its deadline | the park is undone (as today) and the snapshot is refused |
| attach fails in a clone | the clone fails closed, as a failed GPU attach does, and the slot is relaunched with backoff |
| the emulator exits | the container exits, the VM reports it, and the agent relaunches the slot |
| the app stops drawing | the streamer's frame age passes its deadline and the tile is drawn dim, then failed |
| the browser shows no NVIDIA page at the warm phone's start | the phone is reported failed, and the warm snapshot is not taken |
| the phone web proxy is not running (the agent stopped) | the browser shows its connection error; the rest of the phone is unchanged |

### Licensing

- **The SDK's emulator and system images** are under the Android SDK License Agreement
  (developer.android.com/studio/terms): 3.1 grants use "solely to develop applications for
  compatible implementations of Android"; 3.4 forbids copying except for backup, modifying,
  redistributing or creating derivative works. A public wall of phones is not app
  development, so nothing the phone runs is from the SDK:
  - the emulator (`external/qemu`, QEMU's parts under the GPL) and gfxstream (Apache 2.0)
    are built from our forks (`scripts/phone/emulator.sh`);
  - the system image, `adb`, and the tools that build the probe app (`aapt2`, `d8`, `zipalign`,
    `apksigner`, `android.jar`) are built from AOSP's `android-15.0.0_r36`
    (`scripts/phone/aosp-build.sh`, "The system image from AOSP source"), without Google's
    apps or services. Their components keep their own licenses and notices: the image
    carries its NOTICE file, and the tools archive each tool's license. Most of the platform
    and the tools are Apache 2.0; the Linux kernel and its modules (AOSP's prebuilt GKI
    build in the tree) are GPL-2.0; `d8` is the R8 project's BSD-licensed release in
    `prebuilts/r8`.
  - The archives are published as a GitHub release of this repository; its corresponding
    source is `fleet/phone/aosp.xml` (every project at a commit on
    android.googlesource.com) and, for the kernel, the release notes' kernel commit.
  - Robot Park (`fleet/phone-app`) is built with the same tools: Gradle compiles its Kotlin
    against the AOSP build's `android.jar` (no Android Gradle plugin), and
    `fleet/phone-app/build-apk.sh` dexes it with `d8`, links it with `aapt2` and signs it with
    `apksigner`. Nothing in the build, the phone or a host is from the SDK, so no step asks
    for the SDK license.
- **The Android robot** is CC BY 3.0 with the attribution line above; "Android" and Google's
  marks follow Google's brand guidelines, as under "Branding".

### Stack of PRs

1. Merged (#1400): the phone profile: the Firecracker fork's nested state, the guest kernel
   with KVM, the phone container (emulator with `-gpu host` and EGL-on-EGL), the probe app,
   and `make phone-test`. Then this design's "Detaching the renderer, measured".
2. Done: the emulator from source (ejc3/platform_external_qemu, ejc3/gfxstream,
   `scripts/phone/emulator.sh`), with `gpu release` and `gpu retake` on a running phone,
   tested by releasing (no `/dev/nvidia*` descriptor or mapping left in the emulator),
   retaking, and checking the app's frames. Then the system image, `adb` and the app's build
   tools from AOSP source (#1412), so nothing the phone runs is under the SDK license.
3. Done: snapshots of a running phone: fc-agent's park and unpark hooks for GPU holders that are
   not CUDA processes, `--gpu-warm` of a running phone, and a test that a clone's app renders
   on the clone's GPU; restore-to-first-app-frame measured ("Flow B, measured"). The VM's size
   and hugepages are next.
4. The app's scene (Robot Park, `fleet/phone-app`): the Android-themed scene, deterministic
   by rectangle, its info tab, the action intents and socket, surface-loss handling; the seam
   test in `make phone-test`.
5. The phone streamer: screen capture, NVENC, FCVF frames stamped with `t`.
6. Fleet: phone slots in the agent and coordinator (`--phones`), the phone scene fields,
   `claim_phone` and its actions, phone restore timings in the state.
7. Web: `?mode=phones`, the zoom out, the bezels, the take-over panel.
8. ejc3/aws: the host AMI with the phone image and assets.
9. The GPU mirror ("The GPU mirror", above): the gfxstream tap and the GLES cap
   (ejc3/gfxstream#4); the browser's decoder and WebGL2 replay with the replay test; the
   streamer's `GET /mirror` and its admission; the take-over panel's three views and their
   readouts; the emulator pin. Then FREE records, replies on ring streams and no program
   binaries for a mirrorable guest (ejc3/gfxstream#5), and the join snapshot.
10. The browser ("The browser", above): Cromite on www.nvidia.com in the warm snapshot, the
    `open_browser` and `open_app` actions, the phone web proxy, and `make phone-test`'s checks.
    Then the mirror's drawing of it (#1485).

## Potato mode

Status: **design**, from measurements on a g4dn.metal launched from the fleet host image
(2026-11-05). The owner, 2026-11-05: "a set of VMs, cpu only so fine, whenever they receive
the item, they go to sleep and then wake up. State is maintained across sleep / wake by color
or some such. If the thing that is being passed around goes fast enough or the sleep restore
isn't fast enough / glitchy, the ball drops and the simulation restarts, sadly." Then: "It can
go through a progression from 2 VM's and speed until they break until 96 VM's", "The ball
physically has to pass to the next vm ... (Don't show the conductor)", and, choosing between
a conductor that pings each VM and VMs that throw to each other, "thrower is better".

### What a viewer sees

- `?mode=potato` opens it; without the parameter the wall is unchanged.
- **The ring.** Every player VM is a tile on a circle, filled with its colour. A tile is
  bright while its VM runs, dim while it sleeps (no process exists for it), and shows a bar
  filling while it wakes, labelled with the restore's milliseconds. The ball arcs from the
  thrower's tile to the catcher's. Nothing on the page stands for the coordinator.
- **The pace.** The hold (how long a player keeps the ball before it throws), the lap and the
  ring's size, and a meter of the hold against the sleep-and-wake time it leaves a player.
- **The ladder.** Rings of 2, 4, 8, 16, 32, 64 and 96 players. Each size reached shows its
  record (the shortest hold that completed a lap) and what broke it. The current size and
  pace are live. After 96 breaks, the ladder starts again at 2.
- **The drop.** When the ball drops, a card names the player and the stage that was late,
  with its milliseconds ("player 7 was still waking: restore 412 ms, listening 84 ms after the
  ball's flight ended"), and the next round starts 5 s later.
- **Records.** The fastest full sleep-and-wake hold and the longest round, kept with each
  size's record in the coordinator's state file.
- **Two buttons.** "Faster" shortens the current hold by a fifth; "Restart" starts the current
  size again. Both are rate limited per client as the wall's buttons are.
- **Zoom to a player** (a later PR of this stack). Clicking a tile zooms to it and opens a
  direct connection to its VM, which serves its colour and catch counter live; the
  connection drops while it sleeps.

### The game

A ring of N players, each a small CPU-only fcvm VM (`fleet/potato`: an alpine image holding
one static program, 256 MiB, one vCPU, no GPU) at its own address,
`<host prefix>::<0x1000 + player>` in fcvm's routed mode. The player program keeps three
things in guest RAM and nowhere else: its colour (an index into a ring of 12), a catch counter
and a hash chain over every ball it caught. Every player of a host is a clone of one snapshot
of an unassigned player; the agent gives a new clone its number over its ball port, and the
player then draws its first colour from the guest's random pool.

1. **Catch.** The ball arrives over TCP from the previous player's VM, one JSON line:
   `{"type": "ball", round, seq, ring, hold_ms, shrink_permille, min_hold_ms, flight_ms,
   trail}`. The catcher steps its colour forward one, increments its counter, extends its
   chain (`sha256("fcvm-potato-v1" || chain || round || seq || colour)`, with `chain` the 32
   bytes, `round` and `seq` 8-byte and `colour` 4-byte little-endian integers:
   `fleet_potato::chain_step`), acknowledges the ball and reports the catch with its new
   colour, counter and chain.
2. **Throw.** After `hold_ms` it connects to the next address in `ring` and sends the ball
   with its colour on the trail. Nothing relays the ball: the agent and the coordinator only
   read reports. If the connection is refused or not answered, the thrower tries again every
   2 ms until `flight_ms` has passed since the first try, then reports the ball dropped.
   From the moment it throws until it is woken, a player refuses the ball: it is going to
   sleep, and a ball it caught in the few milliseconds before the snapshot pauses it (or
   the 1.8 s a slow btrfs commit can add) would be held by a VM that nobody wakes for it.
   One ladder run stalled exactly that way. A catcher that gets the same ball twice (its
   acknowledgement was lost and the thrower tried again) counts it once.
3. **Sleep.** Once the ball has left, the agent puts the thrower to sleep with
   `fcvm snapshot create --stop`: the VM is paused, saved, never resumed, and its fcvm
   exits. A sleeping player holds no CPU and no memory, only its snapshot on the instance
   store. Plain `snapshot create` resumes the VM it saved; a ball that reaches the player
   between that resume and its teardown is caught by a VM whose state no snapshot holds,
   and the catch is lost. The first ladder run lost one that way, and in the same window
   another player threw a second time from the VM being stopped.
4. **Wake.** Before the ball comes round again, the agent restores it
   (`fcvm snapshot run --snapshot`). The restore retires the connections the snapshot held,
   so the player's report connection fails, it connects again and says hello with the
   colour, counter and chain it woke with. Once its ball port answers (fcvm's restore opens a
   clone's inbound gate last), the agent tells it it woke, and from then on it catches. A
   round that ends reaches every awake player before its sleep (`stop`): from then on it
   refuses every ball until it is woken, and a throw waiting for its hold is abandoned when
   the hold ends. A hold the snapshot paused ends after the player's next wake, when it may
   already hold the next round's ball; the agent puts it to sleep for that abandoned throw
   only if the round being played does not need it (it holds the ball, or the ball reaches
   it within the lead).
5. **Verify.** The coordinator replays the step from the last values it verified: a catch's
   colour must be the last one plus one, its counter the last plus one, and its chain the
   step of the last chain. A hello after a wake must equal what the player reported at its
   last catch, or be one catch ahead of it when that catch's report is still on its way
   (the hello names its last catch, and the step is checked from it). Any other difference
   means a player's memory did not survive the sleep.
6. **Pace.** Each round starts at a hold set for its size and shrinks it by a tenth every
   lap, down to a floor, until the ball drops.

The ball drops when the thrower's flight window runs out (the catcher was asleep, waking,
or not yet accepting), when a proof does not match, when a wake fails, or when no catch is
reported by the round's deadline (the report path is gone). A size's record is the shortest
hold of a lap that completed; then the ring grows to the next size.

**What sets the limit.** A player throws one hold after it caught and catches again N holds
after that catch, so between its throw and its next catch it has (N - 1) holds to sleep, be
torn down and wake: `sleep + teardown + wake <= (N - 1) x hold - margin`. A ring of 2 has
one hold for the whole cycle; a ring of 96 has 95. Larger rings can go faster per hold, until
the host can no longer run that many sleeps and wakes at once: at a hold of h ms, a host runs
about `(sleep + teardown + wake) / h` of them concurrently.

### Measured: one player's sleep and wake (2026-11-05)

g4dn.metal (96 vCPU, instance-store btrfs), the fleet host image, fcvm at the image's
commit (04915961; the third arm and the ladder with this stack's `snapshot create --stop`),
Firecracker 1.17 (the default profile's fork), one session. One player at a time, 240 cycles
in each arm, rotating over 4 players: ball in from the host, catch, throw to a host
listener, sleep, wake. Every one of the 720 wakes returned the colour, counter and chain the
player had before its sleep. `fleet/potato/bench/potatobench.py cycles` runs it;
`summarize.py` prints the table; `fleet/potato/bench/results/2026-11-05-g4dn-metal.json`
holds the summaries.

| stage, ms | diff p50 | diff p99 | full p50 | full p99 |
|---|---|---|---|---|
| sleep: `fcvm snapshot create` | 203.8 | 1616.8 | 394.9 | 592.2 |
| of which: close the guest's network gate, pause, save memory | 86.9 | 165.7 | 299.9 | 331.7 |
| of which: pause (Firecracker) | 0.1 | 0.2 | 0.1 | 0.2 |
| of which: memory save (Firecracker) | 63.5 | 141.8 | 281.4 | 308.5 |
| of which: disk snapshot, resume, reopen the gate | 71.7 | 141.9 | 27.3 | 55.6 |
| memory written per sleep (MiB) | 19.5 | 67.7 | 240.7 | 250.4 |
| teardown (SIGTERM to fcvm's exit) | 108.8 | 146.7 | 107.5 | 138.8 |
| wake: `fcvm snapshot run` to the ball port accepting | 326.1 | 383.0 | 196.3 | 294.1 |
| of which: routed network setup | 102.9 | 133.1 | 105.4 | 132.4 |
| of which: the clone's disk | 13.7 | 28.3 | 11.7 | 15.4 |
| of which: Firecracker start and snapshot load | 9.4 | 17.4 | 9.2 | 21.2 |
| of which: guest restore until fc-agent acknowledges | 194.4 | 225.5 | 63.9 | 161.0 |
| the ball, VM to host (connect, send, acknowledgement) | 0.6 | 1.2 | 0.6 | 1.2 |
| whole cycle, ball in to awake again | 647.6 | 2275.0 | 707.7 | 938.5 |

"Diff" is fcvm's default: a restored VM tracks its dirty pages, and the next snapshot writes
only those into a reflink of the previous memory file. "Full" restores with
`--no-dirty-tracking`, so each snapshot writes the whole memory. The diff writes a twelfth
of the bytes and sleeps in half the time, but its wake is 130 ms slower: the restored VM's
first write to each page traps for KVM's dirty log, and the guest's restore path does a lot
of first writes (fc-agent's acknowledgement came 217 ms after the resume with tracking on,
84 ms with it off). Sleep, teardown and wake together: 639 ms p50 for diff, 702 ms for full.
Potato uses diff; a diff written without KVM's dirty log is below, under "Cheaper sleep".

The sleep the game uses is `snapshot create --stop` (below), measured as a third arm of 240
diff cycles: sleep 256.9 ms p50 (398.6 p99), teardown 106.7 (141.3), wake 355.3 (1894.5),
the whole cycle 728.2 (2321.6). Its log line splits the sleep: closing the guest's gate
17.6 ms p50, pause 0.5, memory save 74.5, the disk 114.7, and the stop 106.7. That run's
btrfs steps were slower than the first arm's (the disk step 115 ms against 72 at p50), after
the 96-player run below had left many subvolumes to the cleaner, so the arms are not an A/B
of `--stop`; it is in the game for correctness.

**The tail is btrfs.** Five of the 240 diff cycles took 1.6 to 2.5 s, and two of the full.
In each, one of the cycle's three btrfs subvolume snapshots waited 0.9 to 2.0 s for a
transaction commit: the snapshot's staged copy of the previous snapshot (before the save),
the snapshot's disk (while the VM is paused), or the clone's disk (during the wake). One ring
at 2 players loses the ball to one such stall.

### Measured: 96 players on one host

The same host, 96 players booted at 8 at a time, then all put to sleep, then woken one at a
time with the other 95 asleep (`potatobench.py memory`):

| | processes | PSS of the players' process trees | host `MemAvailable` |
|---|---|---|---|
| before any player | 0 | 0 | 372.9 GiB |
| 96 awake | 96 trees | 23.9 GiB | 348.7 GiB |
| 96 asleep | 0 | 0 | 370.9 GiB |
| 1 awake, 95 asleep (12 samples) | 1 tree | 133 to 140 MiB | 370.8 to 372.5 GiB |

A sleeping player is two files on the instance store and nothing else. The awake player maps
its memory file privately and faults in what it touches, so it holds 133 to 140 MiB of its
256 MiB, against 255 MiB per player for one booted with `podman run`. The wake with 95
asleep took 265 to 329 ms to the ball port accepting (12 wakes), the same as with 3 asleep.
Sleeping 8 players at once took 650 ms p50 (1098 ms at most) instead of 204 ms, and their
teardowns 195 ms: concurrent sleeps contend, which is the limit large rings meet.

### Measured: the ladder

The same host, the whole game with the bench playing the agent's part
(`potatobench.py ladder`): 96 players booted, then each size from 2 to 96 played until the
ball dropped, from a hold of `max(2000 / (size - 1), 250)` ms shrinking a tenth per lap, a
100 ms flight window, and each sleeping player woken when the ball was its lead away. The
ball went from VM to VM over the host's routes. Two runs of the same configuration in one
session:

| size | first run: record hold, ms | second run: record hold, ms | second run: dropped in lap (its hold, ms) | second run: wake p50 / p99 in the round, ms | second run: sleep p50 / max, ms |
|---|---|---|---|---|---|
| 2 | 1313 | 700 | 11 (630) | 347 / 371 | 349 / 944 |
| 4 | 395 | 289 | 9 (261) | 418 / 1907 | 383 / 2133 |
| 8 | 257 | 257 | 2 (232) | 455 / 725 | 661 / 1262 |
| 16 | none (a bench defect, fixed) | 165 | 5 (149) | 553 / 1277 | 1164 / 2317 |
| 32 | 225 | 203 | 3 (183) | 540 / 1633 | 943 / 2168 |
| 64 | 225 | 250 | 1 (225) | 627 / 1723 | 667 / 2812 |
| 96 | 250 | 250 | 1 (225) | 860 / 3439 | 833 / 5868 |

The record is the hold of the last lap that completed: the lap before the one the ball
dropped in, whose hold follows from the start and the shrink. The bench logged each catch
with the hold of the last throw it had read, which on separate report connections can be a
lap behind, so the records are computed from the drop's lap rather than read from that log
(`ladder_summary.py` does so now; the first draft of this table took them from the log and
was one shrink step high in nine of its rows). Every round ended with the catcher
not yet accepting when the ball came: still waking, except the first run's size 16, where
the bench defect left an awake player refusing. Two limits show:

- **Small rings are set by the tail.** A ring of 2 has one hold for the whole cycle. The
  second run's ring of 2 completed 11 laps, the last at 700 ms, about one sleep and one wake
  (349 ms and 347 ms); the first run's dropped at 1182 ms because one sleep spent 808 ms
  waiting for a btrfs commit before its pause.
- **Large rings are set by the host.** From 16 players up, several players sleep and wake at
  once, and each takes longer: sleeps of 0.7 to 1.2 s at the median and up to 5.9 s, against
  0.26 s alone. From 8 players up the record stays between 165 and 257 ms whatever the size.
  Rings of 64 and 96 broke in their second lap at the 250 ms start, so their edge is above
  it.

Turning btrfs subvolumes off for the cycle did not remove the tail. A build that made every
snapshot and clone directory an ordinary directory, so that the cycle took reflinks only,
ran 240 cycles with the sleep's setup at 1366 ms p99 and a clone's disk at 1771 ms at most,
as with subvolumes, and its median disk steps were slower (the clone's disk 45.5 ms against
30.8). The stall is the transaction commit, which reflinks wait on as well. That build is not
part of this stack. Reflinking only the wake's disk is another matter: with the snapshots
still on subvolumes, four wakes and four sleeps at once (`potatobench.py burst`), the wake's
disk step took 17 ms p50 and 192 ms p99 as a reflink against 193 ms and 1.8 s as a subvolume
snapshot, and fcvm now reflinks a clone's disk of at most 4096 extents.

A soak of the coordinator and one host shaped like staging (96 players, two phones, the
ladder from 2 to 96, 2026-11-07) put a number on the host's side. Sleeps finished at up to
about 4 a second. Rings of 64 and 96 at holds near 250 ms ask for more (4.2 to 4.6 a
second), so sleeps queue: in one 30-second stretch completions fell to 2.6 a second with 54
sleeps running, the median sleep reached 9 to 10 s, and the ball dropped on a player still
going to sleep. The coordinator's sleep lines put the time in the btrfs steps (`stage`, `disk`,
`publish`) and in Firecracker's save, all waiting on transaction commits: bpftrace counted
972 in 240 s, most taking 128 to 512 ms, 684 of them from subvolume snapshots and 287 from
fsyncs of memory files in new subvolumes. Two changes did not move the tail and were not
kept: reflinking a diff's memory file into an ordinary directory (a sleep's p99 9.6 s against
10.9 s), and running at most 4 sleeps at once (p99 16.1 s against 9.1 s, the queue moved
from btrfs into the agent). These runs predate the guest image without maintenance timers,
whose fwupd OOMs stalled a few sleeps further.

### The starting pace

Each size starts at `max(2000 / (size - 1), 400)` ms of hold and shrinks a tenth per lap
(`PotatoConfig`), with a 2 ms floor and a 100 ms flight window:

- A ring of 2 starts at 2000 ms, about three times the 700 ms one sleep and one wake take,
  and reaches that edge in about ten laps (some 20 s) unless the tail ends it sooner.
- From 8 players up the measured edge is near 165 to 257 ms, set by the host, so `2000 /
  (size - 1)` would start below it; 400 ms starts each above its measured break and gives it
  a few laps to find it. At 96 players a lap at 400 ms is 38 s.
- The flight window is 100 ms: long enough for a thrower to retry a few times against a
  catcher that is opening its port, short enough that a ball is not caught by a player
  restored long after its turn.

### Components

- **The player** (`fleet/potato`): the program above, a static musl binary in an alpine image
  (`make potato-image`). It also answers a `probe` on its ball port with its colour and
  counter, which is how the agent learns a woken player accepts connections.
- **The agent** (`fcvm fleet agent --potato-players N`): runs the host's players. It reads
  every player's reports on `127.0.0.1:7071` (each VM forwards that port to the host), puts
  a player to sleep once it has thrown, and wakes each sleeping player when the ball is
  `lead` catches away, `lead` being the slowest of its recent wakes plus a margin, in holds.
  A finished sleep checks the lead again. It serves a new round's first ball to its first
  player, the only time the ball leaves anything but a player. Every report goes to the
  coordinator with the host's timestamps and the stage timings of the sleep or wake it
  ends. All players are clones of one snapshot of an unassigned player, so a new player is
  a restore rather than a boot; the agent tells each its number over its ball port.
- **The coordinator.** The game is part of `fleet_core::apply`: the agent's reports and a
  `tick` carrying the time are events, so drops, deadlines and records replay. It sends the
  agent a desired round (number, ring, starting hold, shrink, floor, flight window), which
  the agent reconciles like a slot assignment. The ladder, each size's record and the
  overall records are in the state file; a round in progress is not, and a restarted
  coordinator starts a new round at the size it was on. Its log (target
  `fcvm::fleet::potato`) holds a line for each round's start and end, with what ended it,
  for each ball nobody caught, and for each sleep and wake when it ends (`potato player
  slept`, `potato player woke`: the player, its start and end on the host's clock, its
  duration and stages), so a round can be read without the host's logs.
- **The page.** A view model in `fleet/web/core` from the `/live` snapshot's `potato`
  section, the mock coordinator's potato game, the ring, the ladder and the drop card,
  and Playwright tests on desktop and phone layouts.

### Invariants (each becomes a test)

1. A woken player's hello holds the colour, counter and chain it reported at its last catch,
   or those stepped by exactly one catch whose report is still on its way (its hello names
   that catch's round and seq).
2. Every catch's colour, counter and chain are the step of the last verified ones; a
   mismatch drops the ball and names the player.
3. A player is woken only after its previous fcvm has exited, and is never in two VMs.
4. A player does nothing after its snapshot: the sleep leaves it paused and stops it
   (`snapshot create --stop`; its test reads the source's Firecracker log for a resume).
5. The ball passes only from player to player, apart from a round's first throw by the agent.
6. A sleeping player has no process on the host.
7. The game is replayable: the same events give the same drops, records and state.
8. Each size's record and the overall records survive a coordinator restart.
9. The ball drops on each of: the flight window running out, a proof mismatch, a failed wake
   and a missed deadline.
10. "Faster" never shortens the hold below the floor, and both buttons are rate limited.
11. A player that has thrown refuses the ball until the agent wakes it; a repeated ball counts
    once; a stopped round's throw is abandoned; a connection that sends nothing never holds
    up the ball (`fleet/potato/tests/player.rs`).

### Failure states

| failure | what happens |
|---|---|
| the catcher is not accepting when the ball arrives | the thrower retries within `flight_ms`, then reports the drop; the agent reports when the catcher did start accepting |
| a woken player's hello does not match | the ball is dropped as a proof mismatch naming the player; the coordinator takes what it reported as its state from then on |
| `fcvm snapshot create` or `snapshot run` fails | the round ends with a failed sleep or wake naming the player; the player is booted fresh |
| no catch reported by the deadline | the round ends as a missed deadline; the agent sleeps every awake player, so a ball still in the air drops |
| the agent disconnects | the round ends; the agent stops every player's VM, as it stops slots |
| the coordinator restarts | the agent's round is replaced by a new one at the same size; records stay |
| the fleet sleeps | the host goes with it; the next host boots new players (new colours) and the ladder continues from its size |

### Cheaper sleep

Measured above, and each a later PR of this stack, to be measured on the ring:

- **A diff without the dirty log.** A restore maps the memory file `MAP_PRIVATE`, so every
  page the guest wrote since is an anonymous page of that mapping. Firecracker could read
  the written set from `/proc/self/pagemap` instead of KVM's dirty log, which would keep
  the diff's 20 MiB writes and give the wake back the 130 ms the dirty log costs.
- **No btrfs commit in the cycle.** Reflinks instead of subvolumes did not help (above); what
  would is keeping the commit out of the sleep's path: the snapshots of the players on a
  filesystem of their own, or the memory written somewhere a commit does not wait on.
- **Keep the network across a sleep.** The routed network costs 103 ms of the wake and
  46 ms of the teardown, and a player's address never changes.

### Stack of PRs

1. This design, the player program and the measurement harness (`fleet/potato`).
2. fcvm: `snapshot create --stop`, and one log line with the snapshot boundary's step times
   (prepare, pause, save, disk, resume), which the agent reports in a drop's breakdown.
3. Agent: `--potato-players`, the player snapshot, sleep and wake, reports.
4. Coordinator: the game in `fleet_core::apply`, its test vectors, the agent messages and
   the `potato` section of the snapshot (fleet/PROTOCOL.md).
5. Web: `?mode=potato`, the mock coordinator's game, the view and the e2e tests.
6. ejc3/aws: the host image with the player image.
7. Cheaper sleep, above, one PR each, measured on the ring.
8. Zoom to a player: a server in the player VM for viewers, apart from the ball's port,
   handled only after a catch; the viewer cap set by measuring the wake with 0, 10, 100 and
   1000 viewers reconnecting at once.

## Windows mode

`?mode=windows` shows disposable Windows desktops, each a Cloud Hypervisor VM restored from a
snapshot of an idle, logged-on desktop, with the time from the click to the first desktop
frame on each tile. The design is its own document: `docs/fleet/windows-sandboxes.md`.

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
   renderer or aggregator that holds an admission key streams to no client without a valid
   one. A fleet renderer takes a scene only from a holder of the scene token, and streams
   nothing before its first scene.
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
never sleeps during a race, a "Your own GPU" session or a borrowed phone. Running cost is then about $5 an hour
while anyone watches (one g4dn.metal at current spot prices, plus EC2 tiles while they run).

**Open tabs alone do not keep it awake (2026-11-04).** The owner: "If no new viewers come in
an hour that should shut things down." Staging had stayed awake for hours on GPU hosts and
tiles because tabs left open counted as active viewers. So the fleet also sleeps
after an hour (`--sleep-without-new-viewer`, default 3600 s) with no arrival, even while
viewers stay connected. An arrival is a new viewer connection or a connected page's Wake
button (the `wake` intent); a page that reconnects or reloads presents its seat ticket and
is not one (fleet/PROTOCOL.md, `fleet.power`). A page connected while the fleet sleeps shows
the sleep card with a Wake button, on the wall and in phones mode. A claim or race in progress
holds the fleet awake past the hour, and it sleeps when that ends.

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
