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
- **The tiles.** Every server is a tile showing its world from above, drawn by the page: one
  flat colour per block material, shaded by height, with each player as a marker and its name.
  Three bots walk about each world and build small towers of their own colour. A tile is
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
  the guest's loopback. A bot that nobody drives walks to a random column of the map's region
  with azalea's pathfinder and builds a tower of 3 to 5 blocks of its wool there. A restore keeps
  the bots connected: their connections to Pumpkin are loopback inside the guest, which fcvm's
  restore keeps (it retires only sockets whose peer is outside the guest). When the agent posts
  the copy's scene, the program reseeds its choices from the guest's random pool, the tile's
  name and its generation, so a fork and its source do not walk the same paths.
- **The map.** The bots share one view of the world. Every 200 ms the program reads the top
  block of every column of a 128 x 128 region around the spawn (the chunk heightmaps the server
  sends, then the block below the first empty one) and turns its registry name into one of 48
  materials of our own (`palette.rs`: water, grass, sand, wood, leaves, the 16 dye colours and
  so on). The page holds the colours. No texture, colour table, sound or other asset of the game
  is used anywhere.
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
  tile renderer (a canvas per tile from its view socket), the take-over panel with its key
  handling and timer, the mock coordinator's servers and a mock tile, and Playwright tests on
  desktop and phone layouts.

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

How many servers one host runs. A table that stood here (2026-11-08: servers per vCPU for
Pumpkin, Mojang's vanilla server and Paper, with the VMs held to 4 vCPUs and the bots outside
them) was measured on kernel 7.0.0-1013-aws, which has a known defect. It and its records are
removed, and the section below supersedes it. The harness for that shape
(`fleet/minecraft/bench/mcbench.py`, `summarize.py`) remains; the three-server comparison has not
been run on a kernel with the fix.

### Servers on one host

How many servers one host runs in the fleet's own shape, from one boot of one bare-metal arm64
host (c8g.metal-48xl: 192 vCPUs, 377 GiB, no local disk) on kernel 7.0.0-1018-aws, 2026-11-24.
Each server is a VM of 1 vCPU and 1024 MiB with its three bots inside it, as
`fcvm fleet agent --minecraft N` starts them. fcvm and the tile image were built from this
branch at `86b7414e8` (main at `c083d7ec6` with this harness), and the steps run with
`fleet/minecraft/bench/hostramp.py`; the records are in
`fleet/minecraft/bench/results/2026-11-24-c8g-metal-48xl/` (`host.txt` names the kernel, the
store and the tree). Two things differ from the fleet: networking was bridged (the fleet's is
routed), and the store was a btrfs image in RAM (the host's only disk is a 30 GiB network
volume), so the store's speed bounds nothing here and its contents come out of the host's
memory.

- **A step** restores K servers from one gold snapshot (the tile ready, 20 TPS for 10 s, then
  `snapshot create --stop`) with 16 restores in flight, gives each its own scene (so its bots are
  reseeded), waits 30 s, then reads every tile's `/health` every 5 s for 60 s. A reading counts
  only with three bots online and a game time under 3 s old, else it is 0. A step holds when
  every reading is 19.5 or more and every server took its scene.

The steps in the order they ran, each torn down before the next. The first five ran back to
back; the last ran half an hour later, after the 48-core steps and the first of the two steps
without bots below (and after the same host had run other VMs for another record, from 21:35
to 21:47 UTC):

| servers | lowest reading, TPS | host cores busy (per server) | K restores | one more restore, to ready | anonymous MiB per server after the step |
|---|---|---|---|---|---|
| 8 | 19.89 | 1.3 (0.159) | 0.8 s | 345 to 365 ms | 265 |
| 64 | 19.86 | 11.6 (0.182) | 3.6 s | 433 to 436 ms | 264 |
| 256 | 19.88 | 48.9 (0.191) | 16.8 s | 621 to 673 ms | 271 |
| 512 | 19.86 | 103.0 (0.201) | 90.4 s | 969 to 1037 ms | 350 |
| 768 | 0.00 | 153.6 (0.200) | 179.4 s | 1485 to 1640 ms | 362 |
| 512, again at the end | 19.76 | 98.6 (0.193) | 55.0 s | 880 to 967 ms | 310 |

- **512 held its tick rate both times, and by the step's rule held once.** In the first run
  every one of the 6,144 readings was 19.86 or more, but 66 of the 512 scene posts timed out
  (the harness gives a post 3 s and does not send it again), so those servers' bots kept the
  gold snapshot's seed and the step does not count as held. Its restores were the slowest of
  the two runs too (90.4 s, the slowest single one 13.4 s, against 55.0 s and 3.1 s). It began
  the moment the 256 step's teardown returned; the second run began on a host that had run no
  VM for a minute, and held, every scene set. Why the first was slower was not measured.
- **768 did not hold:** 5 servers had a reading under 19.5 (the 1st percentile of the 9,216
  readings was 19.89), with 154 of the 192 cores busy and 68 GiB of memory available
  afterwards. No count between 512 and 768 was run.
- **Cores.** With the VMs' cgroup held to 48 cores (`MC_CORES=48`), 192 and 240 servers held
  (lowest readings 19.87 and 19.59, 43.8 cores busy at 240) and 288 did not (232 of the 288 had a
  reading under 19.5): between 5.0 and 6.0 servers a core.
- **Without bots** (`MCTILE_BOTS=0`, an empty server; a bot measures the tick rate, so there is
  none to read): 256 servers used 1.2 cores, 0.005 each, and 84 MiB of anonymous memory each.
  The record holds this step twice. The first ran straight after the 48-core steps and was
  itself held to those 48 cores (`vm_cores_allowed` 48; 1.3 cores busy): the harness's reset of
  the cgroup wrote no bytes, which the kernel never sees. With that fixed the step ran again at
  the end of the boot on all 192 (`host.txt` has the cgroup's CPUs after each).
  Bots connected but standing still were not measured.
- **Start rate.** With 16 restores in flight: 17.8 servers a second at 64, 15.2 at 256, 5.7 and
  9.3 at 512 (the two runs), 4.3 at 768. The "one more restore" column is `fcvm snapshot run` to
  a ready `/health`, polled every 20 ms; at the high counts it includes the harness's own
  `fcvm ls --json` over every VM.
- **Memory** is each step's reading about two minutes after its servers start, and a server
  grows after that. Memory against a server's age was not measured with this harness.

## Stack of PRs

1. This design, `fleet/minecraft` (mctile, the image, its tests) and the density bench.
2. Coordinator: `state.minecraft`, the intents and their test vectors, fleet/PROTOCOL.md.
3. Agent: `--minecraft N`, the gold flow, restores, forks, scenes, reports.
4. Web: `?mode=minecraft`, the tile renderer, the take-over, the mock and the e2e tests.
5. The host image with the tile image (`fleet/ami/host/build.sh`), then ejc3/aws: the image pin
   and the agent's `--minecraft`.
6. Staging behind Cloudflare Access, checked from a browser.
