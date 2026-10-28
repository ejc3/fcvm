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
  width, `y` and `h` of its height, origin top left. The fcvm side is on the left and the
  EC2 side on the right, each as wide as its share of the configured tiles (`layout`).
  `image_aspect` is the whole image's width / height (default 16/9, the renderer's; with
  as many tiles configured on each side, each side is 8:9).
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
{"type": "admission", "seat": "<ticket>", "locked": false, "reason": null,
 "direct": {"value": "v1.1761135000.v-17.<mac>", "expires_at": 1761135000000},
 "broadcast": {"value": "https://<playback host>/<channel>.m3u8?token=<jwt>", "expires_at": 1761138000000},
 "seat_free": false, "capacity": 500}
```

Second on every connection, then again every 5 minutes with fresh tokens, and to a late
viewer whenever `seat_free` changes (docs/fleet/ARCHITECTURE.md, "Broadcast mode"). A
connection admitted while fewer than `capacity` seats are held gets a `seat` ticket and a
`direct` token, which every frame URL it opens carries as `admit=<value>`; one admitted
with every seat held is late: `locked`, `seat` and `direct` null, `reason` the sentence the
page shows, and `seat_free` true once a new connection would be seated (the page offers a
reload; a late connection is never promoted). A page that reconnects passes its ticket as
`GET /live?seat=<ticket>` and keeps its seat if the ticket's connection closed less than 60 s
before. `broadcast` is the playback URL (signed for a private channel, valid for an hour),
null without a broadcast; `capacity` is null when every viewer is seated (no broadcast).
`claim_gpu` from a late connection is refused unrecorded (`version: 0`).

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
 "camera": [{"t": 812.0, "pos": [...], "look": [...], "fov_deg": 60, "intent": "<id>"}],
 "autopilot": true, "ended": null}}
```

Sent only to the connection whose `claim_gpu` committed, whenever the claim changes:
first with `phase: "restoring"` (no frame URL yet), then `running` once the private VM
has drawn its first frame (its `frame_url`, `first_frame_at - clicked_at` the click to
first frame, `phases` the agent's timings of the restore), again whenever its camera
keyframes change (`claim_camera`, `claim_autopilot`, and the autopilot's next keyframes
about every 8 s while it flies), and a last time with `phase: "ended"` and `ended` saying why
(`expired`, or `released`: `release_gpu`, the viewer leaving, or the GPU leaving the
fleet). The frame URL and the token appear in no snapshot. `tile` is the wall tile it
borrowed, null for a standby GPU. `autopilot` is true while the camera flies the
autopilot's loop (below) and false once the viewer has paused it or taken the camera.

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
| `nudge` | `{"target": "<viewpoint>"}` | a vote in the open vote (below), among `nudges.targets`; one vote per client, the latest counts; a pick of the hold just decided (`nudges.next`) is accepted and changes nothing; refused only for a target not offered |
| `word` | `{"text": "hello", "text_style": "skywriting"}` | 1 to 28 characters (the renderer's limit) after collapsing whitespace, no control characters, one per client per 5 s; checked together with every word in the sky first (below). `text_style` is how the renderer draws it (#1178): `lockup` (when absent or null), `skywriting` or `landmark`; anything else is refused before the text check |
| `light` | `{"pos": [x, y, z], "color": [r, g, b]}` | one per client per second; each client keeps its latest 3 (its oldest goes); `color` components are clamped to [0, 1], then scaled so they add up to at most 2; the scene keeps the latest 32 |
| `claim_gpu` | `{}` | a GPU of your own for 60 s (below) |
| `release_gpu` | `{"claim": "claim-3"}` (or `{}`) | ends your claim early; a `claim` that is not yours is refused |
| `claim_camera` | `{"drag": {"yaw_deg", "pitch_deg", "dive"}}` | turns and moves your claimed GPU's camera (only yours; refused without a claim), easing over 0.3 s; the keyframe it eases to gets `"intent": <id>`. Yaw within 360, pitch within 180, dive within 50. On a camera flying its autopilot it also stops the autopilot: the camera eases to rest along its path over 0.5 s while it turns by the drag |
| `claim_autopilot` | `{"on": false}` or `{"on": true}` | stops your claimed GPU's autopilot (the camera comes to rest along its path over 1 s, on a keyframe with `"intent": <id>`, and stays there) or starts it again (the camera flies from where it is to the nearest point of the loop in 2 to 8 s, that keyframe carrying the intent, and on around it). Refused without a claim; the state it already has commits and changes nothing |

There is no intent to move the wall's camera, set its time of day, relaunch every tile
on a viewer's schedule, or change the race cadence: the director and the owner's
configuration own those. Two names differ from what the page (#1143) assumed:

- The race button's intent is `restart_now`, the owner's name for it in the second
  design review, not `relaunch_all`. The race it starts is still reported as the
  snapshot's `relaunch_all`.
- There is no `time_of_day` intent. The directed camera's day cycle sets the time of day
  (below), so a viewer's setting would be undone within a flight.
- There is no weather intent either. The director sets the scene's `weather` keys per
  stop (`weather_of_step` in `fleet/core/src/director.rs`): cover from a six-stop cycle,
  towering cumulus every fifth stop in daylight, crepuscular rays when the sun is low, and
  a snow line between 0.7 and 2.3 km over 36 stops. The current stop's weather holds from
  its arrival; once the next stop is decided its weather is published at its arrival, and
  the renderer eases into it over the flight.

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

**The directed camera.** The coordinator flies the wall's camera along a program generated
from `--camera-seed`, and the camera never stands still. At each stop (a named viewpoint:
the main peak and the clock in the sky framed across the fcvm/EC2 seam at image centre,
valleys and ridgelines, the lakes, the plains, above and up through the cloud layer, high
above the horizon's curve) it makes a slow move for the stop's stay of 9 to 12 s (an orbit,
a push-in, a crane, a pan or a tracking drift), then flies to the next stop in one of four
styles: a banking glide, an arc over the cloud deck, a dive through the cloud layer into a
stop below it, or a skim low over the ground. A leg over 45 km low down always goes over
the deck. Each leg starts with the velocity the previous one ended with and keeps 250 m
above the ground and water; keyframes are one second apart and no two in a row are equal,
so the renderer's spline ("Camera interpolation and holds" below) never holds. The time of
day follows a cycle of 18 stops (late afternoon, golden hour, sunset, dusk, dawn, then the
day, about every 8 minutes): each stop has its own, and the time moves on to the next
stop's only while the camera flies, a step per coordinator tick. No stop is at night; the
night passes in the middle of the one flight from the last dusk stop to the first dawn
stop. The offered stops are never among the last six visited; they are the stops visited
longest ago, shuffled a little by a draw from the seed, the step and the stop being left,
with any stop that would repeat a run of four stops among the last 48 put last. With no
votes the next stop is the first offered. The same seed gives the same program, and it does
not repeat in a short loop.

Viewers pick the stops by vote, and the vote is always open: it is for the first stop not
yet decided. Two seconds before a stop's stay ends the open vote closes and decides the
next stop: the offered viewpoint with the most `nudge` votes, of tied ones the one picked
first (a client that picks the same viewpoint again keeps its first time), and with no
votes the default (the first offered). The winning nudge's id is stamped on that stop's
keyframes (`camera[].intent`). The decision is when the camera commits: the flight and the
next stop's keyframes are appended then and are never changed afterwards (the path is only
trimmed from the front, keeping 4 s before the current stop), so every renderer has the
flight before it starts and no renderer sees the path move under it. In the same step the
vote for the stop after it opens, so a pick made while the camera finishes a stop or flies
counts for the stop after the one it is heading to; votes are not cleared when the camera
arrives. A viewer's pick therefore always counts, and the camera reaches it at the target's
`arrive_at` when it wins. Ties going to the earlier pick mean one viewer cannot overturn
another's pick within its vote; two viewers who agree outvote one.

**Viewer text** (`word`). Before the event is applied the coordinator asks Claude Haiku on
Amazon Bedrock whether the text may be shown publicly, read together with every word now
in the sky (so words cannot combine into something unacceptable). The model id and region
come from the coordinator's configuration (`--text-check-model`, `--text-check-region`).
Only an explicit allow commits the word; a refusal, an error, a timeout (5 s) or a
coordinator started without a text check (`--text-check none`) refuses it. An error's
reason, in the receipt, its `check` and `actions`, is one of two fixed sentences, "words are
turned off right now" (`--text-check none`) or "couldn't check that word, try again"
(anything else); what went wrong is in the coordinator's log. A word checked
against a sky that changed meanwhile is refused (send it again). A check that cannot run
marks the coordinator degraded (`failures`) until one runs again. The scene keeps the
latest 16 words. A refused word's text appears nowhere: not in the scene, not in
`actions`.

**System words.** When no viewer's word has been in the sky for a while, the coordinator
writes a short line of GPU history into the scene itself (`--history-idle`, default 60 s;
0 turns it off). The lines are in `fleet/core/data/gpu_history.txt` (about 40, each at most
28 characters, each checked against a published source). The schedule is part of the
state, not of the coordinator's clock: a `Tick` places the next line once a viewer is on
an awake wall and `--history-idle` seconds have passed since the last word left the sky
(a lockup or landmark word holds for 90 s, skywriting for about 45 s), or since the
viewer arrived at an empty wall. The lines come in a seeded shuffle (`--camera-seed`) that
shows every line before any repeats, and the styles rotate lockup, skywriting, landmark.
A system word goes into `scene.words` like a viewer's, with `"system": true`
(absent when false; the renderer ignores it). It skips the text check, creates no receipt
and no `actions` entry, and is not part of the sky a viewer's word is checked against. A
viewer's word is newer, so the renderer draws it instead at once, and the next system word
waits for that word's hold and the gap. At most one system word is in the scene, and it
is not counted among the latest 16 words nor does it push one out.

**Your own GPU** (`claim_gpu`). The claim takes a standby GPU when there is one. Normally
every GPU draws the wall, so the claim borrows one: the last tile in slot order that is
running its current generation (not relaunching, not still racing). The tile keeps its
place in the layout, its `phase` becomes `borrowed` and its frame URL is withheld; its
host's aggregator draws it black. The agent tears the tile's VM down and restores a
private VM on that GPU drawing the whole image with the session's own camera, which only
its viewer can move (`claim_camera`, `claim_autopilot`). The camera waits at the start of
the autopilot's loop and flies it from 0.5 s after the first frame: a 56 s loop
(`fleet/core/src/autopilot.rs`) low over the lake west of the main peak, up the ridge
face, along the peak's flank, up through the cloud layer to 7 km, over the top, down
through the clouds, low over the water and hills to the south, and back onto the lake. Its
keyframes come a second apart and reach at least 8 s ahead (16 s after each batch). It
flies until the viewer pauses it or drags, scrolls or presses a camera key, and does not
start again on its own. The coordinator times the click to that VM's first frame and
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
      "frame_url": "wss://slot-1-k3x7q2ab.<zone>/frames",
      "frame_url_v4": "wss://slot-1-k3x7q2ab-v4.<zone>:20200/frames",
      "gpu": {"sbdf": "0000:01:00.0", "name": "Tesla T4"},
      "host": "<host name>",
      "place": "EC2 in us-east-1",
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
    {"id": "fcvm:8", "phase": "waiting_capacity", "slot": null, "frame_url": null, "host": null, ...},
    {"id": "ec2:1", "half": "ec2", "rect": {"x": 0.5, "y": 0, "w": 0.25, "h": 0.5}, "phase": "waiting_capacity", "slot": "ec2-1", "frame_url": null, ...}
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
  "scene": {"epoch_unix_ms": ..., "camera": [...], "time_of_day": 0.3, "words": [{"text", "t", "text_style", "system"}], "lights": [{"pos", "color", "t"}]},
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
  "nudges": {"targets": [{"id": "coast", "label": "From the coast", "votes": 2, "arrive_at": 1761134422000},
                         {"id": "horizon", "label": "High above: the curve of the horizon", "votes": 0, "arrive_at": 1761134431000},
                         {"id": "dawn-valley", "label": "Dawn in the valley", "votes": 1, "arrive_at": 1761134420000}],
             "holding": "peak-seam", "holding_label": "The peak across the seam", "next_hold_at": 1761134407000, "next": null, "next_label": null,
             "arrive_next_at": null, "leading": "coast", "opened_at": 1761134383000,
             "closes_at": 1761134405000},
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

- `tiles` lists one tile per configured place, the fcvm side then the EC2 side: `fleet.n`
  fcvm tiles and `fleet.ec2_n` EC2 tiles. The fcvm side has `--fcvm-tiles` places, or one
  per live fcvm GPU (GPUs on hosts that are not draining, at most `max_tiles`) when there
  are more of those; the EC2 side has `--ec2-tiles` places (at most `max_tiles`), one per
  EC2 tile, with an instance or not. Asleep, both sides have none. Each side is as wide as
  its share of the places (`layout.fcvm`, `layout.ec2`: 8 and 8 give each side half the
  width, fcvm 100 and EC2 12 give the fcvm side 100/112), so the halves do not change when
  GPUs go dark: a race, an EC2 tile waiting for capacity, a host not yet connected or
  reclaimed. A side with no places has no rectangle (null) and the other takes the whole
  width; with none on either side there are no tiles. A place with no live renderer is a
  dark tile: an fcvm place with no GPU has `phase` `waiting_capacity` and a null `slot`,
  `host` and `frame_url`; an EC2 tile without an instance keeps its `slot` and reports its
  own phase (`fleet.ec2_waiting` counts those, and the race's `relaunch_all` shows a
  capacity wait). The fcvm side is cut into full-width bands, one per GPU host (hosts in
  the order of their lowest slot number) and, below them, one for the places with no GPU,
  each as tall as its share of the side's tiles and cut into its own grid, row-major, so a
  host's tiles cover one rectangle, which its aggregator encodes whole; the EC2 side is
  one grid.
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
  `budget_from_t` the scene time it took effect at (both null until every placed tile with a
  renderer has reported a fit; a dark place and an EC2 tile without an instance have none); see "Grid budget" below. The page may show it; it needs nothing from
  it.
- `phase`: `waiting_capacity` (dark: no fcvm GPU in the place, or EC2 refused capacity),
  `borrowed` (lent to a visitor, drawn black), `pending` (assigned, nothing reported yet),
  `relaunching` (a newer generation is wanted than the agent has reported), or what the
  agent reported: `stopping`, `restoring`, `starting`, `running`, `failed`.
- `frame_url` is the renderer's frame WebSocket as the browser should open it (append
  `?h=<height>`), over IPv6; `frame_url_v4` the same renderer over IPv4. Both are null while
  borrowed. With `--dns-subzone` both are the launch's own names (see "Running it"),
  published with the launch's address, and a relaunch gives new ones; without it
  `frame_url` is what the agent reported and `frame_url_v4` is null.
  A viewer uses one family for every tile of both halves. After a relaunch the tile
  reconnects when `phase` is `running` again and its URLs are set.
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
- `nudges` is the directed camera: `targets` are the viewpoints the open vote is among,
  the ones `nudge` may name now (`id`), each with a label, its votes and `arrive_at`, when
  the camera gets there if it wins; `holding` (and `holding_label`) is the stop the camera
  is at now, moving slowly through it (null while flying), and `next_hold_at` when the
  camera leaves it; `next` (and `next_label`) is the next stop once decided,
  `arrive_next_at` when the camera gets there. While `next` is null the open vote decides
  it; otherwise it decides the stop after `next`. `leading` is the target that
  wins if the vote closed now (the default when nobody voted); `opened_at` and
  `closes_at` bound the open vote.
- `fleet.power` is whether the GPU capacity is up, when the coordinator launches the GPU
  hosts (`--host-template`), or with `--sleep-when-idle` on hosts it does not launch (a
  fixed host, a test box); without either it stays `awake`. After
  `--idle-scale-down` seconds (default 600) with no active viewer (`viewers.active`), and
  never while a claim is active or a race is running, the fleet goes `asleep`
  (`asleep_since`): every GPU host is terminated, nothing is placed (every fcvm
  GPU still connected goes to standby, so its VM stops; the EC2 half's instances are
  terminated), no race is scheduled (`next_race_at` null) and
  `restart_now` is refused. Hosts that leave then are not reclaimed. When an active viewer
  arrives it is `waking` from `woke_at`, the moment the wake began: every host region gets
  `--awake-capacity` hosts again (default 1) and the EC2 half launches its `--ec2-tiles`
  tiles. `hosts` lists each GPU host by EC2 instance id with the phases it has reached, in
  order: `instance_pending` and `booted` from EC2 (pending, then running),
  `store_ready` when the host marked its store mounted (the agent's hello reports it; the
  phase is absent when the host does not mark it), `agent_connected` when its agent says
  hello (the agent names itself by its instance id), `tiles_rendering` when all its placed
  slots run. Once every placed tile of both halves runs it is `awake`; `last_wake` gives
  the wake's start, when it was complete (`awake_at`), the tiles rendering then (`gpus`,
  both halves; an EC2 place without an instance is not one) and the time between (`ms`);
  `wakes` keeps the last 20. The EC2 half can make the image complete before a GPU host's agent has said hello (a host's slots are placed
  only then), so the hosts' phases keep being recorded after the fleet is `awake`, until
  every host of the wake has `tiles_rendering`. The coordinator logs each step at info
  (`wake: started`, `wake: host phase`, `wake: host boot` with the hello's boot times,
  `wake: tile running` with each tile's timings, `wake: fleet awake`), each with
  `since_wake_ms`, and each EC2 tile's readiness (`tile ready`: since its launch was
  accepted, when the instance was first seen running, when its renderer first answered
  `/health`, and when it was ready). A starting tile is described every 3 s and, once it
  runs with an address, its renderer's `/health` is asked every 250 ms in between.
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
  event that reports it (the side keeps its places: the other hosts' GPUs are re-tiled
  at once and the places left over are dark), so there is
  no `draining` tile phase and no tile-level `host_reclaimed`; `fleet.hosts[].draining`
  and `failures` carry it.
- `aggregates` lists the regions an aggregator serves (see Aggregator): one per GPU host
  that has placed tiles, with the tiles inside it, and the EC2 half when the coordinator
  runs its aggregator. `frame_url` is null until the host's aggregator is serving. With
  the coordinator's subzone (`--dns-subzone`) a host aggregator's `frame_url` and
  `frame_url_v4` are its published names (`wss://agg-<host>-<token>.<subzone>/frames`,
  `wss://agg-<host>-<token>-v4.<subzone>:<port>/frames`), and both stay null for an
  aggregator that serves no browser (no `--aggregate-tls-port`).
- `actions` holds the latest 50 intents, committed and refused, oldest first: the
  `viewer` that sent it, the intent `id`, its `args` (a refused word's are `{}`, so its
  text appears nowhere), and the outcome.

### `GET /poster.jpg`

The latest converged still of the whole image, for link previews and the page's first
paint. Once per camera stop, 5.5 s into it, the coordinator subscribes to every aggregator
(each host's band and the EC2 half), decodes the first frame at or after that scene time
(every aggregator draws the same frame times, so the regions are one frame),
places each region in the whole image and encodes one JPEG (through the `ffmpeg` CLI).
Served with `Cache-Control: public, max-age=10` and `Access-Control-Allow-Origin: *` (the
page reads it from its own origin); `404` with `no-store` until the first poster exists. A stop whose capture does not finish before the flight keeps the previous
poster, and so does a stop while a race runs or any placed tile is starting or
relaunching (its region would be black; claimed GPUs and dark tiles do not count). Borrowed tiles are black in it, as on the wall.

### Running it

```
fcvm fleet coord --listen <addr:port> [--agent-cmd '<command>']... [--state-file <path>]
                 [--text-check bedrock --text-check-model <id> --text-check-region <region> | --text-check none]
                 [--max-tiles 100] [--fcvm-tiles 8] [--ec2 none|fake] [--allowed-origin <scheme://host[:port]>]...
                 [--ec2-aggregate-listen <addr:port> (--ec2-aggregate-url <ws url> | --ec2-aggregate-tls
                  [--ec2-aggregate-tls-dir /run/fcvm-fleet/tls])] [--aggregate-arg <arg>]...
                 [--race-every <secs>] [--restart-cooldown 60] [--camera-seed 1] [--max-borrowed <n>]
                 [--trust-cf-connecting-ip] [--turnstile-secret <secret> --turnstile-site-key <key>]
                 [--admin-listen <addr:port>] [--ffmpeg ffmpeg]
                 [--host-template <region>:<template>... --host-subnet <region>:<subnet id>...]
                 [--awake-capacity 1] [--host-on-demand-max 8] [--idle-scale-down 600] [--history-idle 60]
                 [--state-bucket <bucket> [--state-key state.json] [--state-region <region>]]
                 [--ec2 aws --ec2-tiles 8 --ec2-regions <r1,r2,...> [--ec2-template fcvm-fleet-tile-x86_64]
                  [--ec2-on-demand-template <name>] [--fleet-name stage]
                  [--ec2-pool <n> --ec2-pool-template <name>]
                  [--ec2-subnets <region>:<subnet>,... --ec2-security-group <region>:<group>]...]
                 [--dns-zone fcvm.dev] [--dns-token-secret <secret> [--dns-zone-id <id>]]
                 [--dns-subzone <subzone> --dns-subzone-ns <ns>,... --dns-listen <addr:port> [--dns-rate-limit 200]]
                 [--tls-secret <secret>] [--secrets-region <region>]
                 [--broadcast-listen <addr:port> (--broadcast-secret <secret> | --broadcast-file <path>)
                  [--broadcast-arg <arg>]... [--direct-capacity 500]]
```

- `--broadcast-listen` runs the broadcast (docs/fleet/ARCHITECTURE.md, "Broadcast mode"): an
  aggregator child (this address is its layout and stats listener) whose layout is the
  whole image, every placed tile read directly at its published IPv6 name. It runs while a
  tile is placed and is stopped while none is (asleep), and pushes to the ingest URL of
  `--broadcast-secret` (a Secrets Manager secret, read in `--secrets-region`) or
  `--broadcast-file`: JSON `{"ingest_url": "rtmps://<endpoint>:443/app/<stream key>",
  "playback_url": "https://<...>.m3u8", "channel_arn": "<arn>", "playback_key_pem": "<PKCS#8
  P-384 private key>"}` (`playback_url` optional; the last two together, for a private
  channel, or neither). The coordinator does not start when it cannot read it. With a
  broadcast, `--direct-capacity` (also `FLEET_DIRECT_CAPACITY`, default 500) seats that many
  viewers; the rest watch the broadcast (`admission` above). Its arguments are `--image-height 720
  --tile-query q=std --maxrate-kbps 4500 --crf 21 --preset veryfast`, then any
  `--broadcast-arg`.

- `--state-bucket` (also `FLEET_STATE_BUCKET`, `FLEET_STATE_KEY`) keeps the state as one S3
  object instead of a file: read at start, rewritten whole (one PUT, atomic; the bucket
  keeps versions) at most once a second while it changes and at shutdown. A missing object
  is no state; a refused read (403, which S3 also answers for a missing object to a caller
  without `s3:ListBucket`) stops the coordinator rather than start it empty.
- `--fcvm-tiles` (default 8, at most `--max-tiles`) is the fcvm side's places: with fewer
  live fcvm GPUs the rest are dark tiles, with more the side has one place per GPU. It and
  the EC2 side's `--ec2-tiles` size the two sides (see `tiles` above).
- `--ec2 aws` runs `--ec2-tiles` EC2 tiles, whatever the fcvm side has. Each is one
  instance from the tile launch template (one-time spot), tagged `Purpose=fcvm-fleet-tile`,
  `FleetTile=<tile>` and `Fleet=<--fleet-name>` (`FLEET_NAME`, default `stage`) at launch,
  in the first of `--ec2-regions` (`FLEET_REGIONS`) with
  capacity. In a region given `--ec2-subnets` (`FLEET_EC2_SUBNETS`, entries separated by
  spaces; one subnet per AZ) a launch tries each subnet in order before the next region,
  naming the interface itself: that subnet, the region's `--ec2-security-group`
  (`FLEET_EC2_SECURITY_GROUPS`), a public IPv4 address and one IPv6 address. A refusal for
  capacity, or `Unsupported` (the AZ does not offer the type), moves to the next subnet. A
  region without subnets launches in the template's. When every region refuses for capacity
  it tries `--ec2-on-demand-template` the same way,
  and without one the tile waits for capacity (recorded on its own) and asks again every
  10 s. A tile is ready once its instance runs and its renderer answers
  `https://ec2-<n>.<zone>/health` (the name resolved to the instance) with `ready` true; a
  reply carrying `error` fails the launch like no answer at all. It is terminated by
  id. A relaunch launches the replacement first and terminates the old instance once the
  new launch is accepted. A tile whose instance never serves `/health` has that instance
  terminated and is launched again after a backoff (1 s doubling to 30 s), and so does a
  running tile whose instance is found shutting down, stopped or terminated (a spot
  reclaim), whose renderer's `/health` carries an `error`, or whose renderer leaves
  `/health` unanswered three checks in a row (checked every 15 s). A renderer that
  restarts on its instance (`fcvm-fleet-renderer.service` restarts it) answers with a new
  `boot_id`: the coordinator reads each tile's `/health` once a second (and once before the
  first scene post on an instance) and posts the scene and the budget again when the
  `boot_id` differs from the last one read. The coordinator
  keeps no instance ids across a restart, so before its first launch, and again when it
  stops, it terminates every instance tagged `Purpose=fcvm-fleet-tile` and
  `Fleet=<--fleet-name>` in `--ec2-regions`; another fleet's tiles in the same account (a
  different `--fleet-name`) are left alone.
- `--ec2-pool <n>` with `--ec2-pool-template <name>` keeps up to `n` tile instances stopped
  for the next wake. A tile that leaves the assignment (the fleet sleeps) has its instance
  tagged `FleetPool=<--fleet-name>` and stopped instead of terminated, while fewer than `n`
  are pooled and its image is the pool template's current one. A tile with no instance (a
  wake, or after its instance failed) first starts a pooled instance in the first region,
  retagged `FleetTile=<tile>`, and launches only when none starts; the tile's timings then
  carry `resumed: 1`. The relaunch of a running tile (a race) always launches. Cold
  launches use the pool template, which must give instances that can be stopped
  (persistent spot that stops on interruption, or on demand). Every terminate cancels the
  instance's spot request first when it is persistent, since a persistent request replaces
  a terminated instance, and leaves the instance running when the request cannot be read
  or cancelled. The start and stop sweeps keep up to `n` pooled instances that are stopped
  or stopping. A pooled instance of an older image, or launched more than 6 days ago (a
  persistent request without `ValidUntil` expires after 7), is terminated at the next wake
  instead of started. Measured on g4dn.xlarge (2026-10-25): a
  stopped tile started to its first frame in 15.5 s (persistent spot) and 17.2 to 17.5 s
  (on demand), against 81 to 84 s for a launch; a stopped persistent spot instance can be
  started again only once its request is `disabled`, 42 to 54 s after it stopped.
- `--dns-token-secret` is the zone's Cloudflare token, which the certificate renewal writes
  its DNS-01 records with (see "Certificate").
- `--dns-subzone` makes the coordinator the authoritative DNS server of a subzone of
  `--dns-zone` (the parent delegates it with NS records naming `--dns-subzone-ns`, and no DS
  record) and names launches there. Every launch of an fcvm slot or EC2 tile that reports
  an address gets a label never used before, `slot-<n>-<token>` / `ec2-<n>-<token>` (8
  random base32 characters), and under it `<label>.<subzone>` AAAA (the slot's own IPv6
  address) and `<label>-v4.<subzone>` A (the host's or the tile's public IPv4 address), TTL
  60. A launch's names answer until its slot has a newer launch or leaves the fleet. A
  host aggregator that reports public addresses is named the same way under
  `agg-<host>-<token>` (a host name of lowercase letters, digits and inner hyphens, at most
  40 characters): AAAA its own IPv6 address, A the host's IPv4 address, with its port in
  `frame_url_v4`; the EC2 half's aggregator is `agg-ec2-<token>` (see
  `--ec2-aggregate-tls`). An aggregator's names answer while it reports the same
  addresses; new addresses get a new label. Minting a label and publishing its names are
  one step of the coordinator's state, and the server answers from that state, so a name
  answers from the moment a viewer can learn it, with no wait. It listens on
  `--dns-listen`, UDP and TCP (a high port; a load balancer maps 53), and answers only for
  the subzone: SOA (negative TTL 5 s), NS, the published names, and
  `_acme-challenge.<subzone>` as a CNAME to `_acme-challenge.<zone>`; NXDOMAIN or NODATA
  with the SOA; REFUSED for any other name; AA set, no recursion, EDNS(0), TC over UDP when
  the client's buffer is too small. Over UDP, past `--dns-rate-limit` responses a second
  per source /24 or /56, every second response goes out truncated and the rest are
  dropped.
- `--tls-secret` keeps the wildcard certificate renewed (see "Certificate").
- `--ec2-aggregate-tls` (with `--ec2-aggregate-listen`, `--tls-secret` and
  `--dns-subzone`) serves the EC2 half's aggregator to browsers with TLS on port 443
  of the coordinator task's own addresses, read at start on ECS: the global IPv6 address
  from the task metadata (`ECS_CONTAINER_METADATA_URI_V4`) and the public IPv4 address of
  the task's network interface (EC2 `DescribeNetworkInterfaces` by its private address, in
  `AWS_REGION`, with the task role). It writes the certificate (from `--tls-secret`) as
  `tls.json` into `--ec2-aggregate-tls-dir` for the aggregator and reads the secret again
  hourly, and names the addresses `agg-ec2-<token>` in the subzone (AAAA and `-v4` A, both
  on 443) like a host aggregator. A non-root process binds 443 only where the task lowers
  `net.ipv4.ip_unprivileged_port_start` (it covers IPv6 too). With published names the
  EC2 aggregate's `frame_url`/`frame_url_v4` are those names, else null;
  `--ec2-aggregate-url` is used only without the subzone.

### Certificate

The wildcard certificate (`*.<zone>`, `<zone>` and, with `--dns-subzone`, `*.<subzone>`)
lives in one Secrets Manager secret,
a JSON object: `cert` (PEM full chain), `key` (PEM private key), `domains`, `not_after` and
`issued_at` (RFC 3339 UTC) and `issuer` (fleet/tools/issue-cert.sh writes the first one).
Twice a day the coordinator reads it; within 30 days of `not_after` (or when it is missing
or does not cover every one of those names) it orders a new certificate from Let's Encrypt
(`--acme-directory` for another CA), answers DNS-01 with TXT records at
`_acme-challenge.<zone>` through the DNS token (the subzone's challenge name is a CNAME to
it), waits until public DNS shows them, stores the new version and removes the records. A
renewal that fails is tried again 15 minutes later. Its key is PKCS#8 (`PRIVATE KEY`), where issue-cert.sh wrote SEC 1;
both are PEM that nginx and rustls read. Renderers get it from their agent
(`--tls-secret`), tiles from their own instance role.

- `--host-template` names a GPU host launch template (`<region>:<template>`, repeatable;
  the templates of a region are one request's pools, e.g. one per architecture), and
  `--host-subnet` the subnets hosts may launch in (`<region>:<subnet id>`, repeatable, at
  least one per region with a template). Both also come comma separated from
  `FLEET_HOST_TEMPLATES` and `FLEET_HOST_SUBNETS`, and `--host-on-demand-max` from
  `FLEET_HOST_ON_DEMAND_MAX`. The coordinator launches the hosts itself, with
  the credentials of its task or instance role: a region with fewer pending or running
  hosts than `--awake-capacity` asks EC2 for the shortfall on spot (`CreateFleet` of type
  `instant` over every template and subnet of the region, `price-capacity-optimized`), and
  when the answer refuses spot for capacity (`InsufficientInstanceCapacity`,
  `MaxSpotInstanceCountExceeded`, `UnfulfillableCapacity`, `VcpuLimitExceeded`, ...) asks
  for the rest on demand in the same pass (`spot launch refused; launching on demand now`
  with the refusal), while the fleet runs fewer than `--host-on-demand-max` on-demand hosts
  (default 8, at most 8) in all regions together. The on-demand hosts are counted only when
  every region could be listed; otherwise no region falls back until it can. A region still
  short asks again 10 s later, spot first. Each host is tagged `Purpose=fcvm-fleet-host`,
  `Fleet=<--fleet-name>` and `Market=spot|on-demand`, and is found by those tags, so a
  restarted coordinator keeps the hosts it finds. Asleep, every host of the fleet is
  terminated; awake, a stopped host and a host over the awake capacity (on-demand first)
  are terminated, and a host that is reclaimed or terminated is replaced. The regions are
  listed (`DescribeInstances`) every 2 s while hosts come up or go away and every 15 s while
  they run; an instance a launch named counts as pending while lists leave it out (60 s at
  most), and a launch whose answer is lost is asked again with the same idempotency token. The coordinator logs `wake: host launch accepted` (`market`, `request_ms`, the
  `CreateFleet` round trip), `wake: host running` (`request_to_running_ms`) and, once a
  region runs its awake capacity, `wake: host capacity in service` with `path` (`spot`,
  `on-demand`, or `spot and on-demand`) and `since_wake_ms`. A call that fails marks the
  coordinator degraded (`scale`) and is retried at the next poll.
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
restart. A slot's report is replayed while the slot is assigned; a quarantine until the
coordinator answers it with `quarantine_stored` (below). A reported generation above the
coordinator's is adopted, so the next relaunch goes past it. The coordinator serves `/agent` on its `/live` listener; Cloudflare Access
admits only the agents' service token there.

When its input closes (`--stdio`), the agent tears down every VM it started and exits. If the agent
is killed outright, each VM's fcvm gets SIGTERM from the kernel (the agent arms
`PR_SET_PDEATHSIG` on every fcvm it starts, and fcvm arms it on every process below).

### Agent to coordinator

```json
{"type": "hello", "host": "<name>", "arch": "x86_64", "gpus": [{"sbdf": "0000:01:00.0", "name": "Tesla T4"}], "capacity": 8,
 "store_ready_at": 1761134470000,
 "boot": {"kernel_started_at": 1761134405000, "store_started_at": 1761134417000, "store_mounted_at": 1761134418000, "agent_started_at": 1761134471000}}
```

First line, once. `store_ready_at` (optional) is when the host's store was ready: the
modification time of `--store-ready-file`, which the host's store unit
(`fleet/ami/host/store.sh`) writes once the store is restored. `boot` (optional) is when the
host's boot reached each step, Unix milliseconds on the host's clock: `kernel_started_at`
(now less `/proc/uptime`), `agent_started_at`, and every `*_at` number in the store-ready
file, which the store unit writes as a JSON object (`store_started_at`, `store_mounted_at`).
The coordinator keeps the times that fall inside the current wake in the wake's host record
(`power.hosts` in the state object) and logs them. `gpus` are the NVIDIA display functions bound to `vfio-pci`, in address
order; `capacity` is how many VMs the host runs at once (default: one per GPU). The
coordinator makes one slot per GPU and ignores a repeated address. The first hello binds the
connection to `host`, and a later hello on the same connection must name the same host. A
hello naming a host another connection holds replaces that connection: a dialing agent that
lost its network reconnects while the coordinator may still hold the old, half-open one. The
host's slots stay, and the old connection's later messages are ignored.

```json
{"type": "slot", "id": "slot-3", "phase": "running", "generation": 4, "vm": "fleet-slot-3",
 "frame_url": "ws://<host>:<port>/frames", "ipv6": "2001:db8::3", "ipv4": "192.0.2.10", "v4_port": 20202,
 "place": "EC2 in us-east-1",
 "timings": {"teardown_ms": 310, "restore_ms": 4300, "ack_ms": 4300, "gpu_ms": 3100, "container_ms": 600, "first_frame_ms": 250, "total_ms": 5200},
 "error": null, "retry_in_ms": null}
```

On every phase change. `place` is where the host runs, `EC2 in <region>` (`protocol::place_label`;
absent off EC2): the browser view shows it as the tile's `place` instead of the host name. A slot
that first reports `running` with no `last_relaunch` gets one (`by` is `wake`) timed from
`timings.total_ms`, so a tile that came up at a wake has a last launch. `generation` is the generation the agent is working toward or
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
{"type": "quarantine", "slot": "slot-5", "sbdf": "0000:05:00.0", "error": "...", "report": "<32 hex>"}
```

The agent took a GPU out of rotation and runs nothing on it again: its scrub failed, a
relaunch failed with an error naming the GPU's address, or three relaunches in a row
failed before fcvm's `restored` record (the GPU reset and attach) or while the GPU was
still held. Failures after the restore (the container or the renderer) do not count.
`report` names this report (random per quarantine). The agent sends it again at the start
of every connection until the coordinator answers with `quarantine_stored` for that report.
A replay of a report whose quarantine an operator released (the coordinator keeps the last
released report per GPU in its state) is acknowledged and changes nothing; a new report for
the same GPU quarantines it again.

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
- the renderer's `/health` names a different `boot_id` than the last read (the renderer
  restarted inside its VM and lost its scene): post the scene and the budget again, no
  relaunch.
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
"image_aspect", "camera", "time_of_day", "words", "lights", "weather"}` (`weather` only when
the scene has weather keys: renderer `POST /scene`, "Weather"). The
coordinator posts the same body to each EC2 tile's renderer
(`POST https://ec2-<n>.<zone>/scene`). A tile is reported `running` only after its
renderer has taken the scene: a viewer streams a running tile at once, and a renderer
with no scene draws against epoch 0.

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

```json
{"type": "quarantine_stored", "sbdf": "0000:05:00.0", "report": "<32 hex>"}
```

A stored state holds the agent's `quarantine` report `report` for this GPU: the coordinator's
state write (`--state-file` or `--state-bucket`) that came after the report succeeded.
Without it a coordinator that stopped before its next write would come back with no record
of the quarantine and assign the GPU again, so the agent keeps the report in its replay
until this arrives for that report (a newer report for the same GPU stays). The
coordinator sends it on the store tick (1 s) after the write, to the
host's connection at the time; a host that reconnected first replays the report and is
answered on the new connection. With no store configured it is sent on the next tick. A
release by the operator before the write still gets it: the stored state then holds the
release, and the report must no longer be replayed.

### Grid budget

One budget for every tile of the wall, so tiles draw the same samples per pixel, and take
the same path (fast, or high quality from `HQ_SPP` = 8 samples per drawn pixel), at the same
frame, and moving frames match across tile edges. Tiles never adapt on their own. The
coordinator runs the rule of the renderer's reference controller
(`fleet/renderer/tools/budgetctl.py` at 55e81ad5 on `fleet-renderer-adapt`) in the pure core
(`fleet_core::budget`), on its 1 s tick:

- Every 2 s, a round over every placed tile of both sides that is not lent to a claim (an
  EC2 tile without an instance has no renderer and is left out). No decision until every
  one of them has reported a `budget_fit` (a tile that just joined holds the round until
  its first report; a held round changes nothing, so the count of rounds below carries
  over it). A renderer that is not `ready` reports no fit.
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
                 [--spot-notice-url <url>] [--aggregate-port <port> [--aggregate-arg <arg>]...
                  [--aggregate-tls-port <port>]]
                 [--scrub] [--store-ready-file <path>] [--host-from-imds] [--secret-region <region>]
                 [--tls-secret <secret> [--tls-dir /run/fcvm-fleet/tls]]
                 [--ipv6-prefix <prefix/len> | --ipv6-prefix-from-imds] [--frame-zone <zone>]
                 [--tls-publish-base <port> [--public-ipv4-from-imds]]
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
relaunches and reports it as `ipv6` in every slot message; the coordinator names it in DNS
(see the coordinator's `--dns-subzone`). The renderer's container shares the VM's
network, so a browser reaches its TLS port at that address. `--frame-zone` reports the frame
URL as `wss://<slot id>.<zone>/frames`.

The IPv4 path: a GPU host has one public IPv4 address, shared by its slots.
`--tls-publish-base <port>` forwards each slot's TLS port on the host (`--publish-ip`, port
`<port> + GPU index`, fcvm's `--publish`, which relays into a routed VM and goes away with its
fcvm), and `--public-ipv4-from-imds` reports the host's address (IMDS `public-ipv4`, assigned
by EC2 at launch) as `ipv4`, with the slot's port as `v4_port`. The host
AMI's unit (`fcvm-fleet-agent.service`, `ExecStart=/usr/local/bin/fcvm fleet agent
$FLEET_AGENT_ARGS`) takes, in `/etc/fcvm-fleet/agent.env`:

```
FLEET_AGENT_ARGS=--dial wss://fleet-api-stage.fcvm.dev/agent --access-secret fcvm-fleet/agent-access-token --host-from-imds --image localhost/fcvm-renderer:fleet --base-port 20100 --publish-ip 0.0.0.0 --tls-secret fcvm-fleet/tls-wildcard --ipv6-prefix-from-imds --frame-zone fcvm.dev --tls-publish-base 20200 --public-ipv4-from-imds --store-ready-file /var/lib/fcvm-fleet/store-ready
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

`--aggregate-tls-port <port>` (with `--tls-secret`) also serves the stream to browsers with
TLS (the aggregator's `--tls-listen`, the renderers' certificate file): on `<publish ip>:<port>`,
and with an IPv6 prefix on port 443 of `<prefix>::1`, the host's side of the prefix (fcvm
never gives it to a VM), which the agent puts on the host's default IPv6 interface
(`ip -6 addr replace <addr>/128 dev <iface> nodad`) before it starts. The message then
carries what the coordinator names (`agg-<host>-<token>`, see `--dns-subzone`):
`ipv6` (port 443), and with `--public-ipv4-from-imds` the host's `ipv4` and the port as
`v4_port`. The aggregator's `--listen` stays on `<publish ip>:<aggregate port>`, which the
host's security group does not admit.

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
  of the same generation (a tile's URL changed) replaces the old one at once. If the new
  layout's pipeline stops before it takes over (its encoder failed), the old layout keeps
  composing past `from_frame`; a publishing pipeline that stops is started again after 1 s,
  doubling to 30 s. A viewer that falls behind the stream starts again from the newest
  keyframe.
- The region's size is its share of a whole image `--image-height` tall (default 1080):
  the fcvm half of a 1920 x 1080 image is 960 x 1080. `?h=` is accepted and ignored.
- `POST /layout`: `{"epoch_unix_ms","image_aspect","region":{x,y,w,h},
  "tiles":[{"id","rect","url"}],"generation","from_frame"}` (`generation` and `from_frame`
  default to 0: the layout generation of the tiles' rects and its frame; the agent sends
  its placed slots' newest, the coordinator the EC2 side's); `GET /layout` returns it.
  The listener also serves viewers, so a `POST /layout` needs
  `Authorization: Bearer <token>` when the aggregator was started with
  `FCVM_FLEET_AGGREGATE_TOKEN` in its environment (the agent and the coordinator make one
  for the aggregator they run), and otherwise comes only from this host (a loopback
  address); anything else is refused with 403.
- `GET /stats`: frames out, the output bitrate over the last 10 s, and percentiles of
  `after_last_tile_ms` (last tile frame used to the region frame out of the encoder),
  `after_first_tile_ms`, `encode_ms`, `tile_lateness_ms` and `out_lateness_ms` (wall
  clock minus the slot's scene time), and per tile whether it is streaming.
- `--tls-listen <addr:port>` (repeatable, with `--tls-file <path>`) also serves `GET
  /frames` and `GET /healthz` with TLS on that address, for browsers on an https page.
  `/layout` and `/stats` are served on `--listen` only, which the agent and the
  coordinator keep on an address of their own. `--tls-file` is the file the agent writes
  for its renderers (`tls.json`, `{"cert", "key"}`, PEM; see the agent's `--tls-secret`):
  it is read again every 2 s and a new version is used for the connections that follow;
  a version that does not load is logged and the previous one kept.

### Pushing to a live ingest (the broadcast)

`--push-url-env <VAR>` also pushes the stream to the URL in that environment variable
(`rtmps://<endpoint>:443/app/<stream key>`; the variable keeps the key off the aggregator's
command line). The aggregator writes FLV to an `ffmpeg` child that copies it there: an
`onMetaData` tag naming the size first (so the copy probes nothing), the video, and a silent
AAC-LC track (48 kHz mono, a frame every 1024 samples up to each video frame's time; IVS
serves a stream without audio about 3.4 s later). A child that exits is started again after
1 s, doubling to 30 s. The push starts at a keyframe, and after frames are lost (the pusher fell
behind) waits for the next one. Each frame's FLV timestamp is its scene time in milliseconds
after the first pushed frame, so a skipped slot is a gap. Each frame's access unit carries,
before its first slice, an SEI message of type 5 (user data unregistered) with UUID
`6a0e3c5d-9b41-4f2e-8d77-1c52f30ab619` and the JSON payload

```json
{"t": 1234.533333, "e": 1761134400000}
```

`t` the frame's scene time (seconds) and `e` the scene epoch (unix milliseconds), so the
frame's wall time is `e + 1000 t`. A player reads it as the frame is shown (the IVS web
player's `MetadataEventType.SEI`).

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
| `weather` | weather keys: `{"t", "cover", "towers", "rays", "snow_line_km"}` (see "Weather") | optional; at most 64; absent, null or empty means the defaults throughout |
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
null. If a generation was in force, every open stream gets a new header (and a
keyframe) with `layout_generation` null at the next frame, even when the geometry
is unchanged. With it
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
  apart. The coordinator's directed camera includes none: its path never stands still,
  and its moving frames rely on the reprojected history (below).

#### Words, the clock and lights

A word (an entry of `words`):

| field | meaning | accepted |
|-------|---------|----------|
| `text` | the message | a string; printable ASCII is kept (other bytes below 0x80 become `?`, bytes from 0x80 are dropped), at most 28 characters, case kept |
| `t` | scene time at which it appears | a number |
| `text_style` | how it is drawn (round 12) | optional; `lockup` (the default), `skywriting` or `landmark`; absent or null means `lockup`; any other value is refused with `400` naming `words[].text_style` |
| `system` | the coordinator wrote it, not a viewer (see "System words") | optional boolean the renderer ignores; absent means false |

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

#### Weather (addition, round 14)

`weather` is a list of keys. A frame draws the weather at its layer's time:
the frame's `t`, or the hold's start while the camera holds (so a hold keeps
one weather and converges). Between two keys each field eases from one to the
next (smoothstep of the fraction of the span); before the first key and after
the last the nearest key holds. With no keys every field has its default,
which is the sky through round 13.

| field | meaning | accepted |
|-------|---------|----------|
| `t` | scene time of the key | a finite number; keys are sorted by `t` on arrival |
| `cover` | cloud cover: 0 a clear sky, 0.5 the default, 1 nearly overcast (it moves the coverage threshold of the cloud field) | optional; 0 to 1; default 0.5 |
| `towers` | 0 fair-weather cumulus with tops at 5.6 km (the default), 1 towering cumulus whose densest cores reach 9.6 km | optional; 0 to 1; default 0 |
| `rays` | crepuscular rays: haze below the cloud base, densest near the ground, lit by the sun only where the clouds let it through, so the gaps throw beams and the clouds' shadows lie across the air; strongest looking toward a low sun. 0 draws no haze and costs nothing | optional; 0 to 1; default 0 |
| `snow_line_km` | altitude where snow starts on flatter ground (steeper slopes need more height); 1.6 is the default, 0.5 a winter landscape, 2.6 only the peaks | optional; 0 to 4; default 1.6 |

A field outside its range, or a key without `t`, is refused with `400` naming
the field. A change of the weather a hold draws starts a new hold.

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
- The SPS's VUI carries a `bitstream_restriction` with
  `max_num_reorder_frames` 0 and `max_dec_frame_buffering` equal to
  `max_num_ref_frames`. Without it a decoder must assume the level's
  MaxDpbFrames of reordering and holds that many frames before outputting
  one (Chrome on macOS: 7 or 8 frames, 234 to 268 ms). The page adds the
  restriction itself to an SPS without one (`fleet/web/core/sps.ts`), for
  renderers built before it.
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
 "t":1234.5,"frame":5678,"boot_id":"9f0c2a6e41d84b7c8e2f5a1b3c4d6e7f",
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
`boot_id` is 32 hex digits drawn at random when the process starts: a client
that reads a different one than before is talking to a restarted renderer,
which has no scene and no budget until they are posted again.
`drawn_h` is the stream height the last frame was drawn at (0 while nothing
streams). A renderer draws at the tallest height its streams ask whose frame
fits a frame slot, judged from its last 2 s of frames (render and encode), and
scales that frame to the taller streams: an opened 16:9 tile asks 720 against
the wall's 432, and on terrain a 720 frame overran its slot at the lowest
budget, which held the whole wall to that tile's rate. A height whose frames'
90th percentile passes 85% of a slot is left for the tallest height predicted
(frame time in proportion to drawn pixels) within 70%, and not drawn again for
60 s; a taller height is taken after 10 s at one height when predicted within
65%.
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
allow it. It posts a first budget, and raises one, only when every tile has
reported a fit; while a tile has none (a model starting again at a new drawn
size), an overloaded frame and the fits that are known still step it down. Tiles never adapt on
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
band** of `g + 18` pixels past the stream overlap on each side, where `g = ceil(0.015 * full_h)`
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
- Round 13: over the sky (hit kind sky, with or without a cloud in front), each
  earlier frame's color is first clamped, per channel, to the range of this
  frame's own colors over the 3 x 3 pixels around it that are sky. A cloud near
  the eye crosses the sky faster than reprojection through the sky's distance
  follows, and before this it left a trail of itself up to 7 frames long
  behind the camera.

This is computed for the rect plus the stream overlap (16 pixels) plus 16
pixels on each side (the denoiser's reach), together with an albedo guide (the
mean, over a pixel's samples, of its first hit's surface albedo, weighted
toward 1 by the haze and cloud in front of it, reprojected and weighted like
the color) and the first sample's normal. Where a pixel's total weight is
under 48 samples (fast motion, a fresh renderer, a 1080p view) it is then
denoised, blended in fully below 16 samples and not at all from 48 (converged
holds are not filtered): the color divided by the albedo guide is filtered by
an edge-aware a-trous wavelet (Dammertz et al. 2010) of four 3 x 3 passes at
steps 1, 2, 4 and 8 pixels, and multiplied back. A neighbour's weight falls
with its distance off this pixel's tangent plane (against the pixel's
footprint at its hit distance), with the angle between the normals (cosine to
the 64th) and with the luminance difference over 4 standard deviations of the
local noise (the variance of the luminance over the 3 x 3 pixels on the same
surface, carried through the passes as in SVGF, Schied et al. 2017); it is
zero across hit kinds, and the sky with the clouds over it is one surface that
only the luminance stops.

Every input of that rule is a whole-image pixel, a frame time or the scene, and
every pixel it can read lies inside the drawing renderer's guard band (the
denoiser reads 16 pixels past the overlap, whose reprojection reads `g + 1`
past those), so:
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
