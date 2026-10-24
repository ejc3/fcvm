# fcvm fleet protocol

The wire formats between the browser, the coordinator (`fcvm fleet coord`), the agents
(`fcvm fleet agent`) and the renderers. The design is `docs/fleet/ARCHITECTURE.md`. The Rust
types are in `fleet/core/src/protocol.rs` (wire messages), `fleet/core/src/event.rs`
(intents) and `fleet/core/src/view.rs` (the snapshot state); where this page and those
types disagree, the types are right and this page is the bug.

Conventions:

- Every message is one JSON object with a `type` field.
- Times named `*_at` and `*_unix_ms` are Unix milliseconds. Durations end in `_ms` or
  `_secs`.
- Scene time `t` is seconds since the scene's `epoch_unix_ms`.
- A rectangle `{"x","y","w","h"}` is a fraction of the whole image: `x` and `w` of its
  width, `y` and `h` of its height, origin top left. The fcvm half is `x` in [0, 0.5), the
  EC2 half `x` in [0.5, 1). `image_aspect` is the whole image's width / height (default
  16/9, the renderer's: each half is 8:9).
- Camera `pos` is the camera position and `look` the world point it looks at, as
  `[x, altitude, z]` in kilometres (the renderer's world units); `fov_deg` is the vertical
  field of view. `time_of_day` is a fraction of the day in [0, 1) (0.25 sunrise, 0.5 noon,
  0.75 sunset). Light `color` is linear RGB in [0, 1].

## Coordinator

`fcvm fleet coord` serves the browser over one WebSocket and holds the only copy of the
fleet state. Every change is one event through `fleet_core::apply`, which has no clock, no
I/O and no randomness, so every viewer sees the same sequence.

### `GET /live` (WebSocket)

**Server to browser**

```json
{"type": "hello", "viewer": "v-17", "turnstile": "<site key>"}
```

The first message on every connection: the id this connection acts as (`actions[].viewer`
names it, so a page can find its own intents in the shared list), and the Turnstile site
key when intents need a `verify` first (`null` when they do not).

```json
{"type": "snapshot", "version": 42, "state": { ... }}
```

Sent on connect and whenever the version moves. The `state` is the full state, never a
diff. Versions only increase while the coordinator runs; a browser keeps the highest
version it accepted and drops any snapshot older than that. A browser that reconnects
starts again from the first snapshot it receives (a restarted coordinator without a state
file starts counting again). Under load a socket may skip versions; it always receives
the latest.

```json
{"type": "receipt", "id": "<client id>", "outcome": "committed", "version": 43}
{"type": "receipt", "id": "<client id>", "outcome": "refused", "reason": "no GPU behind fcvm:9", "version": 44}
{"type": "receipt", "id": "<client id>", "outcome": "refused", "reason": "the text check refused it: ...", "version": 45,
 "check": {"verdict": "refuse", "reason": "...", "sky": ["hello", "world"]}}
```

One per intent. `version` is the state version that first holds the intent's effect (or
its refusal in `actions`). An intent id the coordinator has seen before, from any viewer,
returns that id's first receipt again and changes nothing. `check` is present on `word`
intents that reached the text check: `verdict` is `allow`, `refuse` or `error`, and `sky`
the words the new one was judged together with. A refusal with `version: 0` was not
recorded (`"verify first"`: the connection has not passed Turnstile yet); the same id may
be sent again.

```json
{"type": "verified", "ok": true, "reason": null}
```

The answer to a `verify`.

```json
{"type": "claim", "claim": {"id": "claim-3", "tile": "fcvm:5", "phase": "running",
 "frame_url": "ws://<host>:<port>/frames", "token": "<secret>",
 "clicked_at": 1761134400000, "first_frame_at": 1761134405400, "expires_at": 1761134465400,
 "gpu": "Tesla T4", "phases": {"teardown_ms": 310, "restore_ms": 4300, "gpu_ms": 3100, "total_ms": 5400},
 "camera": [{"t": 812.0, "pos": [...], "look": [...], "fov_deg": 60, "intent": "<id>"}], "ended": null}}
```

Sent only to the connection whose `claim_gpu` committed, whenever the claim changes:
first with `phase: "restoring"` (no frame URL yet), then `running` once the private VM
has drawn its first frame (its `frame_url`, `first_frame_at - clicked_at` the click to
first frame, `phases` the agent's timings of the restore), again when `claim_camera`
moves its camera, and a last time with `phase: "ended"` and `ended` saying why
(`expired`, or `released`: `release_gpu`, the viewer leaving, or the GPU leaving the
fleet). The frame URL and the token appear in no snapshot. `tile` is the wall tile it
borrowed, null for a standby GPU.

```json
{"type": "clock", "server_unix_ms": 1761134400123, "client_unix_ms": 1761134400100.5}
```

The answer to a `ping`, echoing the client's time so the page can take the round trip.

```json
{"type": "error", "reason": "message is not JSON: ..."}
```

A message the coordinator could not read; nothing was applied.

**Browser to server**

```json
{"type": "verify", "token": "<Turnstile token>"}
{"type": "visibility", "visible": false}
{"type": "intent", "id": "<client uuid>", "name": "nudge", "args": {"target": "coast"}}
{"type": "ping", "client_unix_ms": 1761134400100.5}
```

`visibility` says whether the page is visible. A connection counts as an active viewer
until it reports `visible: false` (the page sends that when its tab has been hidden long
enough to pause its streams) and again once it reports `true`.

With Turnstile configured (`hello.turnstile` is the site key), every intent is refused
(`"verify first"`, not recorded) until the connection has sent a token that Cloudflare's
siteverify accepts; the answer is `verified`. Without it (local runs) every connection
counts as verified.

Intents:

| name | args | effect |
|------|------|--------|
| `restart_now` | `{}` | starts a race now (below); refused while the global cooldown runs (`race.cooldown_until`) |
| `relaunch` | `{"tile": "fcvm:3"}` (a tile id) or `{"tile": "slot-3"}` / `{"tile": "ec2-2"}` (a slot id) | bumps that slot's generation; refused for a dark tile, a draining host, a borrowed GPU, or a second relaunch from the client within 2 s |
| `nudge` | `{"target": "<viewpoint>"}` | a vote for the next camera hold, among `nudges.targets`; one vote per client, the latest counts; refused once the vote has closed |
| `word` | `{"text": "hello", "text_style": "skywriting"}` | 1 to 28 characters (the renderer's limit) after collapsing whitespace, no control characters, one per client per 5 s; checked together with every word in the sky first (below). `text_style` is how the renderer draws it (#1178): `lockup` (when absent or null), `skywriting` or `landmark`; anything else is refused before the text check |
| `light` | `{"pos": [x, y, z], "color": [r, g, b]}` | one per client per second; each client keeps its latest 3 (its oldest goes); `color` components are clamped to [0, 1], then scaled so they add up to at most 2; the scene keeps the latest 32 |
| `claim_gpu` | `{}` | a GPU of your own for 60 s (below) |
| `release_gpu` | `{"claim": "claim-3"}` (or `{}`) | ends your claim early; a `claim` that is not yours is refused |
| `claim_camera` | `{"drag": {"yaw_deg", "pitch_deg", "dive"}}` | turns and moves your claimed GPU's camera (only yours; refused without a claim), easing over 0.3 s; the keyframe it eases to gets `"intent": <id>`. Yaw within 360, pitch within 180, dive within 50 |

There is no intent to move the wall's camera, set its time of day, relaunch every tile
on a viewer's schedule, or change the race cadence: the director and the owner's
configuration own those. Two names differ from what the page (#1143) assumed:

- The race button's intent is `restart_now`, the owner's name for it in the second
  design review, not `relaunch_all`. The race it starts is still reported as the
  snapshot's `relaunch_all`.
- There is no `time_of_day` intent. Each hold's viewpoint sets the time of day (the
  low-sun holds are viewpoints), so a viewer's setting would undo the director's next
  hold or be undone by it within seconds.

**Clients and limits.** Every limit counts per client address, not per connection: the
`CF-Connecting-IP` header when the coordinator runs with `--trust-cf-connecting-ip`
(only behind Cloudflare, which sets it), else the peer address. Reconnecting does not
reset a limit.

**Races.** A race relaunches every tile at once and times both halves by the
coordinator's clock. The owner sets the cadence (`--race-every <secs>`); `race.next_race_at`
says when the next one starts. Any race starts a global cooldown (`--restart-cooldown`,
default 60 s) during which `restart_now` is refused for everyone; `restart_now` then
starts one at once and the schedule restarts from it. Tiles on a draining host and GPUs
lent to a visitor sit the race out.

**The directed camera.** The coordinator flies the wall's camera along a path generated
from `--camera-seed`: holds of 7 s at named viewpoints (low sun, a high leg showing the
horizon's curve, the main peak and the clock in the sky framed across the fcvm/EC2 seam at
image centre), joined by flights. The order loops and is the same for the same seed. Two
seconds before a hold ends the vote closes: the offered viewpoint with the most `nudge`
votes is next (ties go to the earlier offered; no votes, the default next). The winning
nudge's id is stamped on that hold's keyframes (`camera[].intent`).

**Viewer text** (`word`). Before the event is applied the coordinator asks Claude Haiku on
Amazon Bedrock whether the text may be shown publicly, read together with every word now
in the sky (so words cannot combine into something unacceptable). The model id and region
come from the coordinator's configuration (`--text-check-model`, `--text-check-region`).
Only an explicit allow commits the word; a refusal, an error, a timeout (5 s) or a
coordinator started without a text check (`--text-check none`) refuses it. A word checked
against a sky that changed meanwhile is refused (send it again). A check that cannot run
marks the coordinator degraded (`failures`) until one runs again. The scene keeps the
latest 16 words. A refused word's text appears nowhere: not in the scene, not in
`actions`.

**Your own GPU** (`claim_gpu`). The claim takes a standby GPU when there is one. Normally
every GPU draws the wall, so the claim borrows one: the last tile in slot order that is
running its current generation (not relaunching, not still racing). The tile keeps its
place in the layout, its `phase` becomes `borrowed` and its frame URL is withheld; its
host's aggregator draws it black. The agent tears the tile's VM down and restores a
private VM on that GPU drawing the whole image with the session's own camera, which only
its viewer can move (`claim_camera`). The coordinator times the click to that VM's first frame and
sends the viewer, alone, `claim` messages. After 60 s (`claim_secs`), on `release_gpu`, or
when the viewer disconnects, the private VM is destroyed and the tile's VM restores back
into the wall, timed like any relaunch (`last_relaunch.by` is `return:<session>`). Refused:
a second claim from a client that holds one, a claim within 120 s of the client's last one
ending, a claim when `--max-borrowed` tiles are already lent (default 2 or a tenth of the
tiles rounded up, whichever is smaller), and a claim when no tile can be lent.

**Snapshot state**

```json
{
  "layout": {"generation": 14, "from_frame": 25845, "from_t": 861.5,
             "fcvm": {"x": 0, "y": 0, "w": 0.8928571428571429, "h": 1}, "ec2": {"x": 0.8928571428571429, "y": 0, "w": 0.10714285714285714, "h": 1}},
  "budget": 707107, "budget_from_t": 863.2,
  "tiles": [
    {
      "id": "fcvm:1", "half": "fcvm", "rect": {"x": 0, "y": 0, "w": 0.0893, "h": 0.1},
      "previous_rect": {"x": 0, "y": 0, "w": 0.0909, "h": 0.1},
      "phase": "running",
      "slot": "slot-1",
      "frame_url": "ws://<host>:<port>/frames",
      "gpu": {"sbdf": "0000:01:00.0", "name": "Tesla T4"},
      "host": "<host name>",
      "vm": "fleet-slot-1",
      "generation": 3,
      "fps": 30.0,
      "gpu_stats": { ... },
      "error": null,
      "failure": null,
      "last_relaunch": {"clicked_at": 1761134400000, "by": "viewer:v-4", "first_frame_at": 1761134405200,
                        "phases": {"teardown_ms": 310, "restore_ms": 4300, "gpu_ms": 3100, "container_ms": 600, "first_frame_ms": 250, "total_ms": 5200}},
      "borrowed": null
    },
    {"id": "fcvm:2", "phase": "borrowed", "frame_url": null,
     "borrowed": {"session": "claim-3", "viewer": "v-9", "since": 1761134400000, "until": 1761134460000}, ...},
    {"id": "fcvm:3", "phase": "failed", "error": "...",
     "failure": {"kind": "relaunch_failed", "message": "fcvm exited ...", "retry_at": 1761134402000}, ...},
    {"id": "ec2:1", "half": "ec2", "rect": {"x": 0.5, "y": 0, "w": 0.25, "h": 0.5}, "phase": "waiting_capacity", "slot": null, ...}
  ],
  "aggregates": [
    {"id": "agg:<host>", "half": "fcvm", "host": "<host>", "rect": {"x": 0, "y": 0, "w": 0.5, "h": 1},
     "frame_url": "ws://<host>:<port>/frames", "tiles": ["fcvm:1", "fcvm:2", ...]},
    {"id": "agg:ec2", "half": "ec2", "host": null, "rect": {"x": 0.5, "y": 0, "w": 0.5, "h": 1},
     "frame_url": "wss://...", "tiles": ["ec2:1", ...]}
  ],
  "fleet": {"hosts": [{"name", "arch", "gpus": [{"sbdf", "name"}], "draining"}], "gpus": 8, "fcvm_gpus": 8,
            "ec2_instances": 0, "n": 8, "max_tiles": 100, "standby": ["slot-9"], "standby_ready": 1,
            "power": {"state": "waking", "idle_since": null, "asleep_since": null, "woke_at": 1761134400000,
                      "hosts": [{"host": "i-0123456789abcdef0",
                                 "phases": [{"phase": "instance_pending", "at": 1761134401000},
                                            {"phase": "booted", "at": 1761134446000},
                                            {"phase": "store_ready", "at": 1761134470000},
                                            {"phase": "agent_connected", "at": 1761134492000}]}],
                      "last_wake": {"started_at": ..., "awake_at": ..., "gpus": 16, "ms": 98000},
                      "wakes": [...]}},
  "scene": {"epoch_unix_ms": ..., "camera": [...], "time_of_day": 0.3, "words": [{"text", "t", "text_style"}], "lights": [{"pos", "color", "t"}]},
  "image_aspect": 1.7777777777777777,
  "next_race_at": 1761134700000,
  "cooldown_until": 1761134460000,
  "relaunch_all": {"race": 12, "started_at": 1761134400000, "by": "schedule",
                   "fcvm_done_at": 1761134407530, "ec2_done_at": null, "fcvm_pending": 0, "ec2_pending": 3,
                   "fcvm_gpus": 7, "ec2_gpus": 8, "fcvm_ms": 7530, "ec2_ms": null,
                   "ec2_capacity_wait_ms": 0, "ec2_waiting_since": 1761134402000},
  "scoreboard": {
    "fcvm": {"median_ms": 7410, "last_ms": 7530, "races": 11, "measured_on": 7, "last_capacity_wait_ms": 0, "last_race": 12},
    "ec2": null
  },
  "races": [{"id": 12, "started_at": 1761134400000, "by": "schedule",
             "fcvm": {"tiles": {"slot-1": {"tile": "fcvm:1", "generation": 4, "first_frame_at": 1761134407400,
                                            "phases": { ... }, "capacity_wait_ms": 0, "waiting_since": null}},
                      "dropped": 0, "done_at": 1761134407530, "time_ms": 7530, "last": "slot-6",
                      "capacity_wait_ms": 0, "sitting_out": ["fcvm:2"]},
             "ec2": { ... }}],
  "nudges": {"targets": [{"id": "coast", "label": "From the coast", "votes": 2},
                         {"id": "horizon", "label": "High above: the curve of the horizon", "votes": 0},
                         {"id": "dawn-valley", "label": "Dawn in the valley", "votes": 1}],
             "holding": "peak-seam", "next_hold_at": 1761134407000, "next": null, "arrive_next_at": null},
  "borrow": {"cap": 1, "lent": 1, "next_return_at": 1761134460000},
  "failures": [
    {"kind": "host_reclaimed", "host": "<host>", "since": 1761134400000, "retiling": true},
    {"kind": "coordinator_degraded", "component": "text_check", "reason": "...", "since": 1761134400000},
    {"kind": "gpu_quarantined", "host": "<host>", "sbdf": "0000:05:00.0", "error": "...", "since": 1761134400000}
  ],
  "counters": {"restores": 1834, "restores_last_hour": 96, "scrubs": 212, "scrub_failures": 1},
  "claims": [{"session": "claim-3", "viewer": "v-9", "slot": "slot-2", "gpu": {...}, "host": "<host>",
              "claimed_at": 1761134400000, "expires_at": 1761134460000, "first_frame_at": 1761134405400,
              "click_to_first_frame_ms": 5400, "borrowed": "fcvm:2"}],
  "viewers": {"count": 3, "active": 2},
  "actions": [{"at", "viewer", "id", "name", "args", "summary", "outcome"}]
}
```

- `tiles` lists one tile per live GPU, the fcvm side then the EC2 side: `fleet.n` fcvm
  tiles (fcvm GPUs on hosts that are not draining, at most `max_tiles`) and `fleet.ec2_n`
  EC2 tiles (EC2 tiles with an instance, at most `max_tiles`). Each side is as wide as its
  share of the live GPUs (`layout.fcvm`, `layout.ec2`: fcvm 100 and EC2 12 give the fcvm
  side 100/112 of the width); a side with none has no rectangle (null) and the other takes
  the whole width; with none on either side there are no tiles. There are no dark tiles:
  an EC2 tile still waiting for capacity is not on the wall (`fleet.ec2_waiting` counts
  them, and the race's `relaunch_all` shows the wait). The fcvm side is cut into
  full-width bands, one per GPU host (hosts in the order of their lowest slot number), as
  tall as the host's share of its tiles, each band its own grid, row-major, so a host's
  tiles cover one rectangle, which its aggregator encodes whole; the EC2 side is one grid.
  Tile ids `fcvm:<k>` and `ec2:<k>` (1-based) name places; `slot` names the GPU behind the
  place. A GPU keeps its slot (and its addresses) when the fleet re-tiles: only its
  rectangle changes. Slot ids are `slot-<n>` (fcvm, reused lowest first) and `ec2-<n>`. A
  borrowed GPU keeps its tile's place.
- `layout` is the current layout generation. Every change of the tiling (a GPU joining or
  leaving either side) makes a new `generation` that takes effect at frame `from_frame`
  (frames fall on scene time k/30; `from_t` is that time), 45 frames (1.5 s) after the
  change, so it reaches every renderer first. Each renderer draws its old rectangle up to
  that frame and its new one from it. A tile the change moved carries `previous_rect`
  (null for a tile that did not move; the whole image for a GPU that warmed before joining). The page reads `layout.generation`,
  `layout.from_t` (or `from_frame`), `layout.fcvm`, `layout.ec2`, and each tile's `rect`
  and `previous_rect`: it keeps the old tiles on screen until every tile of the new
  generation has delivered its first frame at or after `from_t`, then swaps them all at
  once.
- `budget` is the grid's render budget in pixel-samples per frame per tile and
  `budget_from_t` the scene time it took effect at (both null until every placed tile has
  reported a fit); see "Grid budget" below. The page may show it; it needs nothing from
  it.
- `phase`: `waiting_capacity` (dark: no GPU behind the tile, or EC2 refused capacity),
  `borrowed` (lent to a visitor, drawn black), `pending` (assigned, nothing reported yet),
  `relaunching` (a newer generation is wanted than the agent has reported), or what the
  agent reported: `stopping`, `restoring`, `starting`, `running`, `failed`.
- `frame_url` is the renderer's frame WebSocket as the browser should open it (append
  `?h=<height>`); null while borrowed. After a relaunch the tile reconnects when `phase` is
  `running` again.
- `last_relaunch.first_frame_at - clicked_at` is the tile's relaunch time; `phases` are
  the agent's timings for that relaunch (see Agent).
- `next_race_at` is when the owner's cadence races next (null without one);
  `cooldown_until` is when `restart_now` is accepted again (null before any race).
- `relaunch_all` is the latest race (null before the first): who started it (`schedule`
  or `viewer:<id>`), each half's tiles timed (`*_gpus`), still to reach their first frame
  (`*_pending`), when the half finished and its time (`*_ms`, click to the last tile's
  first frame as the coordinator saw it, less EC2 capacity waits). A half with no GPUs
  (`*_gpus` 0) has nothing to restart: its `*_done_at` is `started_at` and its `*_ms` null,
  so the race is over once the other half is. `ec2_capacity_wait_ms`
  is the capacity waits finished so far; `ec2_waiting_since` is when the half's current
  wait began (null when none runs), so a page can draw the wait as it grows.
- `scoreboard` holds only measured numbers; a half is null until one of its races has
  finished. `last_ms` is the latest finished race's time, `median_ms` the median of the
  last 20 finished, `races` how many that median is over, `measured_on` the tiles the last
  race timed, `last_capacity_wait_ms` its EC2 wait (left out of `last_ms`), `last_race`
  its id. A race with no tiles in a half does not count for it.
- `races` keeps the last 10 races with every tile's phases; tiles whose host was
  reclaimed mid-race are `dropped`, borrowed ones `sitting_out`.
- `nudges` is the directed camera: `targets` are the viewpoints `nudge` may name now
  (`id`), with a label and this hold's votes; `holding` is the viewpoint held now (null
  while flying) and `next_hold_at` when the camera leaves it; `next` is the next hold once
  the vote has closed, `arrive_next_at` when the camera gets there.
- `fleet.power` is whether the GPU capacity is up, when the coordinator owns the GPU hosts'
  auto scaling groups (`--scale-group`), or with `--sleep-when-idle` on hosts it does not
  scale (a fixed host, a test box); without either it stays `awake`. After
  `--idle-scale-down` seconds (default 600) with no active viewer (`viewers.active`), and
  never while a claim is active or a race is running, the fleet goes `asleep`
  (`asleep_since`): every group is set to desired capacity 0, nothing is placed (every fcvm
  GPU still connected goes to standby, so its VM stops; the EC2 half's instances are
  terminated), no race is scheduled (`next_race_at` null) and
  `restart_now` is refused. Hosts that leave then are not reclaimed. When an active viewer
  arrives it is `waking` from `woke_at`, the moment the wake began: every group goes back
  to `--awake-capacity` (default 1) and the EC2 half launches as many tiles as the fcvm
  half had. `hosts` lists each GPU host by EC2 instance id with the phases it has reached,
  in order: `instance_pending` and `booted` from the group (Pending, then InService),
  `store_ready` when the host marked its store mounted (the agent's hello reports it; the
  phase is absent when the host does not mark it), `agent_connected` when its agent says
  hello (the agent names itself by its instance id), `tiles_rendering` when all its placed
  slots run. Once every placed tile of both halves runs it is `awake`; `last_wake` gives
  the wake's start, when it was complete (`awake_at`), the tiles placed then (`gpus`, both
  halves) and the time between (`ms`); `wakes` keeps the last 20.
- `counters` are totals since the fleet first started, kept in the state file across
  coordinator restarts: `restores` counts fcvm GPU VMs that reached their first frame
  (the wall's, races', claims' and returns'), `restores_last_hour` those in the current
  minute and the 59 before it, `scrubs` and `scrub_failures` the GPU scrubs agents
  reported.
- `fleet.standby` is the standby pool: GPUs beyond `max_tiles` (every other GPU draws the
  wall), on hosts that are not draining and not quarantined. They run no VM; their agent
  keeps them reset, scrubbed and bound, and `standby_ready` counts those that reported
  ready. A claim takes a standby GPU before it borrows a wall tile, and a standby GPU takes
  the place of a quarantined or reclaimed one when the fleet re-tiles.
- `fleet.warming` lists GPUs whose VM is starting before they join the wall. A GPU joins a
  wall that already has tiles only once its VM renders: a new host, a standby GPU taking a
  place, a GPU released from quarantine. Until then it warms off the wall (assignment
  `warm`), the wall keeps its tiling, and the tile a returning GPU will displace keeps its
  place; once it reports `running` at its generation the fleet re-tiles with it, so no
  re-tile waits on a VM that is still starting. With no tile on the wall (a cold start, a
  wake) every GPU joins at once.
- A GPU whose reset, scrub or attach fails is quarantined by its agent (Agent: `quarantine`):
  it leaves the tiling and the agent's assignment, `failures` lists it as `gpu_quarantined`,
  and it stays out (also when its agent reconnects or the coordinator restarts with its state
  file) until an operator releases it with `POST /quarantine/release`.
- `borrow`: at most `cap` wall tiles are lent at once, `lent` are now, and the next comes
  back at `next_return_at`.
- A tile whose VM did not come back carries `failure` (`kind: "relaunch_failed"`, the
  agent's message, `retry_at` when the agent tries again). `failures` lists what is not
  one tile's, each with its own `kind`: `host_reclaimed` (a spot notice or the agent went
  away; kept for a minute after the host is gone) and `coordinator_degraded` (a component
  such as the text check is failing). A reclaimed host's tiles leave the tiling in the
  event that reports it (its slots are re-tiled onto the other hosts at once), so there is
  no `draining` tile phase and no tile-level `host_reclaimed`; `fleet.hosts[].draining`
  and `failures` carry it.
- `aggregates` lists the regions an aggregator serves (see Aggregator): one per GPU host
  that has placed tiles, with the tiles inside it, and the EC2 half when the coordinator
  runs its aggregator. `frame_url` is null until the host's aggregator is serving.
- `actions` holds the latest 50 intents, committed and refused, oldest first: the
  `viewer` that sent it, the intent `id`, its `args` (a refused word's are `{}`, so its
  text appears nowhere), and the outcome.

### `GET /poster.jpg`

The latest converged still of the whole image, for link previews and the page's first
paint. Once per camera hold, 5.5 s into it, the coordinator subscribes to every aggregator
(each host's band and the EC2 half), decodes the first frame at or after that scene time,
places each region in the whole image and encodes one JPEG (through the `ffmpeg` CLI).
Served with `Cache-Control: public, max-age=10` and `Access-Control-Allow-Origin: *` (the
page reads it from its own origin); `404` with `no-store` until the first poster exists. A hold whose capture does not finish before the flight keeps the previous
poster, and so does a hold while a race runs or any placed tile is starting or
relaunching (its region would be black; claimed GPUs and dark tiles do not count). Borrowed tiles are black in it, as on the wall.

### Running it

```
fcvm fleet coord --listen <addr:port> [--agent-cmd '<command>']... [--state-file <path>]
                 [--text-check bedrock --text-check-model <id> --text-check-region <region> | --text-check none]
                 [--max-tiles 100] [--ec2 none|fake] [--allowed-origin <scheme://host[:port]>]...
                 [--ec2-aggregate-listen <addr:port> --ec2-aggregate-url <ws url>] [--aggregate-arg <arg>]...
                 [--race-every <secs>] [--restart-cooldown 60] [--camera-seed 1] [--max-borrowed <n>]
                 [--trust-cf-connecting-ip] [--turnstile-secret <secret> --turnstile-site-key <key>]
                 [--admin-listen <addr:port>] [--ffmpeg ffmpeg]
                 [--scale-group <region>:<group name>]... [--awake-capacity 1] [--idle-scale-down 600]
                 [--on-demand-group <region>:<group name>]... [--spot-wait 120]
                 [--state-bucket <bucket> [--state-key state.json] [--state-region <region>]]
                 [--ec2 aws --ec2-tiles 8 --ec2-regions <r1,r2,...> [--ec2-template fcvm-fleet-tile-x86_64]
                  [--ec2-on-demand-template <name>] [--fleet-name stage]] [--dns-zone fcvm.dev]
                 [--dns-token-secret <secret> [--dns-zone-id <id>]] [--tls-secret <secret>] [--secrets-region <region>]
```

- `--state-bucket` (also `FLEET_STATE_BUCKET`, `FLEET_STATE_KEY`) keeps the state as one S3
  object instead of a file: read at start, rewritten whole (one PUT, atomic; the bucket
  keeps versions) at most once a second while it changes and at shutdown. A missing object
  is no state; a refused read (403, which S3 also answers for a missing object to a caller
  without `s3:ListBucket`) stops the coordinator rather than start it empty.
- `--ec2 aws` runs `--ec2-tiles` EC2 tiles, whatever the fcvm side has. Each is one
  instance from the tile launch template (one-time spot), tagged `Purpose=fcvm-fleet-tile`,
  `FleetTile=<tile>` and `Fleet=<--fleet-name>` (`FLEET_NAME`, default `stage`) at launch,
  in the first of `--ec2-regions` (`FLEET_REGIONS`) with
  capacity; when every region refuses for capacity it tries `--ec2-on-demand-template`,
  and without one the tile waits for capacity (recorded on its own) and asks again every
  10 s. A tile is ready once its instance runs and its renderer answers
  `https://ec2-<n>.<zone>/health` (the name resolved to the instance) with `ready` true; a
  reply carrying `error` fails the launch like no answer at all. It is terminated by
  id. A relaunch launches the replacement first and terminates the old instance once the
  new launch is accepted. A tile whose instance never serves `/health` has that instance
  terminated and is launched again after a backoff (1 s doubling to 30 s), and so does a
  running tile whose instance is found shutting down, stopped or terminated (a spot
  reclaim; checked every 15 s). The coordinator
  keeps no instance ids across a restart, so before its first launch, and again when it
  stops, it terminates every instance tagged `Purpose=fcvm-fleet-tile` and
  `Fleet=<--fleet-name>` in `--ec2-regions`; another fleet's tiles in the same account (a
  different `--fleet-name`) are left alone.
- `--dns-token-secret` writes the slots' names in `--dns-zone` with the zone's Cloudflare
  token: for every fcvm slot and EC2 tile that reported a public address,
  `slot-<n>.<zone>` / `ec2-<n>.<zone>` AAAA (unproxied, TTL 60) and `slot-<n>-v4.<zone>` /
  `ec2-<n>-v4.<zone>` A (proxied). Records are written only when the wanted set changes,
  and records of that shape nobody wants are deleted; other names in the zone are left
  alone.
- `--tls-secret` keeps the wildcard certificate renewed (see "Certificate").

### Certificate

The wildcard certificate (`*.<zone>` and `<zone>`) lives in one Secrets Manager secret,
a JSON object: `cert` (PEM full chain), `key` (PEM private key), `domains`, `not_after` and
`issued_at` (RFC 3339 UTC) and `issuer` (fleet/tools/issue-cert.sh writes the first one).
Twice a day the coordinator reads it; within 30 days of `not_after` (or when it is missing
or does not cover the zone) it orders a new certificate from Let's Encrypt
(`--acme-directory` for another CA), answers DNS-01 with `_acme-challenge` TXT records
through the DNS token, waits until public DNS shows them, stores the new version and
removes the records. Its key is PKCS#8 (`PRIVATE KEY`), where issue-cert.sh wrote SEC 1;
both are PEM that nginx and rustls read. Renderers get it from their agent
(`--tls-secret`), tiles from their own instance role.

- `--scale-group` hands the coordinator the GPU hosts' auto scaling groups (repeatable,
  one per region); it sets their desired capacity (Auto Scaling `SetDesiredCapacity`) and,
  while waking, reads their instances (`DescribeAutoScalingGroups`) every 5 s, with the
  credentials of its task or instance role. A call that fails marks the coordinator
  degraded (`scale`) and is retried.
- `--on-demand-group` names an on-demand group as the fallback of the `--scale-group` in its
  region (at most one per region, and that region must have exactly one scale group). Auto
  Scaling does not move a spot group's capacity to on demand by itself, so the coordinator
  does: while waking, when the spot group and its on-demand group together have had fewer
  live instances (any lifecycle state but `Terminating*`, `Terminated` and `Detach*`) than
  `--awake-capacity` for `--spot-wait` seconds (default 120), it sets the on-demand group to
  the shortfall and the spot group to the live instances it has, so a spot launch that
  succeeds later cannot add a host. A split written only half way (one call failed) is
  written again on every poll until both groups hold it, and `scale` stays degraded until
  then. A group it cannot describe is not taken to be short. It never raises the on-demand
  group otherwise, sets it to 0 with the spot group when the fleet sleeps, and the next
  wake asks spot first. Before raising a spot group for a wake (also the first one after
  the coordinator starts) it reads both groups' desired capacity: an on-demand group that
  already holds hosts keeps them, and the spot group is held at the rest. The on-demand group's own `max_size` caps
  what it can be given. The on-demand instances are listed with the spot ones in `hosts`.
- `--admin-listen` serves the operator's endpoint, `POST /quarantine/release` with
  `{"host": "<host>", "sbdf": "0000:05:00.0"}`, which returns a quarantined GPU to
  rotation. It has no authentication: keep it on loopback or a private network.

- `--agent-cmd` starts an agent as a child process and talks to it over its stdin and
  stdout: `fcvm fleet agent --stdio ...` locally, `ssh <host> fcvm fleet agent --stdio ...`
  for a remote host. Repeatable. An agent that exits is removed from the fleet (its
  slots leave the tiling in that event) and restarted after a backoff of 1 s doubling to
  30 s.
- `--state-file` keeps the state as JSON, rewritten atomically (temporary file, fsync,
  rename) at most once a second while it changes, and once more on shutdown; a restarted
  coordinator resumes its version, scene, director, races, receipts and action log from
  it. Hosts, slots, claims and viewers are rebuilt as agents and browsers connect.
- `--race-every` (also `FLEET_RACE_EVERY`) is the owner's race cadence; without it races
  run only on `restart_now`.
- `--turnstile-secret` and `--turnstile-site-key` (also `FLEET_TURNSTILE_SECRET`,
  `FLEET_TURNSTILE_SITE_KEY`) turn on Turnstile verification: the site key goes out in
  each hello, and a `verify` token is checked with
  `POST https://challenges.cloudflare.com/turnstile/v0/siteverify` (the secret, the token
  and the client address).
- `GET /healthz` answers `ok`.
- `/live` refuses a cross-origin browser with `403`: a request whose `Origin` is neither
  the coordinator's own (`Origin` naming the request's `Host`) nor one given with
  `--allowed-origin`. A request without `Origin` (not a browser) is served.
- SIGTERM or SIGINT stops the coordinator after it has written the last state to
  `--state-file`; its agents' commands then end, and their VMs with them.

## Agent

`fcvm fleet agent` runs on a GPU host, one per host. It speaks newline-delimited JSON:
one message per line, UTF-8, on stdin (coordinator to agent) and stdout (agent to
coordinator); its logs go to stderr. The same lines run over a local pipe, over
`ssh <host> fcvm fleet agent --stdio`, and over the WebSocket the agent dials (`--dial`).

With `--dial wss://<api host>/agent` the agent dials the coordinator instead and sends the
same messages, one per WebSocket text frame, with the Cloudflare Access service token's
`CF-Access-Client-Id` and `CF-Access-Client-Secret` headers (from `--access-secret`, read
with the instance role). The VMs keep running while the connection is down; the agent
reconnects (1 s doubling to 30 s) and starts each new connection with its hello and the
latest of every state message (each slot's report, the aggregator, a draining notice,
quarantines), so a coordinator that lost the host learns where each slot is without a
restart. A reported generation above the coordinator's is adopted, so the next relaunch
goes past it. The coordinator serves `/agent` on its `/live` listener; Cloudflare Access
admits only the agents' service token there.

When its input closes (`--stdio`), the agent tears down every VM it started and exits. If the agent
is killed outright, each VM's fcvm gets SIGTERM from the kernel (the agent arms
`PR_SET_PDEATHSIG` on every fcvm it starts, and fcvm arms it on every process below).

### Agent to coordinator

```json
{"type": "hello", "host": "<name>", "arch": "x86_64", "gpus": [{"sbdf": "0000:01:00.0", "name": "Tesla T4"}], "capacity": 8,
 "store_ready_at": 1761134470000}
```

First line, once. `store_ready_at` (optional) is when the host's store was ready: the
modification time of `--store-ready-file`, which the host's boot writes once the store is
mounted. `gpus` are the NVIDIA display functions bound to `vfio-pci`, in address
order; `capacity` is how many VMs the host runs at once (default: one per GPU). The
coordinator makes one slot per GPU and ignores a repeated address. The first hello binds the
connection to `host`, and a later hello on the same connection must name the same host. A
hello naming a host another connection holds replaces that connection: a dialing agent that
lost its network reconnects while the coordinator may still hold the old, half-open one. The
host's slots stay, and the old connection's later messages are ignored.

```json
{"type": "slot", "id": "slot-3", "phase": "running", "generation": 4, "vm": "fleet-slot-3",
 "frame_url": "ws://<host>:<port>/frames", "ipv6": "2001:db8::3", "ipv4": null,
 "timings": {"teardown_ms": 310, "restore_ms": 4300, "ack_ms": 4300, "gpu_ms": 3100, "container_ms": 600, "first_frame_ms": 250, "total_ms": 5200},
 "error": null, "retry_in_ms": null}
```

On every phase change. `generation` is the generation the agent is working toward or
running. Phases, in order: `stopping` (tearing the old VM down), `restoring` (fcvm started;
the snapshot restore and GPU attach run inside it), `starting` (fcvm acknowledged the
restore; the container and renderer are coming up), `running` (the renderer reported its
first frame), and `failed` (`error` says why; `retry_in_ms` is when the agent tries again,
its backoff; absent when the failure quarantined the GPU). A slot assigned `standby`
reports `standby` once its GPU is free (and scrubbed, with `--scrub`) and no VM runs.

`timings`, all milliseconds, present once known:

| key | from | to |
|-----|------|----|
| `teardown_ms` | the agent starts stopping the old VM | its fcvm process has exited and the GPU is free |
| `restore_ms` | the agent starts fcvm | fcvm prints its `restored` record |
| `ack_ms` | fcvm's restore call (fcvm's own clock, the record's `ack_ms`) | the guest's ACK |
| `gpu_ms` | the guest starts its GPU attach (the record's `guest_phases.gpu_ms`) | the attach is done |
| `container_ms` | the `restored` record | the renderer's `GET /health` first answers |
| `first_frame_ms` | `/health` first answers (the scene is posted then) | `/health` reports a frame drawn after the scene was posted: `ready` is not false and `frame` is above its value in the reply read just before the post (for a renderer without `frame`, a numeric `t`) |
| `renderer_gpu_ready_ms` | the renderer process starts | CUDA is ready (`/health` `startup_ms.gpu_ready`) |
| `renderer_first_frame_ms` | the renderer process starts | its first frame is drawn (`startup_ms.first_frame_drawn`) |
| `total_ms` | the agent starts the relaunch | the first frame |

A `/health` reply carrying `error` (the renderer failed) fails the relaunch at once.

```json
{"type": "health", "id": "slot-3", "fps": 29.8, "gpu": { ... }, "budget_fit": 1210000, "t": 861.9, "render_ms_max": 14.2}
```

Once a second per running slot: the renderer's `/health` `fps`, its GPU readings,
`budget_fit` (absent while the renderer is not `ready`), `t` (the scene time of its newest
frame) and `render_ms_max` (its `frames_2s.render_ms_max`), passed through. The coordinator
reads the same fields from each EC2 tile's renderer (`GET https://ec2-<n>.<zone>/health`)
once a second.

```json
{"type": "draining", "reason": "spot interruption notice: terminate at 2026-10-22T10:00:00Z"}
```

The host is going away (EC2 `spot/instance-action` appeared). The coordinator takes the
host's slots out of the tiling at once and stops relaunching them; the agent keeps its VMs
until the host stops.

```json
{"type": "scrub", "slot": "slot-9", "sbdf": "0000:09:00.0", "ok": true}
{"type": "scrub", "slot": "slot-9", "sbdf": "0000:09:00.0", "ok": false, "error": "..."}
```

A GPU scrub finished (with `--scrub`: `<fcvm> gpu scrub <sbdf>`, exit status 0 is ok).

```json
{"type": "quarantine", "slot": "slot-5", "sbdf": "0000:05:00.0", "error": "..."}
```

The agent took a GPU out of rotation and runs nothing on it again: its scrub failed, a
relaunch failed with an error naming the GPU's address, or three relaunches in a row
failed before fcvm's `restored` record (the GPU reset and attach) or while the GPU was
still held. Failures after the restore (the container or the renderer) do not count.

### Coordinator to agent

```json
{"type": "assign", "slots": [{"id": "slot-3", "sbdf": "0000:01:00.0", "generation": 4, "rect": {"x": 0, "y": 0, "w": 0.25, "h": 0.5}},
                             {"id": "slot-9", "sbdf": "0000:09:00.0", "generation": 1, "rect": null, "standby": true,
                              "layout_generation": 14, "rect_from_frame": 25845},
                             {"id": "slot-4", "sbdf": "0000:02:00.0", "generation": 7, "rect": null,
                              "private_camera": [{"t": 812.0, "pos": [-100, 0.35, -90], "look": [-63.5, 2.2, -87], "fov_deg": 60, "intent": "<claim id>"}]}],
 "region": {"x": 0, "y": 0, "w": 0.5, "h": 0.5}}
```

The complete set of slots the host runs, sent after the hello and whenever any of them
changes, and `region`, the rectangle the host's placed slots cover together (null when
none is placed). For each slot the agent reconciles:

- no VM, or a VM below `generation`: tear the old one down, wait until its fcvm process
  has exited and no process holds the GPU's VFIO group, start a new one, wait for the
  renderer's `/health`, post the scene, report `running` with the timings. All slots of a
  host do this at once.
- a newer `generation` arriving mid-relaunch waits for the current relaunch and then runs
  once.
- `rect` changed, generation not: post the new scene to the running renderer, no
  relaunch. `rect: null` (the host drains): keep the VM, post nothing.
- `standby: true` (the standby pool): stop any VM, wait until the GPU is free, scrub it
  with `--scrub`, report `standby`, and run nothing until the slot is placed again.
- `warm: true` (with `rect: null`): the GPU is joining. Run its VM as for a placed slot and
  post the whole image (`rect` `{"x": 0, "y": 0, "w": 1, "h": 1}`) with the shared camera,
  outside the host's aggregator layout. When it is placed (same generation, `warm` gone,
  `rect` set, `previous_rect` the whole image) post its rectangle; the renderer keeps drawing
  the whole image until `rect_from_frame`, so it opens the new encoders ahead of the switch.
  No relaunch.
- a slot no longer listed: tear its VM down.
- a VM that exits on its own: report `failed`, relaunch after 1 s, doubling to 30 s; the
  backoff resets once a VM has run for 60 s.
- `private_camera` present: the GPU is claimed by a visitor. The claim and its end each
  come with a new `generation`, so a fresh VM restores; the renderer is posted the whole
  image (`rect` `{"x": 0, "y": 0, "w": 1, "h": 1}`) with this camera instead of the
  shared one, and `rect` is null so the slot is not in the host's aggregator layout.

```json
{"type": "scene", "epoch_unix_ms": 1761134400000, "image_aspect": 3.5555555555555554,
 "camera": [{"t": 0, "pos": [-32, 1.9, -55], "look": [-60, 1.9, -100], "fov_deg": 62}], "time_of_day": 0.3,
 "words": [{"text": "hello", "t": 12.5, "text_style": "lockup"}], "lights": [{"pos": [1, 2, 3], "color": [1, 0.5, 0], "t": 14.0}]}
```

The scene shared by every renderer, sent after the hello and whenever it changes. The
agent posts `scene` plus the slot's `rect` to each running renderer as
`POST http://<publish ip>:<port>/scene`, with the body
`{"epoch_unix_ms", "rect", "layout_generation", "rect_from_frame", "previous_rect",
"image_aspect", "camera", "time_of_day", "words", "lights"}`. The
coordinator posts the same body to each EC2 tile's renderer
(`POST https://ec2-<n>.<zone>/scene`).

The layout fields of that body, for the renderer (#1174 implements them):

| field | meaning |
|---|---|
| `layout_generation` | the layout generation `rect` belongs to; it only grows (0 before the first) |
| `rect_from_frame` | the frame index `k` (scene time `t = k / 30`, the same slots the renderer draws on) from which the renderer draws `rect` |
| `previous_rect` | the rectangle the tile drew before this generation, or absent when the tile is new or did not move |
| `rect` | the tile's rectangle from `rect_from_frame` on |

For a frame `k < rect_from_frame` the renderer draws `previous_rect` when present (its
streams keep the old tile size and crop until then); from `k >= rect_from_frame` it draws
`rect`. With no `previous_rect`, it draws `rect` at once. A body that arrives after
`rect_from_frame` (a late post, a relaunch) draws `rect` at once. Every renderer of the wall
gets the same `rect_from_frame` for one generation, so every tile switches at one slot; the
page and the aggregators swap their composites at that frame (see "Aggregator"). A new
scene body with the same `layout_generation` and `rect_from_frame` does not restart the
switch.

A renderer needs `rect_from_frame` at least 0.5 s after the post that brings it arrives, to
open the new rectangle's encoders off its render thread (renderer round 10). The coordinator
sets it, for a layout made at scene time `t`, to `ceil(30 t) + 30 + 15`
(`fleet_core::state::layout_from_frame`): 30 frames (1 s) for the post to reach every
renderer (coordinator to agent to renderer, or coordinator to an EC2 tile) and 15 frames
(0.5 s) for the encoders. A post that takes longer than the allowance leaves the renderer
less than 0.5 s. A stream's height can change at
`rect_from_frame` (the whole image keeps its height): the renderer sends a new stream header
there, which a subscriber must take, as the aggregator does.

```json
{"type": "budget", "budget": 707107, "t": 863.2}
```

The grid budget, sent after the hello and whenever it changes. The agent posts
`{"budget", "t"}` to every running renderer that draws a wall tile
(`POST http://<publish ip>:<port>/budget`): when it arrives, and again after every scene
post. Since renderer round 10 a scene with the same `epoch_unix_ms` keeps the budget
schedule, a new epoch keeps the budget in force, and only a scene carrying its own `budget`
clears it (the coordinator's scene never carries one), so the re-post is no longer needed to
avoid the renderer's default; it is kept because it costs one small request per scene
change and keeps a renderer that missed the original post on the grid's budget. A renderer
drawing the whole image for a claim gets no budget (it chooses its own). The coordinator posts the same
body to each EC2 tile's renderer.

### Grid budget

One budget for every tile of the wall, so tiles draw the same samples per pixel, and take
the same path (fast, or high quality from `HQ_SPP` = 8 samples per drawn pixel), at the same
frame, and moving frames match across tile edges. Tiles never adapt on their own. The
coordinator runs the rule of the renderer's reference controller
(`fleet/renderer/tools/budgetctl.py` at 55e81ad5 on `fleet-renderer-adapt`) in the pure core
(`fleet_core::budget`), on its 1 s tick:

- Every 2 s, a round over every placed tile of both sides that is not lent to a claim. No
  decision until every one of them has reported a `budget_fit` (a tile that just joined
  holds the round until its first report; a held round changes nothing, so the count of
  rounds below carries over it). A renderer that is not `ready` reports no fit.
- The smallest fit governs, smoothed half the way each round.
- The first budget is the ladder rung under smoothed fit / 1.15 (the ladder is
  250,000 x 2^(n/4), n from -8 to 40).
- Down, to the rung under the lower of the smoothed fit and this round's, when the budget
  exceeds the smoothed fit by more than 5%. When any tile's `render_ms_max` (its slowest
  render over the last 2 s) is over 90% of a slot (30 ms), at least one rung down at once.
- Up, to the rung under smoothed fit / 1.15, after three rounds in a row allow it.
- A change takes effect at one scene time for every tile: `t` = the newest frame any tile
  has drawn plus 1 s, rounded up to a frame (k/30). The agents and the EC2 poster post the
  same `{budget, t}` everywhere.

The renderer's fit is a cost model of its frames: a fixed part per frame and a cost per
sample on each path. The fast path's and the high-quality path's fits differ, and each
renderer reports the larger budget that fits 70% of a slot on either. The path a frame takes
follows from the budget and the tile's drawn size, and every tile of the wall has the same
size, so the high-quality choice is grid-wide with the budget: the slowest tile decides it
for all.

The budget does not ride on layout generations. Every tile covers the same share of the
image (each side is as wide as its share of the tiles), so one budget gives every tile the
same samples per pixel, and the same path, in any layout, and a re-tile, which every
renderer applies at the layout's frame, changes them for all tiles together. A tile's size
moves its fit little; when it does, the next rounds answer it. A tile that joins draws the
budget in force (its `t` is in the past, so it applies from its first frame) until the
round that counts it.

### Running it

```
fcvm fleet agent (--stdio | --dial <wss url> [--access-secret <secret>]) --image <renderer image> --base-port <port>
                 [--host <name>] [--publish-ip <ip>] [--frame-host <name or ip>]
                 [--gpus <sbdf>=<name>,...] [--fcvm <path>] [--fcvm-arg <arg>]...
                 [--spot-notice-url <url>] [--aggregate-port <port> [--aggregate-arg <arg>]...]
                 [--scrub] [--store-ready-file <path>] [--host-from-imds] [--secret-region <region>]
                 [--tls-secret <secret> [--tls-dir /run/fcvm-fleet/tls]]
                 [--ipv6-prefix <prefix/len> | --ipv6-prefix-from-imds] [--frame-zone <zone>]
```

`--host-from-imds` names the host by its EC2 instance id. `--tls-secret` hands the fleet's
wildcard certificate to every renderer in the renderer's TLS interface (its PROTOCOL.md,
"TLS", round 6): the agent reads the secret (the format below, "Certificate") with the
instance role, writes its `cert` and `key` as `tls.json` (a JSON object with those two
fields, mode 0600, replaced by rename) into `--tls-dir`, maps that directory read-only into
each VM at `/run/fleet-tls` with `TLS_CERT_FILE=/run/fleet-tls/tls.json` and
`TLS_PORT=443`, and reads the secret again every hour. A renewal is a new file renamed into
place; the renderer checks its file every 2 s and loads a new version for the connections
that follow (the mapped directory's attribute cache is 1 s), so a renewal needs no
restart.

`--ipv6-prefix` (or `--ipv6-prefix-from-imds`: the prefix AWS delegates to the instance's
primary interface, `Ipv6PrefixCount = 1`) runs each slot's VM in fcvm's routed mode at
`<prefix>::<GPU index + 2>` (`--network routed --ipv6-prefix <prefix> --ipv6-address
<addr>`; fcvm refuses the prefix's addresses 0 and 1). The slot keeps the address across
relaunches and reports it as `ipv6` in every slot message; the coordinator writes it to
`slot-<n>.<zone>` (AAAA, unproxied, TTL 60). The renderer's container shares the VM's
network, so a browser reaches its TLS port at that address. `--frame-zone` reports the frame
URL as `wss://<slot id>.<zone>/frames`. The host
AMI's unit (`fcvm-fleet-agent.service`, `ExecStart=/usr/local/bin/fcvm fleet agent
$FLEET_AGENT_ARGS`) takes, in `/etc/fcvm-fleet/agent.env`:

```
FLEET_AGENT_ARGS=--dial wss://fleet-api-stage.fcvm.dev/agent --access-secret fcvm-fleet/agent-access-token --host-from-imds --image localhost/fcvm-renderer:fleet --base-port 20100 --publish-ip 0.0.0.0 --tls-secret fcvm-fleet/tls-wildcard --ipv6-prefix-from-imds --frame-zone fcvm.dev --store-ready-file /var/lib/fcvm-fleet/store-ready
```

Each slot's VM is `fcvm podman run --name fleet-<slot> --kernel-profile gpu --gpu <sbdf>
--publish <publish ip>:<base port + gpu index>:8080 [--fcvm-arg ...] <image>`, run
rootless; the pre-start snapshot cache makes every launch after the first a restore. The
frame URL is `ws://<frame host>:<port>/frames`, or the slot's name with `--frame-zone`. `--gpus` replaces
the sysfs inventory (tests and hosts without vfio-pci). `--spot-notice-url` polls an EC2
metadata URL (IMDSv2) for `spot/instance-action` every 5 s.

With `--aggregate-port` the agent runs `fcvm fleet aggregate --listen <publish ip>:<port>`
as a child (it dies with the agent, and is restarted with backoff if it exits), posts it
the layout (`region`, every placed slot's rect and renderer URL, the scene's epoch) after
every assignment or scene change, and reports it once it answers:

```json
{"type": "aggregate", "frame_url": "ws://<frame host>:<port>/frames"}
```

## Aggregator

`fcvm fleet aggregate` joins one region's tiles into one stream: the agent runs one per
GPU host for its band, the coordinator one for the EC2 half. It subscribes to every tile
(`<renderer url>?h=<tile height>&q=hi`, near-lossless; the renderer's highest bitrate
until `q=hi` exists), decodes each frame, and for each slot `k` (t = k/30) waits until
every streaming tile has its frame for `k` or 150 ms past the slot's time. It then draws
each tile's inner rectangle (crop margins dropped) at its place in the region, a tile
with no frame within the last 3 slots black (relaunching, dark, down), and encodes the
region once (libx264, zero-latency tuning, no B-frames, a keyframe every 2 s, High
profile 4:2:0, BT.709 limited range). Decoding and encoding run in `ffmpeg` child
processes; the host needs the `ffmpeg` CLI with libx264.

- `GET /frames` (WebSocket): the renderer's frame protocol. The header is
  `{"codec","width","height","full_width","full_height","x0","y0","fps":30,
  "crop":{"left":0,"top":0,"right":0,"bottom":0},"aggregate":true,"tiles":[...]}`,
  `x0, y0` the region's top-left pixel in the whole image; then FCVF frames, each at the
  slot's `t`. A viewer that joins gets the header and every frame since the last
  keyframe, then live frames; one that falls behind resumes at the next keyframe. A
  layout change sends a new header and a keyframe. A tile stream's own header can change
  mid-stream (a re-tile changes its height at `rect_from_frame`); the aggregator takes the
  new header and decodes the frames that follow at its size. Each generation subscribes
  with `?h=` of its own tile height.
- A layout of a newer `generation` (a re-tile) swaps the way the page does: the
  aggregator keeps composing the old layout up to the frame before `from_frame` and
  subscribes to the new layout's tiles, dropping their frames before `from_frame`. Once
  every tile of the new layout has delivered a frame at or after `from_frame` (or 2 s have
  passed since `from_frame`'s time, or since the layout arrived if that is later; a tile
  that has delivered nothing is then drawn black), the new layout takes over at once, from
  the first frame every tile still holds, with a new header and a keyframe. Until then
  viewers hold the old composite's last frame. No frame mixes the two generations. A layout
  of the same generation (a tile's URL changed) replaces the old one at once.
- The region's size is its share of a whole image `--image-height` tall (default 1080):
  the fcvm half of a 1920 x 1080 image is 960 x 1080. `?h=` is accepted and ignored.
- `POST /layout`: `{"epoch_unix_ms","image_aspect","region":{x,y,w,h},
  "tiles":[{"id","rect","url"}],"generation","from_frame"}` (`generation` and `from_frame`
  default to 0: the layout generation of the tiles' rects and its frame; the agent sends
  its placed slots' newest, the coordinator the EC2 side's); `GET /layout` returns it.
- `GET /stats`: frames out, the output bitrate over the last 10 s, and percentiles of
  `after_last_tile_ms` (last tile frame used to the region frame out of the encoder),
  `after_first_tile_ms`, `encode_ms`, `tile_lateness_ms` and `out_lateness_ms` (wall
  clock minus the slot's scene time), and per tile whether it is streaming.

## Renderer

The renderer is the program in the container of every GPU VM (fcvm and EC2
alike). It draws one rectangle of one shared image and streams it as H.264.
The agent (or, locally, any client) sets the scene with `POST /scene`;
browsers read frames from `GET /frames`, a poster frame from `GET /still`
and stats from `GET /health`.

The renderer listens on `0.0.0.0:8080` (IPv4 and IPv6, plain HTTP and
WebSocket) for host-local clients, and, given a certificate, on `:443` with
TLS for browsers (HTTPS and WSS, the same routes; see "TLS"). Source:
`fleet/renderer/`.

### Coordinates and time

- **World units are kilometres.** A position is `[x, altitude, z]`: `x` and
  `z` are horizontal, the second number is the altitude above sea level. The
  renderer bends the world into a planet (radius 6360 km) itself, so altitudes
  stay altitudes anywhere on the map. The default flight works around
  `x, z in [-60, 0]`; land, sea and mountains continue in every direction
  (the GPU's baked terrain covers |x|, |z| < 512 km and is computed directly
  beyond, at a higher cost per frame).
- The eye is kept at least 60 m above the ground and the sea; a keyframe
  below that is lifted. It may go as high as you like: above about 10 km the
  terrain march reaches the horizon, and above 60 km the camera is outside the
  atmosphere, which is then drawn as a limb around the curved planet
  (`tools/scene-high.json` holds at 80 km).
- The world has one fixed landmark: an orbital ring, a band 120 km wide at
  420 km above the surface on an axis through the planet's centre, seen as an
  arc over the southern sky: lanes of plating and solar panels, lattice edges
  and two open gaps crossed by trusses. It shades the ground where it crosses
  the sun and is dark where the planet's shadow falls on it. It is part of the
  world, like the terrain, and needs nothing in the scene.
- Round 6: mountain basins hold lakes (water at a level of their own, a few
  metres below the basin floor); the sea is at altitude 0. The eye is kept 60 m
  above either.
- **`t`** is seconds since `epoch_unix_ms` on the VM's clock (chrony keeps the
  VMs in step): `t = (now_unix_ms - epoch_unix_ms) / 1000`.
- **Frames are drawn on a fixed grid of times:** `t = k / 30` for integer `k`
  (`FPS`, default 30). Every renderer draws the same `t` values, so frames of
  different tiles with equal `t` are the same instant; a renderer that cannot
  keep up skips slots rather than drawing between them. The `t` in a frame
  header is that exact double.

### `POST /scene`

`Content-Type: application/json` is required (anything else is `415`), so a
web page cannot send a scene without a CORS preflight, which the renderer never
grants. The body is at most 1 MiB.

```json
{"epoch_unix_ms": 1761100000000,
 "rect": {"x": 0.5, "y": 0.0, "w": 0.0625, "h": 0.111},
 "image_aspect": 1.7778,
 "camera": [{"t": 0.0, "pos": [-16, 0.3, -19], "look": [-42, 1.3, -72], "fov_deg": 60.0}, ...],
 "time_of_day": 0.30,
 "words": [{"text": "HELLO", "t": 12.0}],
 "lights": [{"pos": [-36, 1.75, -61], "color": [1.0, 0.45, 0.15], "t": 30.0}]}
```

| field | meaning | accepted |
|-------|---------|----------|
| `epoch_unix_ms` | the scene clock's zero | integer |
| `rect` | this renderer's part of the whole image, normalized, origin top-left | inside the unit square, `w` and `h` at least 1/256 |
| `image_aspect` | whole image width / height | 0.1 to 10 |
| `camera` | keyframes; sorted by `t` on arrival | 1 to 4096 entries; `fov_deg` (vertical) 1 to 170 |
| `time_of_day` | sun position: 0.25 sunrise, 0.5 noon, 0.75 sunset (the built-in scene uses 0.245, golden hour) | 0 to 1 |
| `words` | viewer text: `{"text", "t", "text_style"}` (see "Words, the clock and lights") | optional; may be empty |
| `lights` | viewer lights | optional; may be empty; `color` components 0 to 64 |
| `budget` | pixel-samples per frame for every tile (see "Budget") | optional; 10,000 to 50,000,000; absent means the renderer's `BUDGET` |
| `layout_generation` | the coordinator's layout this rect belongs to (see "Layout generations") | optional; a non-negative integer or null |
| `rect_from_frame` | the first frame (scene time x 30) drawn with `rect` | optional; a non-negative integer or null |
| `previous_rect` | the rect drawn for frames before `rect_from_frame` | optional; null, or a rect as above; needs `rect_from_frame` |

Answers `200 {"ok":true}`, or `400 {"error":"<what is wrong>"}` naming the
field. A scene replaces the previous one whole.

#### Layout generations (addition, round 10)

The coordinator re-tiles a running wall by posting every renderer a scene with
a new `layout_generation`, its new `rect`, its old one as `previous_rect` and
one `rect_from_frame` K shared by all of them, far enough ahead that every post
arrives first. Each renderer then:

- draws frames before K with `previous_rect` and frames from K with `rect`, so
  the frames before K stitch under the old layout and the frames from K under
  the new one;
- starts its history again at K (every renderer, whether its rect moved or not:
  a moving frame reads the 8 frames before it, and those were drawn under
  another layout on some neighbours), so frame K and the 7 after it draw like
  the first frames of a stream;
- sends every subscriber a new text header at frame K, with the new tile's
  size and position and `layout_generation`, followed by a keyframe. A
  subscriber keeps the whole image's height: its height is converted so that
  `full_height` stays what it was under `previous_rect` (asked at `h=270` for a
  1/2-high cell of a 540-high image, it gets `height` 540 after a move to a
  full-height column).

A scene whose `layout_generation` is the one in force, or already pending,
changes nothing about the switch (the coordinator re-posts the same body).
Without `layout_generation` a scene takes effect at once, as before: it
cancels a pending switch, and headers sent after it carry `layout_generation`
null. With it
and no `previous_rect`, the new rect applies from K and frames before K use it
too. A re-tile keeps the clock, so it keeps the budget schedule (see "Budget"),
and the budget the coordinator re-posts after the scene, with a `t` at or
before the current frame, changes nothing: every frame drawn between the two
posts already has the grid's budget.

A switch must skip no slot: a tile that skips one lacks that frame in its
history, and its moving frames stop matching its neighbours' for 8 frames. So
the renderer opens the new layout's encoders ahead of K, on a separate thread,
as soon as the scene arrives, and tears the old ones down on that thread after
K (opening took 10 to 25 ms per encoder and tearing down 29 to 245 ms on a T4;
done on the render thread they skipped 1 to 8 slots at K). K must leave time
for that: at least 0.5 s after the last post arrives.

#### Camera interpolation and holds

- Between two keyframes that differ, `pos` and `look` follow a Catmull-Rom
  spline (tangents scaled to each segment's duration) and `fov_deg` is linear.
- **A hold** is a run of consecutive keyframes with identical `pos`, `look`
  and `fov_deg`. Inside it the camera is exactly still. Before the first
  keyframe and after the last, the nearest keyframe holds (and joins any hold
  it starts or ends).
- Next to a hold the tangent is zero, so motion eases out of and into holds.
- To let the image converge, include holds: two equal keyframes a few seconds
  apart.

#### Words, the clock and lights

A word (an entry of `words`):

| field | meaning | accepted |
|-------|---------|----------|
| `text` | the message | a string; printable ASCII is kept (other bytes below 0x80 become `?`, bytes from 0x80 are dropped), at most 28 characters, case kept |
| `t` | scene time at which it appears | a number |
| `text_style` | how it is drawn (round 12) | optional; `lockup` (the default), `skywriting` or `landmark`; absent or null means `lockup`; any other value is refused with `400` naming `words[].text_style` |

- Each word is drawn in the `text_style` it was sent with. Only the newest
  word that has appeared and not yet expired is drawn: a newer one replaces
  it whatever either's style. There is no scene-wide style.
- All text is set in Inter (SIL Open Font License 1.1,
  `fleet/renderer/fonts/OFL.txt`), weight 600, from a signed distance atlas
  built into the binary (`src/font_sdf.h`, generated by
  `tools/make_font_sdf.py`), with the font's kerning. It is drawn every frame
  at that frame's `t`, exactly (no sampling), as rays against geometry in the
  world, on top of the accumulated image (below), so every tile draws the
  same pixels.
- **The clock** is always drawn, the same in every style: the time of the
  frame (`epoch_unix_ms / 1000 + t`, to the second) in America/Los_Angeles,
  labelled `PDT` or `PST`. Daylight time runs from 2:00 local on the second
  Sunday in March (10:00 UTC) to 2:00 local on the first Sunday in November
  (9:00 UTC); the renderer applies that rule in code. Warm white type, 2.9%
  of the image height to the cap, its left end at 5% of the width from the
  left edge and its cap top at 17% of the image height from the top, whatever
  the word's style. Its digits sit on one fixed advance so it does not shift
  as it counts. It stands 1.2 km in front of the camera, along the camera's
  right and up axes (so it keeps its place in the frame), and it is drawn
  over whatever the pixel shows: no cloud or terrain hides it. Its light is
  set against the frame's exposure, so it is near white at any time of day,
  and a soft shade around its letters (fading out over 0.14 em) holds what
  lies behind them below 0.22 of the exposed range: bright cloud is darkened,
  dark sky and ground are left as they are.
- A word appears at its `t`, fading in over 1.5 s, and fades out at 90 s
  (skywriting sooner, below). By style:

| style | the word |
|---|---|
| `lockup` | the clock's type at 4.9% of the image height to the cap, 6 km out (the haze dims it and scatters its light around the letters), with the same shade at 80%, left-aligned with the clock above it, its cap top at 8% of the image height from the top; a line wider than 57% of the image breaks at its middle space onto two lines (the last line keeps the one-line baseline, the first sits above it). Laid out on the screen like the clock; nearer terrain hides it. |
| `skywriting` | smoke written across the sky 9 km out, in the plane facing the camera where it was when the word arrived (upper middle of that view), 8.5% of that view's height to the em. Puffs lie along each glyph's centreline (from the glyph's skeleton, 0.055 em apart) and appear left to right at 2.2 em/s. At the head a puff is the stroke's own width and dense; behind it each grows 0.003 em a second (thinning, keeping its smoke), drifts with the wind (0.008 em/s right, 0.003 em/s up) and wanders on its own. Lit by the sun with the clouds' own scattering (several orders, through the puff's depth toward the sun) and by the sky. It holds until 25 s after the last letter, then fades out over 8 s. |
| `landmark` | big solid letters standing on a ridge in the view the camera had when the word arrived, facing where the camera was, cap height 6.5% of that view's height. The ridge: of the ground a fan of rays just below the horizon meets first (2.5 to 14 km out, above the sea), among the points at least 60% as high as the highest, where the whole row is in that view, no ground hides it (checked 0.15, 0.4 and 0.7 em above the ground along it) and the ground under it is flattest; 6 km ahead when there is none. Each letter stands on the ground under its middle. The glyphs extruded 0.22 em, painted white, lit by the sun (shadowed by terrain and clouds) and the sky; they cast shadows on the terrain in the path-traced samples, nearer terrain hides them, and at dusk a soft warm light runs along the edges of their faces. A new landmark word starts a new hold. |

Skywriting and the landmark stay where the camera was looking when the word
arrived, so a moving camera can leave them behind; the clock stays on the
screen.

- A **light** appears at its `t` as a glowing sphere (35 m) of its `color`, and
  lights the terrain within 3 km. At most 32.

### `GET /frames?h=<height>&q=<quality>` (WebSocket)

`h` is 16 to 2160. `q` is optional: `hi`, `std` (the default) or `lo`; see
"Stream quality". The tile's size follows from `h` and the rect; see
"Tile geometry". The first message is text:

```json
{"codec":"avc1.64001f","width":512,"height":556,
 "full_width":1920,"full_height":1080,"x0":480,"y0":0,"fps":30,
 "crop":{"left":16,"top":0,"right":16,"bottom":16},"q":"std","bitrate":6830899,
 "layout_generation":2}
```

`codec`, `width`, `height` are the contract: the size of the delivered
(decoded) pictures. The renderer adds `full_width`, `full_height` (the whole
image's pixel grid at this height), `x0`, `y0` (the tile's top-left pixel in
that grid), `fps` and `layout_generation` (the layout the tile belongs to,
null without one; a new header arrives at the frame a new layout takes
effect).

**`crop` (addition, round 3).** Each picture is the tile plus an overlap of up
to 16 pixels (`MARGIN`) on every side that lies inside the image (none past the image's
own edges; one side is one pixel short where that keeps the size even). The
client draws only the inner rectangle:

```
source x = crop.left,  y = crop.top,
       w = width - crop.left - crop.right,  h = height - crop.top - crop.bottom
destination (in the whole image grid) = (x0, y0), same w x h
```

The overlap pixels are the neighbours' pixels, drawn under the same rules, so
the encoder codes each tile edge from both sides of it like any other pixels
inside its picture; a tile edge is never an edge of an encoded picture, where
H.264 neither deblocks nor predicts across. A client that ignores `crop` sees
each tile 16 pixels too large on its inner sides.

Then binary messages, each one access unit:

| bytes | content |
|-------|---------|
| 0-3 | ASCII `FCVF` |
| 4 | version, 1 |
| 5 | flags: bit 0 keyframe |
| 6-7 | width, u16 LE (the delivered picture, overlap included) |
| 8-9 | height, u16 LE |
| 10-17 | `t`, f64 LE |
| 18-21 | frame number, u32 LE (the renderer's own count; it skips numbers for frames a subscriber did not get) |
| 22- | one H.264 Annex-B access unit; SPS and PPS precede every keyframe |

- H.264 High profile, no B-frames, NVENC with ultra-low-latency tuning,
  BT.709 limited range (signalled in the VUI), CBR at the quality's rate (the
  header's `bitrate`, bits per second).
- The first binary message is a keyframe. Keyframes follow at least every 2 s.
- **A stream's first frames (round 10).** A moving frame averages the 7
  frames before it, so a tile whose stream starts must have those 7 or its
  first frames differ from a neighbour whose stream started at another slot.
  When a tile starts drawing at a new size (a stream starts or changes
  height; not at a layout switch, where every tile starts its history at the
  same frame, and not for a whole-image renderer), it draws the current slot
  and the following ones back to back, unsent, until it has drawn 7 and is at least a
  slot ahead of the clock (at most 90 slots), then continues on the
  clock. A tile that took more than 90% of a slot for its last frame stops
  after 7 unless its frames are still getting faster (a GPU leaving its idle
  clocks: 585 MHz on a T4 against 1590, about 1.5 s to full speed): it cannot
  get ahead, and skips slots on the clock as before. Its first frame sent has
  all 7 behind it.
- **Warming (round 10).** A posted scene keeps the renderer drawing every
  slot for 10 s at the height it last streamed (540 before any stream), as
  it would for a subscriber. A tile that joins or rejoins the wall gets its
  scene before its stream, so its GPU has left the idle clocks and, when the
  stream asks for the height being drawn, the 7 slots before its first frame
  are already drawn and it starts at once. A stream at another height primes
  as above. The encoder opens on a
  second thread meanwhile, and NVENC's minimum frame size is asked once at
  start on that thread (opening a session to ask took about 250 ms on the
  render thread). Measured on a 2 x 2 grid at 270 px: priming took 111 to
  392 ms (7 to 13 slots; a GPU clocking up from idle draws its first frames
  slower than later ones) at 353,553 pixel-samples, so a stream's first frame
  arrived 0.1 to 0.4 s after it subscribed. At 500,000, where one cell drew a
  frame in about a slot, priming took up to 1.9 s (57 slots) and that cell
  still skipped a slot right after it, as any tile that slow does at any
  time; the controller's 70% target keeps tiles clear of that.
- The client may send text `{"h": <height>}`. The renderer then sends a new
  text header and continues from a keyframe at the new size.
- A subscriber that falls more than 90 messages or 16 MiB behind has its queue
  dropped and resumes at the next keyframe (which the renderer then forces).
- **Minimum size:** NVENC's H.264 encoder on a T4 takes nothing narrower than
  145 px or shorter than 49 px. When the requested height gives a smaller
  tile, the tile is drawn at the smallest larger even height that fits (a
  1/16 x 1/9 rect of a 16:9 image asked at `h=136` has a 146 x 146 inner
  rectangle). The header says the actual sizes; the client scales.
- The renderer draws once per frame at the largest height any subscriber
  wants, scales it down (area filter) for the smaller ones, and encodes each
  (height, quality) pair that has subscribers, once.

#### Stream quality (addition, round 4)

| `q` | bits per pixel of the delivered picture, at 30 fps | floor | cap | preset | for |
|---|---|---|---|---|---|
| `hi` | 4 | 2 Mbit/s | 160 Mbit/s | P4 | an aggregator on the same host that decodes and re-encodes once: near-lossless, so its own encode sees no tile edges |
| `std` | 0.8 (`ENC_BPP`) | 0.6 Mbit/s | 20 Mbit/s | P6 | the default; tile edges decode within the seam ratio of the unencoded image |
| `lo` | 0.25 | 0.15 Mbit/s | 20 Mbit/s | P6 | direct mode at high tile counts, where a browser receives many small tiles |

All three are CBR with no B-frames and the same keyframe rules. Measured
seam ratios and rates per quality are in the PR and `fleet/renderer/README.md`.

#### Tile geometry

For a requested height, `H` is `h` clamped to 16..2160 and rounded down to
even. Then

```
full_h = round(H / rect.h)          full_w = round(full_h * image_aspect)
y0     = round(rect.y * full_h)     x0     = round(rect.x * full_w)
W      = round(rect.w * full_w), rounded up to even
```

and the tile is whole-image pixels `[x0, x0+W) x [y0, y0+H)`. W is at most
4096 (NVENC H.264): when it would be wider, `H` is lowered to
`floor(H * 4096 / W)` rounded down to even and the geometry worked out again, so
the tile still covers the whole rect at a lower height. Tiles of a grid
asked at one height share `full_w x full_h`; neighbours meet with no gap and at
most one pixel of overlap, and an overlapping pixel is the same whole-image
pixel in both, drawn identically.

### `GET /health`

```json
{"fps":30.0,"heights":[240],"streams":[{"h":240,"q":"std"}],
 "gpu":{"name":"Tesla T4","temp_c":38,"power_w":64.2,"util_pct":70,"mem_used_mb":648.3},
 "t":1234.5,"frame":5678,
 "ready":true,"render_ms":13.0,"encode_ms":0.5,"spp":3,"checkerboard":1,"hold":false,"hold_samples":0,"slots_missed":0,
 "startup_ms":{"gpu_ready":1515,"first_frame_drawn":1845,"first_frame_sent":1899}}
```

The first six keys are the contract. GPU numbers come from NVML, sampled once
a second; a value NVML does not give is `null`. The rest: `ready` (the GPU is
initialized), `error` (only when the renderer has failed), `render_ms` and
`encode_ms` of the last frame, `streams` (each height and quality being
encoded), `spp` (samples per pixel drawn in it), `checkerboard` (see below),
`hold`/`hold_samples` (see below), `slots_missed` (frame slots skipped while a
subscriber was connected, since start), and `startup_ms`, milliseconds from the
process start (read from `/proc/self/stat`) to CUDA being ready, the first
frame drawn and the first frame sent to a subscriber (`-1` until it happens).
`layout_generation` is the layout in force (null without one), and
`layout_pending` (`{"generation", "from_frame"}`, present only while one is
waiting) the next.
`Access-Control-Allow-Origin: *` is set on this route only.

### Sampling, accumulation and determinism

Every pixel is a path-traced estimate. Its random numbers are seeded by
(whole-image pixel, seed, sample index), never by the tile or the GPU, and no
value is reduced across pixels, so a pixel's samples are the same wherever it
is drawn.

**Budget.** Each frame spends a fixed number of pixel-samples (its budget)
over the pixels it draws: its rect plus the stream overlap plus a guard band
(below), counted before clipping to the image. The samples per pixel are
`round(budget / drawn pixels)`, clamped to 1..64, so every tile of a grid
given the same budget draws the same number. Moving frames match across tile
edges only if every tile uses the same budget at the same slot and draws
every slot.

The budget for a slot is, in this order (round 8):

1. **A posted schedule.** `POST /budget` with `{"budget": B, "t": T}`
   (`Content-Type: application/json`; B in 10,000..500,000,000) sets budget B
   from the first slot at or after scene time T, until a later entry takes
   effect. A later entry for the same slot replaces it. A T at or before the
   current frame applies from the next frame drawn. A new scene with the same
   `epoch_unix_ms` (a re-tile) keeps the schedule; one with a new
   `epoch_unix_ms` keeps only the budget in force at the last frame drawn, from
   slot 0, since the old entries' times are on the old clock; one with its own
   `budget` clears it (round 10; until then every scene cleared it, so a frame
   drawn between a scene post and the controller's next budget post fell back
   to `BUDGET` on some tiles and not others). `200 {"ok":true}` or `400`
   naming the field.
2. The scene's `budget`.
3. For a renderer drawing the whole image (`rect` is the unit square, so it
   has no edges to match) with `ADAPT=1` (the default): its own fit, rounded
   down to the ladder below.
4. `BUDGET` (250,000).

A hold keeps the budget it started with (its samples per frame and its
quality stay fixed while it accumulates); a new budget reaches a hold at the
next hold.

**Adapting a grid.** Each renderer reports in `/health`: `budget` (the last
frame's), `budget_source` (`schedule`, `scene`, `local` or `default`),
`budget_fit`, `hq` (below), `trace_ms` (the part of `render_ms` spent drawing
samples) and `frames_2s` (over the last 2 s: `n`, `frame_ms_p50`,
`frame_ms_p99`, `frame_ms_max` for a whole frame including encoding and
sending, `render_ms_max`, `encode_ms_max`). `budget_fit` comes from a cost
model of the frames the renderer drew: the fixed part of a frame
(`render_ms - trace_ms`) and the cost per sample of each path (`trace_ms` /
samples drawn), each the 95th percentile over the frames of the last 180 s,
so the fit holds over what the camera has shown in that time instead of
following the latest frames (round 10: on the coordinator's directed camera
the latest frames' fit swung 2x and the grid budget changed 9 times in 2
minutes). It is 0 until the window spans 60 s (a controller leaves it out);
the model starts again when the drawn size changes. `budget_fit_now` is the
same fit from the latest frames (smoothed over about five), for diagnosis. It is the largest budget whose predicted frame takes 70% of a
slot, on the fast path below `HQ_SPP` samples per drawn pixel or on the
high-quality path from it; until a renderer has drawn a high-quality frame it
assumes that path costs `HQ_COST_PRIOR` (1.0) times the fast one per sample.
So a tile that cannot afford the high-quality path reports a fit just under
`HQ_SPP` samples per pixel.

One controller sets one budget for all tiles of a grid. Every 2 s it reads
every tile's `budget_fit`, takes the smallest (the slowest tile governs) and
smooths it over rounds (half the way each round), and posts the budget in
force to every tile with one `t`, at least a second past the newest frame any
tile has drawn, so every tile switches at the same slot. The budget moves on
the ladder `250,000 x 2^(n/4)` (n from -8 to 40) with hysteresis: down, to the
rung under the fit, when the budget exceeds the smoothed fit by more than 5%,
and by at least a rung at once when any tile's `render_ms_max` is over 90% of
a slot; up, to the rung under fit / 1.15, after three rounds in a row that
allow it. It posts only when every tile has reported. Tiles never adapt on
their own. `tools/budgetctl.py` is the reference controller; the fleet
coordinator is to run the same rule.

**The high-quality path.** From `HQ_SPP` (8) samples per drawn pixel, with
every pixel drawing, a frame takes the high-quality path: the full cloud march
(fine steps of 1.5% of the distance, the light march every second step with
four samples, until transmittance falls under 0.01), a second diffuse bounce
with the first bounce's sunlight shadowed, a sun shadow march of up to 56
steps over 60 km, and 12 aerial-perspective steps. Below it a frame takes the
fast path of round 7. The choice follows from the budget and the tile size, so
every tile of a grid makes the same one at the same slot. `/health` and the
`/raw` tile header report `hq`.

**Checkerboard (round 4).** Where the budget gives under 0.5 samples per
drawn pixel, a frame draws one sample on half its pixels (stride 2: those
whose whole-image `X + Y + k` is even, `k` the frame slot), and under 0.3 on
a quarter (stride 4: one pixel of each 2 x 2 block, top-left, bottom-right,
top-right, bottom-left over four slots). At the default budget that is a 720p
or 1080p view; wall tiles keep a sample on every pixel. Every pixel that drew
nothing is filled from its 8 neighbours that did, at half their weight, and the
history below does the rest. Round 6: where one or more pairs of opposite
neighbours (left-right, up-down, the two diagonals) both drew and share a hit
kind, the fill averages the pair whose brightness (and, on land and water,
distance) differs least, so an edge is continued along its own direction;
otherwise it averages the neighbours of the first one's hit kind (left, right,
up, down, then the diagonals). In a hold, a pixel's next sample index is its own sample count, so a
hold's sum is still independent of how frames split it. The pattern, the fill
and the rule for when to use it name only whole-image pixels, slots and the
budget, so the matching rules below hold unchanged. `/health` reports the
stride as `checkerboard` (1 when every pixel draws).

**While the camera holds** (see "Camera interpolation and holds"):

- The hold has a seed computed from the hold's start `t`, its pose, the time of
  day, the whole-image size and the lit lights (a light appearing, or any
  change to those, starts a new hold). The rect is not part of it.
- Sample `k` of a pixel in a hold is a pure function of (pixel, hold seed, `k`),
  drawn at the hold's start time (clouds and water stand still during a hold).
  Each frame adds the next samples to a per-pixel sum in order, so the sum
  after `n` samples does not depend on how they were split across frames.
- Accumulation stops at 1024 samples per pixel.
- While fewer than 256 samples are in, the last moving frame's output (below)
  is blended in, weighing up to 16 samples and falling linearly to zero at 256
  samples, so the switch from motion to hold does not flash noisy. From 256
  samples on, a held pixel is the hold's own path-traced average and nothing
  else.
- **Result:** once a renderer has at least 256 samples of a hold, its pixels
  depend only on (scene, hold, sample count, whole-image pixel) and, for the
  overlays, on `t`. Two renderers with the same sample count give identical
  pixels; at 1024 every renderer gives the same image. A relaunched renderer
  starts at 0 and catches up, showing as a noisier patch that heals.

**While the camera moves:** frame `k` (at `t = k/30`) draws its samples at
its own `t`, seeded by (pixel, `t`, sample index), over its rect **plus a guard
band** of `g + 4` pixels past the stream overlap on each side, where `g = ceil(0.015 * full_h)`
(clipped to the image). It keeps the last 8 frames it drew. Each pixel of the
rect is then the sample-weighted average of

- this frame's samples, and
- for each of the frames `k-1 .. k-7` this renderer drew: that frame's color
  at the point where this pixel's first hit was in that frame's camera
  (bilinear over four pixels), counted only where the four pixels' hit kind
  (sky, land, water) matches, their hit distance agrees within 4% (land and
  water only), and the point moved at most `g` pixels in x and in y. A frame
  weighs its sample count, at most 64.
- Round 6: a pixel's "first hit" is the cloud in front of what it shows when
  that cloud lets through under half the light (over the sky) or under a fifth
  (over land and water), at the cloud's transmittance-weighted distance, taken
  from the pixel's first sample. Its hit kind stays that of what lies behind.
  Before, a near cloud was reprojected through the terrain behind it, or
  through the sky at infinity, and smeared while the camera moved fast.

This is computed for the rect plus the stream overlap (16 pixels) plus 2
pixels on each side. Where a pixel's
total weight is under 48 samples (fast motion, a fresh renderer, a 1080p view)
it is then filtered: a 5 x 5 edge-aware average of those resolved pixels with
the same hit kind, a depth within 3% and similar brightness, blended in fully
below 16 samples and not at all from 48. Converged holds are not filtered.

Every input of that rule is a whole-image pixel, a frame time or the scene, and
every pixel it can read lies inside the drawing renderer's guard band (the
filter reads 2 pixels past the overlap, whose reprojection reads `g + 1` past
those), so:
**two renderers that drew the same frames `k-7 .. k` give identical pixels for
frame `k`**, whatever their rects. A renderer that started (or missed a slot)
less than 8 frames ago differs only until 8 frames have passed. History older
than 8 frames contributes nothing. Motion faster than `g` pixels over the
window simply uses fewer frames, the same way on every renderer.

During a hold, each frame's entry in those 8 is the hold's current average, so
the first moving frames after a hold reproject the converged image instead of
starting from noise.

**Accumulation off** (`ACCUMULATE=0`, and the seam test): each frame is the
filtered average of its own samples, a pure function of (scene, `t`, samples
per pixel, whole-image pixel).

**The seam test** (`fleet-renderer test`, run inside the container on a GPU)
draws a 960 x 540 image whole and as a 2 x 2 grid of tiles and compares them
pixel for pixel, 8-bit and float: with accumulation off on a moving frame; in a
hold with 16 samples split 4 x 4 for the whole image and 2 x 8 for the tiles;
on the 12th of 12 consecutive moving frames with history; and the same two with
checkerboard frames of stride 2 and 4. Two controls (a tile drawn one pixel off, another `t`)
must differ.

### `GET /still?h=<height>&quality=<1-100>` (addition, round 5)

A poster frame: the next frame of this renderer's rectangle, as a baseline
JPEG (`image/jpeg`, 4:2:0, default `quality` 90), with the same
`X-Fleet-Tile` header as `GET /raw` (it says `hold` and `hold_samples`, so a
page can tell how far the image has converged). `Access-Control-Allow-Origin:
*` and `Access-Control-Expose-Headers: X-Fleet-Tile` are set, so a page can
read it with `fetch`.

- `h` is the tile height, as in `GET /frames` (16 to 2160, default 540;
  anything else is `400`).
- While a stream runs, the frame is the stream's, at the stream's height.
- With no stream, the request makes the renderer draw every slot for tile
  height `h` for the next 30 s, and the frame returned is drawn that way. In a
  hold, each frame adds samples (the same accumulation as a stream), so a
  second request a few seconds later returns a more converged image.
- `503` if no frame was drawn within 15 s (the GPU is not ready yet).

### TLS (addition, round 6)

Browsers reach renderers directly over IPv6, so the renderer terminates TLS
itself.

- With `TLS_CERT_FILE` set, every route is also served over TLS on `TLS_PORT`
  (443): `https://` for `/health`, `/scene`, `/still`, `/raw`, and `wss://`
  for `/frames`, with the same requests and responses as plain. Plain HTTP
  stays on `PORT` (8080) for host-local clients (an aggregator, the agent).
- **The file** is a JSON object in the format of the fleet's wildcard secret
  (`fleet/tools/issue-cert.sh` on `fleet-deploy` documents it): `cert`, the
  leaf certificate followed by its intermediates, PEM; `key`, the leaf's
  private key, PEM. Other fields are ignored. The host hands it to the VM
  (for fcvm: `--map <dir>:/run/fleet-tls:ro --env TLS_CERT_FILE=/run/fleet-tls/tls.json`).
- **Reload:** the file is checked every 2 s. A new version of it (renamed into
  place, or rewritten: a new inode, size or modification time) is loaded and
  used for every connection accepted after that; connections already open,
  streams included, keep the pair they started with and are not dropped. A
  version that does not load (unparsable, no certificate, a key that does not
  match) is logged and the pair in use stays. Until a pair has loaded, TLS
  handshakes fail and plain HTTP works as before.
- TLS 1.2 and 1.3 only. TLS 1.2 uses ECDHE with AES-GCM or ChaCha20-Poly1305;
  no renegotiation, no compression. ALPN selects `http/1.1` when offered.
- `/health` adds `"tls": {"loaded": bool, "loads": n, "not_after": "<RFC 3339>"}`
  (`loads` counts pairs loaded since start) and, for every listener,
  `"cpu_s"`, the process's CPU seconds (user + system).

### `GET /raw?t=<t>` (debug)

Waits for the frame at `t` (a future slot, at most 15 s ahead) and returns this
renderer's tile of it unencoded, as a binary PPM at the height being drawn,
with a header `X-Fleet-Tile: {"t":..,"x0":..,"y0":..,"width":..,"height":..,
"full_width":..,"full_height":..,"spp":..,"layout_generation":..}` (and
`hq`, `budget`, `budget_source`); the tile is the one drawn for that frame,
under `previous_rect` before `rect_from_frame`; `404` if the renderer skipped that
slot. `tools/rawmosaic.py` uses it to stitch an unencoded mosaic that can be
compared pixel for pixel with the same frame drawn whole
(`fleet-renderer still --seq 12`). Not for browsers.

### Process

- `fleet-renderer` (or `serve`) listens at once and initializes the GPU in
  the background; `/health` answers with `"ready": false` until it is ready.
- With no subscriber it draws twice a second at a small size, so `/health`
  stays current; a new subscriber wakes it at once.
- Kernels are SASS for `sm_75` only (no PTX), so CUDA compiles nothing at
  start. Other GPU generations need a build with their `GENCODE`.
- Environment: `PORT` (8080), `FPS` (30), `BUDGET` (250000), `ADAPT` (1), `MARGIN` (16), `ENC_BPP` (0.8),
  `ACCUMULATE` (1), `TLS_CERT_FILE` (unset: no TLS listener), `TLS_PORT` (443). `FIT_SCALE`
  (1) scales the reported `budget_fit`, to test a grid of unequal GPUs; never set it in production.
