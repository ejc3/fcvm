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
Rust, GPL-3.0, pushed to daily. It is run unmodified, built from a pinned upstream commit
(`PUMPKIN_COMMIT` in the Makefile, `3ea7d281` of 2026-11-08, which speaks Java Edition 26.3,
protocol 777, and only that). Its README now lists world saving, lighting and world borders as
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
  boolean and log "Boolean value was not 0 or 1" for every bubble particle. The swing bug is
  reported upstream as [azalea-rs/azalea#404](https://github.com/azalea-rs/azalea/issues/404),
  which is the reason for the fork, and the particles bug is to be added there; once upstream
  fixes both, the pin moves back to azalea-rs/azalea. Three bots join over
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
  public IPv6 address. At start it boots one gold VM (`mc-gold`), waits until its `/health` says
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
  binary is public. Nothing of it is changed.
- **azalea** is MIT. Both fixes are public in `ejc3/azalea`; the swing fix is reported upstream as
  azalea-rs/azalea#404.
- **No Mojang asset.** The page draws flat colours of our own; the image holds no client or
  server jar, texture, sound or language file.
- **Offline mode** means the servers check no Minecraft account, so anyone with a Java Edition
  client can join under any name. The owner chose open joins in offline mode (2026-11-08), with
  the servers reset from their snapshot: on the schedule above, on Reset, and when a borrow
  ends. A public player reaches only the Java listener: the fleet security group admits TCP
  25565 over IPv6 and nothing else of the server's beyond the view socket on 443, and Pumpkin's
  query, management, proxy, LAN and Bedrock services are off by name. At most 20 players join a
  server, and Pumpkin's packet limiter is on. The page and the coordinator stay behind
  Cloudflare Access.
- **The density comparison** runs Mojang's vanilla server and Paper beside Pumpkin, on the
  bench box only, with Mojang's EULA accepted there by the owner's decision (2026-11-08,
  "Compare is fine for research"). The jars are downloaded on that box and are never in git, a
  published image, a staging tile or the page.

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
