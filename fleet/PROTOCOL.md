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
| `word` | `{"text": "hello"}` | 1 to 28 characters (the renderer's limit) after collapsing whitespace, no control characters, one per client per 5 s; checked together with every word in the sky first (below) |
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
  "scene": {"epoch_unix_ms": ..., "camera": [...], "time_of_day": 0.3, "words": [{"text", "t"}], "lights": [{"pos", "color", "t"}]},
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
  (null for a new tile, or one that did not move). The page reads `layout.generation`,
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
  first frame as the coordinator saw it, less EC2 capacity waits). `ec2_capacity_wait_ms`
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
  auto scaling groups (`--scale-group`; without any it stays `awake`). After
  `--idle-scale-down` seconds (default 600) with no active viewer (`viewers.active`), and
  never while a claim is active or a race is running, the fleet goes `asleep`
  (`asleep_since`): every group is set to desired capacity 0, the EC2 half has no tiles
  (their instances are terminated), no race is scheduled (`next_race_at` null) and
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
                 [--state-bucket <bucket> [--state-key state.json] [--state-region <region>]]
                 [--ec2 aws --ec2-tiles 8 --ec2-regions <r1,r2,...> [--ec2-template fcvm-fleet-tile-x86_64]
                  [--ec2-on-demand-template <name>]] [--dns-zone fcvm.dev]
                 [--dns-token-secret <secret> [--dns-zone-id <id>]] [--tls-secret <secret>] [--secrets-region <region>]
```

- `--state-bucket` (also `FLEET_STATE_BUCKET`, `FLEET_STATE_KEY`) keeps the state as one S3
  object instead of a file: read at start, rewritten whole (one PUT, atomic; the bucket
  keeps versions) at most once a second while it changes and at shutdown. A missing object
  is no state; a refused read (403, which S3 also answers for a missing object to a caller
  without `s3:ListBucket`) stops the coordinator rather than start it empty.
- `--ec2 aws` runs `--ec2-tiles` EC2 tiles, whatever the fcvm side has. Each is one
  instance from the tile launch template (one-time spot), tagged `Purpose=fcvm-fleet-tile`
  and `FleetTile=<tile>` at launch, in the first of `--ec2-regions` (`FLEET_REGIONS`) with
  capacity; when every region refuses for capacity it tries `--ec2-on-demand-template`,
  and without one the tile waits for capacity (recorded on its own) and asks again every
  10 s. A tile is ready once its instance runs and its renderer answers
  `https://ec2-<n>.<zone>/health` (the name resolved to the instance); it is terminated by
  id. A relaunch launches the replacement first and terminates the old instance once the
  new launch is accepted.
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
connection to `host`; a hello naming a host another live connection holds is refused (the
coordinator closes the new connection), and a later hello on the same connection must name
the same host.

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
| `ack_ms` | fcvm's own restore clock (the record's `ack_ms`): restore call to the guest's ACK |
| `gpu_ms` | the guest's GPU attach (the record's `guest_phases.gpu_ms`) |
| `container_ms` | the `restored` record | the renderer's `GET /health` first answers |
| `first_frame_ms` | `/health` first answers (the scene is posted then) | `/health` reports a frame drawn after the scene was posted: `ready` is not false and `frame` is above its value in the reply read just before the post (for a renderer without `frame`, a numeric `t`) |
| `renderer_gpu_ready_ms` | the renderer process starts | CUDA is ready (`/health` `startup_ms.gpu_ready`) |
| `renderer_first_frame_ms` | the renderer process starts | its first frame is drawn (`startup_ms.first_frame_drawn`) |

A `/health` reply carrying `error` (the renderer failed) fails the relaunch at once.
| `total_ms` | the agent starts the relaunch | the first frame |

```json
{"type": "health", "id": "slot-3", "fps": 29.8, "gpu": { ... }, "budget_fit": 1210000, "t": 861.9}
```

Once a second per running slot: the renderer's `/health` `fps`, its GPU readings,
`budget_fit` and `t` (the scene time of its newest frame), passed through. The coordinator
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
 "words": [{"text": "hello", "t": 12.5}], "lights": [{"pos": [1, 2, 3], "color": [1, 0.5, 0], "t": 14.0}]}
```

The scene shared by every renderer, sent after the hello and whenever it changes. The
agent posts `scene` plus the slot's `rect` to each running renderer as
`POST http://<publish ip>:<port>/scene`, with the body
`{"epoch_unix_ms", "rect", "layout_generation", "rect_from_frame", "previous_rect",
"image_aspect", "camera", "time_of_day", "words", "lights"}`: the renderer draws
`previous_rect` (when present) for frames before `rect_from_frame` and `rect` from it. The
coordinator posts the same body to each EC2 tile's renderer
(`POST https://ec2-<n>.<zone>/scene`).

```json
{"type": "budget", "budget": 707107, "t": 863.2}
```

The grid budget, sent after the hello and whenever it changes. The agent posts
`{"budget", "t"}` to every running renderer that draws a wall tile
(`POST http://<publish ip>:<port>/budget`): when it arrives, and again after every scene
post, because a new scene clears the renderer's budget schedule. A renderer drawing the
whole image for a claim gets no budget (it chooses its own). The coordinator posts the same
body to each EC2 tile's renderer.

### Grid budget

One budget for every tile of the wall, so tiles draw the same samples per pixel at the same
frame and moving frames match across tile edges. Tiles never adapt on their own. The
coordinator runs the rule of the renderer's reference controller
(`fleet/renderer/tools/budgetctl.py`) in the pure core (`fleet_core::budget`), on its
1 s tick:

- Every 2 s, a round over every placed tile of both sides that is not lent to a claim. No
  decision until every one of them has reported a `budget_fit` (a tile that just joined
  holds the round until its first report).
- The smallest fit governs, rounded down to the ladder 250,000 x 2^(n/4), n from -8 to 40.
- A lower rung is taken at once. A higher one only after two rounds in a row ask for at
  least that much, and then the lower of the two asks.
- A change takes effect at one scene time for every tile: `t` = the newest frame any tile
  has drawn plus 1 s, rounded up to a frame (k/30). The agents and the EC2 poster post the
  same `{budget, t}` everywhere.

The budget does not ride on layout generations. Every tile covers the same share of the
image (each side is as wide as its share of the tiles), so one budget gives every tile the
same samples per pixel in any layout, and a re-tile, which every renderer applies at the
layout's frame, changes them for all tiles together. A fit measures the GPU's
pixel-samples per frame, which a tile's size changes little; when it does, the next round
answers it. A tile that joins draws the budget in force (its `t` is in the past, so it
applies from its first frame) until the round that counts it.

### Running it

```
fcvm fleet agent (--stdio | --dial <wss url> [--access-secret <secret>]) --image <renderer image> --base-port <port>
                 [--host <name>] [--publish-ip <ip>] [--frame-host <name or ip>]
                 [--gpus <sbdf>=<name>,...] [--fcvm <path>] [--fcvm-arg <arg>]...
                 [--spot-notice-url <url>] [--aggregate-port <port> [--aggregate-arg <arg>]...]
                 [--scrub] [--store-ready-file <path>] [--host-from-imds] [--secret-region <region>]
                 [--tls-secret <secret> [--tls-dir /run/fcvm-fleet/tls]]
```

`--host-from-imds` names the host by its EC2 instance id. `--tls-secret` hands the fleet's
wildcard certificate to every renderer: the agent reads the secret (the format below,
"Certificate") with the instance role, writes `fullchain.pem` and `key.pem` (mode 0600)
into `--tls-dir`, maps that directory into each VM at `/run/fleet-tls` with
`TLS_CERT_FILE` and `TLS_KEY_FILE` set, and reads the secret again every hour. The host
AMI's unit (`fcvm-fleet-agent.service`, `ExecStart=/usr/local/bin/fcvm fleet agent
$FLEET_AGENT_ARGS`) takes, in `/etc/fcvm-fleet/agent.env`:

```
FLEET_AGENT_ARGS=--dial wss://fleet-api-stage.fcvm.dev/agent --access-secret fcvm-fleet/agent-access-token --host-from-imds --image localhost/fcvm-renderer:fleet --base-port 20100 --publish-ip 0.0.0.0 --tls-secret fcvm-fleet/tls-wildcard --store-ready-file /var/lib/fcvm-fleet/store-ready
```

Each slot's VM is `fcvm podman run --name fleet-<slot> --kernel-profile gpu --gpu <sbdf>
--publish <publish ip>:<base port + gpu index>:8080 [--fcvm-arg ...] <image>`, run
rootless; the pre-start snapshot cache makes every launch after the first a restore. The
frame URL is `ws://<frame host>:<port>/frames` (one function, `frame_url`, to be replaced
by the slot's routed IPv6 name and TLS once fcvm has `--ipv6-address`). `--gpus` replaces
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
  layout change sends a new header and a keyframe.
- The region's size is its share of a whole image `--image-height` tall (default 1080):
  the fcvm half of a 1920 x 1080 image is 960 x 1080. `?h=` is accepted and ignored.
- `POST /layout`: `{"epoch_unix_ms","image_aspect","region":{x,y,w,h},
  "tiles":[{"id","rect","url"}]}`; `GET /layout` returns it.
- `GET /stats`: frames out, the output bitrate over the last 10 s, and percentiles of
  `after_last_tile_ms` (last tile frame used to the region frame out of the encoder),
  `after_first_tile_ms`, `encode_ms`, `tile_lateness_ms` and `out_lateness_ms` (wall
  clock minus the slot's scene time), and per tile whether it is streaming.
