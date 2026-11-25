# Minecraft servers: `?mode=minecraft`

Status: **design and prototype**, the first PR of its stack. The owner, 2026-11-08: "Research
Minecraft port to rust. A lot of these demos are Minecraft, look those up and let's write / use
a more efficient implementation if available so more than one per core possibly and create a
similar demo as others."

Other sandbox demos use a Minecraft server because a world is state a viewer can see: someone
builds something, the VM is copied, and the building is on the copy. CodeSandbox clones a running
Minecraft VM; PandaStack forks a booted game server per match. This mode shows many servers per
core, each in its own fcvm VM restored from one snapshot with its world loaded, and a live fork
of a running server next to its source.

## What a viewer sees

- `?mode=minecraft` opens it; without the parameter the wall is unchanged.
- **The tiles.** Every server is a tile showing its world in 3D, drawn by the page from the top
  block of each column: plain boxes, one flat colour per block material, with each player as a
  block figure and its name. Each tile's camera flies through its own world, cutting every 10 s
  between a bot, something the bots built and the whole region, and to a column that just
  changed. A fork's picture flies from its source to the tile taking the copy for as long as the
  fork runs, and the copy then outlines and counts the columns that differ from its source. A
  browser with no WebGL2 gets the map from above, and the tile says so ("The page in 3D" below).
  Three bots walk about each world's dry land and build walls, the outlines of houses and
  towers of their own colour. A tile is
  labelled with its slot (`mc-3`), its ticks per second, and "Up in X.XX s": the time from the
  click that asked for a fresh server to the first chunk its bots received.
- **The header.** Servers up, the server's name and version, the median and slowest time to
  first chunk of the last restore of each, the last fork with its time, and the servers'
  address for Java players.
- **Reset.** Clicking a tile's reset throws its server away and restores a new one from the
  snapshot; "Reset all" does every server at once.
- **Take over.** A viewer borrows one server (`claim_minecraft`) and drives its first bot from
  the keyboard: W A S D to walk, the arrows or the mouse to turn, space to jump, shift to
  sprint, E to place a block, Q to break one. The panel shows the time from each key press to
  the first change the page drew from it. When the borrow ends the server is reset.
- **Fork.** "Fork" on a tile snapshots its running server and restores the copy into the tile
  next to it (the next one on the same host that is running and not lent), so both worlds hold
  the same towers and then diverge. The target tile shows "Forked from mc-2 in X.XX s": the
  click to the copy's first chunk, split into the snapshot and the restore.
- **Join.** People who own Minecraft Java Edition can join any tile's server at its address
  (`mc-3.<zone>`, port 25565, version 26.3) and build next to the bots.

## The server: Pumpkin

[Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) is a Minecraft Java Edition server written in
Rust, GPL-3.0, pushed to daily. It is built from a pinned commit of the ejc3/Pumpkin fork
(`PUMPKIN_COMMIT` in the Makefile, `1aa35ecc`): upstream `3ea7d281` of 2026-11-08, which speaks
Java Edition 26.3, protocol 777, and only that, plus one commit that closes a connection which
does not finish logging in within `login_timeout` (upstream holds it for as long as the client
keeps it open; "Public joins" below). Its README now lists world saving, lighting and world borders as
done; chunk generation, redstone, combat and commands are partly done. The demo needs what it has:
world generation from a seed, chunk sending, movement, block placing and breaking, the tab list
and creative mode.

The other Rust servers were looked at and not used: Valence is a framework for minigame
servers with no vanilla mechanics (last push June 2026), and Feather has been stalled since
2024. Writing our own server would be a recreation of the game.

Pumpkin's configuration (`fleet/minecraft/src/pumpkin.rs`): offline mode (no account checks,
so the bots need no accounts; see Licensing), creative mode for everyone, no spawn protection,
a view distance of 6 chunks, a fixed seed (`fcvm1`, which spawns in a forest by a lake) so every
tile is the same world, and nothing that reaches out: Pumpkin's telemetry heartbeat (on by
default), the Bedrock listener, LAN discovery and plugins are off.

## The tile program: `mctile`

`fleet/minecraft` is one Rust program beside Pumpkin in each tile's image. It starts Pumpkin as
its child and ends when Pumpkin ends, so the VM fails and the agent relaunches it. It is its
own Cargo workspace: its bot client needs nightly Rust, and its dependency tree stays out of
fcvm's lockfile.

- **Bots.** [azalea](https://github.com/azalea-rs/azalea), an MIT-licensed Rust bot client, from
  its 26.3 branch with two fixes (`ejc3/azalea`, branch `fcvm-26.3`). The swing animation
  packet's entity id is a VarInt; without the fix every swing breaks the packet stream. The level
  particles packet is read in 26.3's field order (the particle first, a max speed per axis, a
  VarInt count and a randomization type); without the fix the bots read the particle id as a
  boolean and log "Boolean value was not 0 or 1" for every bubble particle. Both bugs are
  reported upstream in [azalea-rs/azalea#404](https://github.com/azalea-rs/azalea/issues/404),
  the swing bug in the issue and the particles bug in
  [a comment](https://github.com/azalea-rs/azalea/issues/404#issuecomment-6841991366); once
  upstream fixes both, the pin moves back to azalea-rs/azalea. Three bots join over
  the guest's loopback. A bot that nobody drives walks with azalea's pathfinder to a column of
  dry land that has dry land all round it, and builds there in its wool (`plan.rs`): a wall of 4
  to 7 columns, the outline of a house 4 or 5 columns a side with a gap for its door, or a tower,
  each raised two blocks (a tower three). It walks to the place beside each column before it
  raises it, and leaves out a column on water, on a tree, or off the level of the first. A bot's
  targets and standing places are dry columns, and one that is in water swims or walks to the
  nearest shore before anything else; a walk between two of them can cross water, and that is
  how it leaves again. After a restore or a new scene a bot's first walk is at most 6 columns, so a
  copy starts building, and differs from its source, within seconds. A restore keeps
  the bots connected: their connections to Pumpkin are loopback inside the guest, which fcvm's
  restore keeps (it retires only sockets whose peer is outside the guest). When the agent posts
  the copy's scene, the program reseeds its choices from the guest's random pool, the tile's
  name and its generation, so a fork and its source do not walk the same paths.
- **The map.** The bots share one view of the world. Every 200 ms the program reads the top
  block of every column of a 128 x 128 region around the spawn (the chunk heightmaps the server
  sends, then the block below the first empty one) and turns its registry name into one of 48
  materials of our own (`palette.rs`: water, grass, sand, wood, leaves, the 16 dye colours and
  so on). The page holds the colours. No texture, colour table, sound or other asset of the game
  is used anywhere. Beside each top block the map carries the column's ground: the highest block
  that is not leaves, a plant or water, up to 48 blocks down, so the page draws the forest floor
  under a canopy and a lake's bed from what is there. A column whose top block has not changed
  keeps the ground it had; one row in 16 is read again each time, so a block placed under a
  canopy shows within 3.2 s.
- **Ticks per second**, as a client measures them: the server's game time in its `set_time`
  packets against the program's own clock over 10 s.
- **The view socket** (`GET /view`, fleet/PROTOCOL.md "Minecraft tile"): the page's only route to
  the tile. Its first message is the viewer's direct token, checked as a renderer checks one;
  then the tile sends the whole map, the changed columns, and 10 times a second the players and
  the ticks per second.
- **The take-over** is on the same socket. Only the viewer the agent named as the tile's driver
  (`POST /scene`, from the coordinator's borrow) may send drive messages, and the socket's
  viewer is the one its direct token names, so no other page can drive. A message names the keys
  held, where the bot looks and at most one action (place or break); the keys are a lease that
  ends 1 s after the last message, so a lost key-up never leaves a bot walking. No message
  carries text or a command.

## The wall mode

### Components

- **The agent** (`fcvm fleet agent --minecraft N`): runs N servers on a host, named in its hello
  (`minecraft`), each an fcvm VM of the `fleet/minecraft` image in routed mode with its own
  public IPv6 address. With `--tls-publish-base` and the host's public IPv4 address, each server's
  view socket (443 in the VM) is also published on the host at the base plus the Minecraft
  range's offset plus its index (`fleet/published-ports.json`), behind the host's IPv4 front on
  443, which routes `mc-<n>-<token>-v4` there by TLS server name: a viewer with no IPv6 reaches
  the tile on 443, as it reaches a GPU slot or a sandbox. The game port (25565) is IPv6 only and
  not part of the front. At
  start it boots one gold VM (`mc-gold`), waits until its `/health` says
  ready (the server listens, every bot spawned, every column of the map known) and the tick rate
  has read 20 for 5 s, and puts it to sleep with `fcvm snapshot create --stop`. Every server is
  a `fcvm snapshot run` of that snapshot at its own address. A server relaunches when its
  desired generation moves (a reset, a borrow ending), and after a failure with a backoff of 1 s
  doubling to 30 s. A fork (the generation moves with `fork` set) is `fcvm snapshot create` of
  the source's running VM (the source is not paused for longer than the snapshot's own pause and
  is never stopped), then the target's VM stopped and the fork restored at the target's address,
  then the fork's snapshot removed. The agent posts each server's scene (its slot, its
  generation, the admission keys, its driver) and reports `restoring`, `forking`, `running` or
  `failed` with the restore's timings and the time to the first chunk.
- **The coordinator.** `state.minecraft` mirrors `state.windows`: tiles with `frame_url` (the
  view socket), `join`, `phase`, `last_relaunch` (click to first chunk), the borrow, and the last
  fork. Intents: `minecraft_reset` (one, or all), `claim_minecraft`, `release_minecraft`,
  `minecraft_fork`. The borrow rules are `claim_windows`'s. Every event goes through
  `fleet_core::apply`, so it replays. Driving is not an intent: it goes from the page to the tile.
- **The page.** A view model in `fleet/web/core` from the snapshot's `minecraft` section, the
  tile renderer (3D on one WebGL2 canvas from the tiles' view sockets), the take-over panel with its key
  handling and timer, the mock coordinator's servers and a mock tile, and Playwright tests on
  desktop and phone layouts.

### Many servers on one host

A host can run hundreds of servers, far more than a page draws. The snapshot would grow with
them (512 servers listed whole are 271,732 bytes, 525 a server, and every restore sends it
again), so past 16 servers it lists 16 as tiles and names every server in `minecraft.field`
(15,810 bytes at 512; `make fleet-snapshot-size`). What each server is doing travels outside
the snapshot: the agent reads every server's `/health` once a second (64 at a time, 400 ms
each) and sends one row of 8 bytes a server with the host's own CPU and memory readings; the
coordinator keeps each host's latest rows, in no state and at no version, and passes them to
the pages that asked (PROTOCOL.md, "Many servers: the field and the status rows"). A server
whose `/health` did not answer has a row that says so. A host that stops reporting leaves its
servers unreported, not at zero and not as they last were: the coordinator stamps each report
with when it came and sends rows older than 5 s to no page, and a host whose last server is
unassigned says so in one last report. A lent server is always one of the tiles, whatever its
number: its holder's page needs its borrow, its join address and its own view.

**Measured through that path**, 2026-11-24, on one boot of one bare-metal arm64 host
(c8g.metal-48xl: 192 vCPUs, 377 GiB, no local disk), kernel 7.0.0-1018-aws, fcvm from main at
`2f89b1c97` with this change: one coordinator, its local agent with `--minecraft N` (each server
a VM of 1 vCPU and 1024 MiB with its three bots, routed networking) and a client that reads the
status rows as a page does (`fleet/minecraft/bench/fleetfield.sh`). The records are in
`fleet/minecraft/bench/results/2026-11-24-fleet-path-c8g-metal-48xl/`, one directory a run, and
every number in the table is from a run's `summary.json` (the run that never had every server
running also keeps its `samples.jsonl`, whose last line is its row). The store was a btrfs image: on the host's
30 GiB root volume (network storage) for the first three runs, in RAM for the last two. The
IPv6 neighbour limit was Ubuntu's 1024 for the first two runs and 16384 after.

| run | all N up, from the first restore's start | busy CPUs a server, minute 1 / 5 / 29 | MiB a server at 5 / 30 min | readings under 19.5 TPS or unanswered |
|---|---|---|---|---|
| 256, root volume | 24.6 s | 0.195 / 0.206 / 0.165 | 412 / 441 | 327 of 89,344, all between 5.2 and 6.5 min |
| 512, root volume, limit 1024 | never: at 347 s, 500 ran, 2 were restoring and 10 had failed | | | 325 of the 500 running unanswered at 347 s |
| 512, root volume | 73.7 s | 0.193 / 0.193 / stalled | 432 / stalled | stalled at 14.6 min (below) |
| 512, RAM | 29.9 s | 0.262 / 0.245 / 0.199 | 492 / 543 | 43 of 179,200, all in the first 18 s |
| 256, RAM (100 s) | 6.9 s | 0.201 | | 0 of 5,120 |

- **All N up** is from the coordinator's own log (`all_up_s` in a run's summary; a run keeps
  the lines it is read from as `coord-up.log`, and a test derives it from them again): from the gold
  snapshot being made (the 512 RAM run made it) or, where it was already there, from the agent's
  hello, to the last server's first "server up". The client's own figure (`restore_all_s`)
  starts at its first sample that shows a restoring server (it samples every 5 s once rows
  come), and in the 512 RAM run that sample came about 20 s after the gold was made: it read
  12.2 s there.
- **A reading** is one server's tick rate in its status row, read every 5 s from when every
  server ran until the run's reset of all; a running server whose row has no rate is
  unanswered. Busy CPUs are the host's (`/proc/stat`), and memory is the VMMs' proportional
  anonymous memory (`smaps_rollup` `Pss_Anon`). The snapshot's memory image is shared and not
  in it: a VMM's resident memory was 730 MiB at 5 minutes with 512 in RAM.
- **512 servers held 20 TPS for half an hour** with the store in RAM: after the first 18 s the
  lowest of 177,152 readings was 19.5. A third of the host's CPUs were idle (129 of 192 busy at
  5 minutes, 100 at 30), and 56 GiB of memory were available at 30 minutes, with 35 GiB of the
  rest holding the store and the build tree in RAM.
- **Reset all.** 512 servers were back 64.8 s after the click: none at 10 s, 2 at 20 s, 55 at
  30 s, 184 at 40 s, 364 at 50 s, 479 at 60 s. 256 took 16.0 s.
- **The store.** On the root volume the worlds' first saves
  showed in the tick rate (256 servers: every low or unanswered reading fell in two windows at
  5.2 and 6.4 minutes; outside them the lowest was 19.8), restores took four to six times as
  long, and 512 servers stalled: 14.6 minutes in, status reads began to go unanswered, and at
  15.3 minutes 499 of 512 were. 513 of the host's processes were then in uninterruptible sleep
  (`states-at-stall.txt`), 505 of them VMMs; the kernel stacks of 398 of 400 read were in
  btrfs's space reservation (`btrfs_delalloc_reserve_metadata`), with 17.6 GiB of data in the
  24 GiB image. A server wrote 29 MiB to its disk in half an hour on the root volume and 38 MiB
  in RAM. Servers on the faster store also used more CPU and memory at the same age.
- **What bounds N**, in the order the runs met them:
  1. The IPv6 neighbour table (`net.ipv6.neigh.default.gc_thresh3`, 1024 on Ubuntu): 512 routed
     servers overflowed it and did not all start.
  2. The store's size and speed, as above.
  3. Memory: 543 MiB a server at 30 minutes, when the coordinator resets it. This host's
     377 GiB hold about 610 with the store in RAM and about 660 with it on a disk.
  4. CPU: 0.26 of a CPU a server in its first minutes, 0.20 at 30. 192 CPUs give about 730 at
     the first figure.
  5. On a fleet host, before any of these: `fleet/published-ports.json` declares 4 IPv4 ports
     for Minecraft view sockets, and with `--tls-publish-base` the agent refuses a fifth
     server. This run gave the agent no base.
  Addresses do not bound it: a server takes one of the host's /80.
- **Not measured:** a local NVMe store, an x86 host, the fleet's GPU host with its other modes
  running, and more than 512 servers through this path.

### The page in 3D

`fleet/web/core/minecraft3d.ts` (the geometry, the cameras, the sky, the fork's flight, the frame
guard; pure and unit-tested) and `fleet/web/app/_wall/minecraft-gl.ts` and `minecraft-stage.ts`
(WebGL2, no library).

- **One context.** One WebGL2 canvas the size of the stage sits under the tiles. Each world is
  drawn into its tile's rectangle with viewport and scissor, in six draw calls: sky, the columns'
  tops, their sides, the world's boxes, the figures, water. The canvas is held to 2.5 M device
  pixels.
- **A world on the GPU** is three lists of column instances of 8 bytes (x, z, material, height,
  how far down its sides reach, flags): a top for every column and every canopy column, sides
  only for the ones that stand above a neighbour (the two that face the camera are drawn), and
  the water's surface. Trunks, plants, lone trees and the figures are boxes of 44 bytes. It is
  rebuilt and uploaded when the world, its fork's source or its glowing columns change, at most
  every 200 ms a tile (`REBUILD_MIN_MS`; a new launch or a new level of detail is built at
  once). The page takes a grid of at most 262,144 columns, 1,024 a side, which is the
  largest region a tile serves (512 x 512): a rebuild took 4 ms for the tile's 128 x 128, 16 ms
  for 256 x 256 and 66 ms for 512 x 512 (node, a shared development host).
- **What one block per column cannot say is guessed**: a cliff is solid down to its lowest
  neighbour; the ground under leaves is taken from the columns beside them that are not leaves,
  spreading inward, with a trunk under each canopy's top; a lake's bed deepens up to 4 blocks from
  its shore; a plant is a small box on the block under it. The optional `map.ground`
  (fleet/PROTOCOL.md, "Minecraft tile") replaces each guess.
- **The camera** keeps above the ground, leaves out the canopy between it and what it looks at,
  and circles from a side its subject can be seen from when a hill is in the way.
- **The sky** is the world's time of day when the tile sends `status.day_time`, else mid-morning.
- **Within a device's means.** Frame times on a laptop's and a phone's GPU are not measured. The
  page draws no frame while the GPU is still drawing the last (a fence sync), so a slow renderer
  is never queued behind, and it carries a guard (`FrameBudget`): each frame's time until the GPU
  had drawn it is sampled, and when the median of 20 is over 40 ms, or three in a row are over
  160 ms, the page
  gives up a level: first detail (one column for every 2 x 2 blocks), then frames (10 a second),
  then the flight (one still view a second, a column for every 4 x 4 blocks). A reduced view says
  "Reduced detail" on its tile, and a coarse column is outlined when any column it stands for
  differs from the fork's source, so the outline agrees with the count. After 20 s of frames
  that keep up with the display (done by its next tick, whatever its rate) the page tries one
  level up; a recovery that fails doubles the wait before the next, to 320 s, and one that holds
  starts it over. A frame the GPU never reports done is drawn past after 5 s and counted. A
  software renderer starts at the last level. The floor, measured on Chromium's software renderer
  (the kind CI has) on a shared development host: four tiles in a 1232 x 541 canvas took 1.8 s a
  frame at full detail, the guard reached its last level, and a frame there took 0.16 s.
- **What it holds.** A tile off the screen and a hidden tab hold no buffers on the GPU; they are
  uploaded again when next drawn. No WebGL2 is the flat map with "Flat map: no WebGL2 in this
  browser." on each tile; a lost context is the flat map with "3D view lost; showing the flat
  map.", and the 3D view returns when the browser restores the context.
- **What is drawn from data and what is filled in** is said once under the header: "Drawn from
  each column's top block; trunks are filled in, and so are the forest floor and lake beds of a
  server that does not send them." The sky does not move until a server sends its clock.
- **Reduced motion:** one still wide view per tile, each from its own side, drawn again (at most
  four times a second) only when its world or its players change; no flight for a fork. A bot
  another viewer drives does not move the tile; the viewer who drives it keeps the view from
  behind it.
- **A column that just changed** takes a tile's next cut, chosen when the cut begins and kept
  for it, and never two cuts in a row: bots that build without pause do not hold the camera.
- **Opening a tile** (a click, or Enter on it) gives one server the whole stage; Escape or "All
  servers" brings the wall back, and focus goes to "All servers" and back to the tile. The other
  servers keep their sockets.
- **A fork's flight** is drawn only when it can be: in 3D, with motion, both tiles on the stage
  and the source's world in. Otherwise, and when the copy fails, the tile keeps its own timer or
  error.

### A field of servers

A snapshot that carries `minecraft.field` (PROTOCOL.md, "Many servers: the field and the status
rows") names more servers than the page can draw. The page then shows two things side by side
(the field above the tiles on a phone):

- **The field:** every server a cell on one canvas, with no element per cell, coloured by what
  its host's last status row says: running at 19.5 TPS or more, running slower, restoring,
  forking, failed. A server whose host has sent no row, or none for 5 s, is an outline: not
  reported, never zero and never running. Hovering, tapping or the arrow keys give one cell's
  line: its state, TPS, players, time since its restore and how long the restore took. A cell
  is lighter for the first minute after its server's restore. The cells shrink to 9 px to fit;
  under that the field scrolls.
- **Six 3D tiles**, each a real view socket to one server. They cut to other running servers
  every 12 s; a click or Enter on a cell brings that server into the first tile for a minute.
  Only the six servers in tiles, and any lent one, have a socket and a world on the GPU.
- **The headline** counts the servers and their states from the rows, and gives servers per busy
  CPU and per GiB in use from the hosts' own readings (`busy`, `mem_used_mib`). With a host
  silent, or a reading absent, that part is not shown.
- **Reset all** is counted as it comes back: "Reset all: 212 of 512 back · 31.8 a second ·
  6.7 s", then the whole time once every server the reset was sent to is back. A reset is over
  then, or 180 s after its click (`WAVE_END_S`), whichever comes first: a reset that ends short
  reads "509 of 512 back after 36 s". Before its end, with nothing being restored and none back
  for 10 s the count reads as stopped, and runs again if more come back. The line goes a minute
  after the reset is over. The count and the time are the coordinator's when the snapshot's
  `last_reset_all` carries them (`back`, `took_ms`), and otherwise read from the rows, which do
  not say which servers a reset was sent to. A cell flashes when its server returns.

The page asks for the rows by sending `"minecraft": true` with its visibility; the other pages do
not get them. The field's canvas is painted only when a cell, the cursor or the tiles change, or
while a flash runs.

### Invariants (each becomes a test)

1. Only the viewer the scene names as driver can move a bot; every other socket's drive is
   refused, and none is accepted before a scene.
2. A drive message carries keys from a fixed set, a direction and at most one of two actions;
   anything else is refused, and the keys stop 1 s after the last message.
3. The view socket needs a direct token valid under the scene's keys, as a renderer's frames do.
4. A fork never stops or restarts its source: the source's server keeps its players and its
   world, and its game time keeps increasing across the fork.
5. A fork's copy holds the source's world at the snapshot: a tower built before the fork is on
   both.
6. A reset or a returned borrow restores the gold snapshot, never a fork.
7. Pumpkin's configuration turns off telemetry, Bedrock, LAN discovery and plugins.
8. No Mojang asset is in the image, the repository or the page.

### Failure states

| failure | what happens |
|---|---|
| the gold VM is not ready within 300 s | the gold flow fails with its last `/health`; every server reports failed with that reason, and the flow runs again after 60 s |
| a restore's `/health` is not ready within 60 s | the server is failed and relaunched with backoff (1 s, doubling, capped at 30 s) |
| Pumpkin exits | mctile exits, the container and the VM end, the agent reports failed and relaunches |
| a fork's snapshot fails | the target is left as it was and reports the error; the source keeps running |
| a fork's restore fails | the target is failed and relaunched from the gold snapshot |
| the agent disconnects | every server's VM is stopped, as slots are |
| a world is griefed or wedged, and nobody holds it | the coordinator resets the server to its gold snapshot 30 min after its launch (`fleet_core::minecraft::AUTO_RESET_MS`), with no viewer; a borrowed server is reset when its borrow ends |

## Licensing

- **Pumpkin** is GPL-3.0. The image carries its licence and a notice naming the repository and
  commit it was built from, and the build fetches that exact commit, so the source of every
  binary is public. The one change from upstream, the login timeout, is public in ejc3/Pumpkin.
- **azalea** is MIT. Both fixes are public in `ejc3/azalea`; both bugs are reported upstream in
  azalea-rs/azalea#404.
- **No Mojang asset.** The page draws flat colours of our own; the image holds no client or
  server jar, texture, sound or language file.
- **Offline mode** means the servers check no Minecraft account, so anyone with a Java Edition
  client can join under any name. The owner chose open joins in offline mode (2026-11-08), with
  the servers reset from their snapshot: on the schedule above, on Reset, and when a borrow
  ends. Virginia's dedicated Java security group admits IPv6 TCP/25565 through approved GPU hosts
  to the Minecraft guests. Viewer sockets use separate rules. Pumpkin's query, management, proxy,
  LAN and Bedrock services are disabled. The page and the coordinator stay behind Cloudflare Access.
- **The density comparison** runs Mojang's vanilla server and Paper beside Pumpkin, on the
  bench box only, with Mojang's EULA accepted there by the owner's decision (2026-11-08,
  "Compare is fine for research"). The jars are downloaded on that box and are never in git, a
  published image, a staging tile or the page.

## Public joins (TCP 25565)

Virginia's `fcvm-fleet-minecraft-java` security group admits TCP/25565 over IPv6. Clients
running Java Edition 26.3 can use the tile's current join hostname. The group is separate from
the shared viewer group and attaches only to the Virginia x86 GPU host template for the AMI
approved in [AWS Terraform](https://github.com/ejc3/aws/blob/main/fcvm-fleet.tf)
(`minecraft_java_access.host_ami`). Before each agent start, the host verifies the actual boot
AMI, delegated IPv6 prefix and guest layout. The host forwards Java traffic only to the Minecraft
slots, leaving its own address, other guests and the gold VM closed on that port. An unapproved
boot AMI or missing metadata leaves Java closed.

**What answers.** Pumpkin, at the pinned commit of the ejc3/Pumpkin fork (upstream plus the login
timeout below), in offline mode, speaking Java Edition 26.3 and nothing else, on the Minecraft VMs
only. Nothing else in a Minecraft VM or on a host listens on 25565, and Pumpkin's query,
management, proxy, LAN and Bedrock services are off by name
(`the_config_is_offline_creative_and_reaches_nowhere`). Every other port of a VM is closed to the
internet by the security group, except the tile's view socket (443 over IPv6, and its port of the
host's IPv4 range over IPv4).

**Limits**, each enforced by code and covered by a test that fails without it:

| Limit | Value | Where | Test |
|---|---|---|---|
| Open connections per source /64 | 4 | the host's `FCVM-MC-JOIN` chain (`ip6tables` connlimit), reached from `FORWARD` for TCP 25565 | `one_source_holds_at_most_the_open_connection_cap` |
| New connections per source /64 | a burst of 6, then 12 a minute | the same chain (hashlimit) | `one_source_opens_at_most_its_burst_of_new_connections_at_once` |
| Players per server | 20, the tile's three bots among them | Pumpkin's `max_players` | `the_tile_caps_its_players_and_closes_a_login_that_does_not_finish` |
| Login (handshake, login and configuration) | 15 s, then the connection is closed | Pumpkin's `login_timeout` (the fork's; upstream has none) | the same, and the fork's `pumpkin-tester` check `login timeout` |
| Packets per connection | Pumpkin's packet limiter, on | Pumpkin | `the_config_is_offline_creative_and_reaches_nowhere` |

A source is counted by its /64 because an IPv6 subscriber usually holds a whole /64. The agent
installs the chain at every start, before any server, and does not start when it cannot
(`src/fleet/agent/joinguard.rs`). Over the limits a connection is reset, not dropped, so a client
sees the refusal at once.

**Blast radius.** A player who gets past Pumpkin's protocol handling is inside one Minecraft VM:
- The VM is its own fcvm microVM in routed mode. A Windows sandbox's network namespace has no
  route out (`an_isolated_namespace_has_no_way_out`), and the cross-VM check found no Minecraft
  VM port reachable from a sandbox. Minecraft VMs are reachable from the host and from other
  routed VMs on 22, 111 and 8080 (the guest's own services and the tile's plain port), which the
  security group does not admit from outside.
- Every server is restored from the gold snapshot on Reset, when a borrow ends, and on the
  schedule (`AUTO_RESET_MS`, 30 minutes), so nothing a player builds or breaks survives long.
- The guest holds no credential: no instance role is reachable from it (routed VMs have no route to
  the metadata service), and the only file the host maps in is the fleet's TLS certificate for the
  tile's view socket, read-only.

**Closing public joins.** In `ejc3/aws`, restore `minecraft_java_access.source_cidr` in
Virginia's `fcvm_fleet_east1` configuration from `::/0` to the administration jumpbox's
IPv6 `/128`. Use the [canary configuration](https://github.com/ejc3/aws/pull/866) to derive
the address from Terraform. Review and apply that ingress change through the AWS repository.
New public joins stop without a host restart or a viewer change; [tracked connections can
remain open](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/security-group-connection-tracking.html).

**Checking it.** Read back the Virginia group's IPv6 rule and the host template's approved AMI.
From an allowed source, check Java status on a live tile and confirm the fifth open connection
and seventh new connection in a burst are refused. Verify Java is closed to a serving
non-Minecraft guest, the host and the gold VM. After narrowing the rule, use a different IPv6
source to check that the tile's viewer socket still responds and TCP/25565 is closed.

## Density

The headline is servers per vCPU: how many server VMs share one vCPU while every server keeps its
tick rate at 19.5 or more with three bots playing on it. Measured on a bare-metal arm64 host
(c8gd.metal-24xl, 96 vCPUs, 188 GiB; never the shared dev host), 2026-11-08, one boot, with
`fleet/minecraft/bench/mcbench.py`; the records are in
`fleet/minecraft/bench/results/2026-11-08-c8gd-metal-24xl/`, and the table below is its
`summary.json` (`summarize.py` over `ladder.jsonl`, `restore.jsonl` and `fork.jsonl`).
`sweep.jsonl` is the first sweep of the same boot, which found each server's edge; its readings
had no freshness check (below), so no number here comes from it.

- **The shape.** Every server VM has 1 vCPU and 3 GiB (Paper ran out of memory in 2 GiB), bridged
  networking, and is restored from its arm's gold snapshot: the server booted, three bots
  joined, the 128 x 128 map around the spawn loaded, and ten readings in a row at 19.5 or more.
  All the server VMs run in one cgroup whose cpuset holds 4 vCPUs. The bots run on the host's
  other cores, one `mctile bots` process per server, so the 4 vCPUs hold only the servers.
- **A run** restores K servers, joins three bots to each, waits 30 s, then reads every server's
  tick rate about every 5 s for 60 s. A reading counts only while all three bots are online and
  the server's last game time is under 3 s old; otherwise it is a 0. A count holds when every
  reading of every server in every run of it is 19.5 or more, and below the lowest count that
  did not hold.
- **The servers.** Pumpkin 3ea7d281 (Java Edition 26.3), Mojang's vanilla server 26.3 and Paper
  26.3 build 244, each with the same seed, view and simulation distance 6, offline and creative;
  the Java servers with a 1.5 GiB heap on Java 25. Pumpkin does not implement all of vanilla's
  mechanics, so this compares the servers on this workload, not on the whole game.

| | servers on 4 vCPUs that held in every run | per vCPU | the next count | MiB per added server | restore to first chunk, p50 | fork, p50 |
|---|---|---|---|---|---|---|
| Pumpkin | 24 (3 of 3 runs) | 6.0 | 25: 1 of 3 runs | 348 | 870 ms | 1116 ms |
| vanilla | 6 (3 of 3) | 1.5 | 7: 2 of 3 | 1349 | 775 ms | 2217 ms |
| Paper | 10 (3 of 3) | 2.5 | 11: 1 of 3 | 1301 | 1175 ms | 1991 ms |

Pumpkin fits four times as many servers on a vCPU as the vanilla server and 2.4 times as many as
Paper, in about a quarter of the memory. Near each edge the result varies from run to run: Pumpkin held
26 servers in 3 of 3 runs but 25 in 1 of 3. Memory is the least-squares slope of the VMs'
cgroup anonymous memory over the counts that held below the first that failed. Restore to first chunk is from `fcvm snapshot
run` to a new bot process's first chunk, bot start-up included (a new bot joining the same warm
server took 136, 347 and 444 ms). A fork is `fcvm snapshot create` of a running server with its
bots (373, 1331 and 865 ms) and the copy's restore to its first chunk; the source's first bot
was never disconnected (its login count did not change).

## Stack of PRs

1. This design, `fleet/minecraft` (mctile, the image, its tests) and the density bench.
2. Coordinator: `state.minecraft`, the intents and their test vectors, fleet/PROTOCOL.md.
3. Agent: `--minecraft N`, the gold flow, restores, forks, scenes, reports.
4. Web: `?mode=minecraft`, the tile renderer, the take-over, the mock and the e2e tests.
5. The host image with the tile image (`fleet/ami/host/build.sh`), then ejc3/aws: the image pin
   and the agent's `--minecraft`.
6. Staging behind Cloudflare Access, checked from a browser.
