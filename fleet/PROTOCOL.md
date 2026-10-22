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
  32/9, two 16:9 halves).
- Camera `pos` is the camera position and `look` the world point it looks at, as
  `[x, altitude, z]` in kilometres (the renderer's world units); `fov_deg` is the vertical
  field of view. `time_of_day` is a fraction of the day in [0, 1] (0.25 sunrise, 0.5 noon,
  0.75 sunset). Light `color` is linear RGB in [0, 1].

## Coordinator

`fcvm fleet coord` serves the browser over one WebSocket and holds the only copy of the
fleet state. Every change is one event through `fleet_core::apply`, which has no clock, no
I/O and no randomness, so every viewer sees the same sequence.

### `GET /live` (WebSocket)

**Server to browser**

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
 "check": {"verdict": "refuse", "reason": "..."}}
```

One per intent. `version` is the state version that first holds the intent's effect (or
its refusal in `actions`). An intent id the coordinator has seen before, from any viewer,
returns that id's first receipt again and changes nothing. `check` is present on `word`
intents that reached the text check; `verdict` is `allow`, `refuse` or `error`.

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
{"type": "intent", "id": "<client uuid>", "name": "relaunch", "args": {"tile": "fcvm:3"}}
{"type": "ping", "client_unix_ms": 1761134400100.5}
```

Intents:

| name | args | effect |
|------|------|--------|
| `relaunch` | `{"tile": "fcvm:3"}` (a tile id) or `{"tile": "slot-3"}` / `{"tile": "ec2-2"}` (a slot id) | bumps that slot's generation; refused for a dark tile, a draining host, or a second relaunch from the viewer within 2 s |
| `relaunch_all` | `{}` | bumps every slot's generation in one event (draining hosts excepted) and starts `relaunch_all`; one per viewer per 30 s |
| `set_auto_relaunch` | `{"secs": 60}` or `{"secs": null}` | shared interval; on each tick a slot whose last relaunch (or creation) is that old is relaunched. 10 to 86400 s |
| `camera` | `{"keyframes": [{"t", "pos", "look", "fov_deg"}, ...]}` | replaces the path (1 to 64 keyframes, in time order, fov 10 to 120) |
| `camera` | `{"drag": {"yaw_deg", "pitch_deg", "dive"}}` | turns the current camera by yaw (about world up) and pitch, moves it `dive` units along its view, easing over 0.3 s |
| `time_of_day` | `{"v": 0.3}` | a fraction of the day in [0, 1] |
| `word` | `{"text": "hello"}` | 1 to 24 characters after collapsing whitespace, no control characters, one per viewer per 5 s; checked by the text check first (below) |
| `light` | `{"pos": [x, y, z], "color": [r, g, b]}` | one per viewer per second; the scene keeps the latest 32 |

**Viewer text** (`word`). Before the event is applied the coordinator asks Claude Haiku on
Amazon Bedrock whether the text may be shown publicly. The model id and region come from
the coordinator's configuration (`--text-check-model`, `--text-check-region`). Only an
explicit allow commits the word; a refusal, an error, a timeout (5 s) or a coordinator
started without a text check (`--text-check none`) refuses it. The scene keeps the latest
16 words. A refused word's text appears nowhere: not in the scene, not in `actions`.

**Snapshot state**

```json
{
  "tiles": [
    {
      "id": "fcvm:1", "half": "fcvm", "rect": {"x": 0, "y": 0, "w": 0.25, "h": 0.5},
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
      "last_relaunch": {"clicked_at": 1761134400000, "by": "viewer:v-4", "first_frame_at": 1761134405200,
                        "phases": {"teardown_ms": 310, "restore_ms": 4300, "gpu_ms": 3100, "container_ms": 600, "first_frame_ms": 250, "total_ms": 5200}}
    },
    {"id": "ec2:1", "half": "ec2", "rect": {"x": 0.5, "y": 0, "w": 0.25, "h": 0.5}, "phase": "waiting_capacity", "slot": null, ...}
  ],
  "fleet": {"hosts": [{"name", "arch", "gpus": [{"sbdf", "name"}], "draining"}], "gpus": 8, "fcvm_gpus": 8,
            "ec2_instances": 0, "n": 8, "max_tiles": 100, "standby": []},
  "scene": {"epoch_unix_ms": ..., "camera": [...], "time_of_day": 0.3, "words": [{"text", "t"}], "lights": [{"pos", "color", "t"}]},
  "image_aspect": 3.5555555555555554,
  "settings": {"auto_relaunch_secs": null},
  "relaunch_all": {"started_at", "by", "fcvm_done_at", "ec2_done_at", "fcvm_last", "ec2_last", "fcvm_pending", "ec2_pending"},
  "viewers": {"count": 3},
  "actions": [{"at", "by", "name", "summary", "outcome"}]
}
```

- `tiles` lists the fcvm half then the EC2 half, each N tiles row-major from the top
  left; N = `fleet.n` = min(`max_tiles`, max(fcvm GPUs, EC2 instances)). Tile ids
  `fcvm:<k>` and `ec2:<k>` (1-based) name places; `slot` names the GPU behind the place,
  which can change when the fleet re-tiles. Slot ids are `slot-<n>` (fcvm, reused lowest
  first) and `ec2-<n>`.
- `phase`: `waiting_capacity` (dark: no GPU behind the tile, or EC2 refused capacity),
  `pending` (assigned, nothing reported yet), `relaunching` (a newer generation is wanted
  than the agent has reported), or what the agent reported: `stopping`, `restoring`,
  `starting`, `running`, `failed`.
- `frame_url` is the renderer's frame WebSocket as the browser should open it (append
  `?h=<height>`). After a relaunch the tile reconnects when `phase` is `running` again.
- `last_relaunch.first_frame_at - clicked_at` is the tile's relaunch time; `phases` are
  the agent's timings for that relaunch (see Agent).
- `relaunch_all.fcvm_done_at - started_at` is the fcvm half's click-to-last-first-frame
  time; `fcvm_last` names the slot that finished last. A half with nothing to relaunch
  keeps `*_done_at` null. `*_pending` count tiles still to reach their first frame.
- `actions` holds the latest 50 intents, committed and refused, oldest first.

### Running it

```
fcvm fleet coord --listen <addr:port> [--agent-cmd '<command>']... [--state-file <path>]
                 [--text-check bedrock --text-check-model <id> --text-check-region <region> | --text-check none]
                 [--max-tiles 100] [--ec2 none|fake] [--allowed-origin <scheme://host[:port]>]...
```

A viewer is one `/live` connection: the per-viewer limits (one word per 5 s, one light per
second, one relaunch per 2 s, one relaunch_all per 30 s) count per connection.

- `--agent-cmd` starts an agent as a child process and talks to it over its stdin and
  stdout: `fcvm fleet agent --stdio ...` locally, `ssh <host> fcvm fleet agent --stdio ...`
  for a remote host. Repeatable. An agent that exits is removed from the fleet (its
  slots leave the tiling in that event) and restarted after a backoff of 1 s doubling to
  30 s.
- `--state-file` keeps the state as JSON, rewritten atomically (temporary file, fsync,
  rename) at most once a second while it changes, and once more on shutdown; a restarted
  coordinator resumes its version, settings, scene,
  receipts and action log from it. Hosts and slots are rebuilt as agents say hello.
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
`ssh <host> fcvm fleet agent --stdio`, and later over a WebSocket the agent dials.

When its input closes, the agent tears down every VM it started and exits. If the agent
is killed outright, each VM's fcvm gets SIGTERM from the kernel (the agent arms
`PR_SET_PDEATHSIG` on every fcvm it starts, and fcvm arms it on every process below).

### Agent to coordinator

```json
{"type": "hello", "host": "<name>", "arch": "x86_64", "gpus": [{"sbdf": "0000:01:00.0", "name": "Tesla T4"}], "capacity": 8}
```

First line, once. `gpus` are the NVIDIA display functions bound to `vfio-pci`, in address
order; `capacity` is how many VMs the host runs at once (default: one per GPU). The
coordinator makes one slot per GPU and ignores a repeated address. The first hello binds the
connection to `host`; a hello naming a host another live connection holds is refused (the
coordinator closes the new connection), and a later hello on the same connection must name
the same host.

```json
{"type": "slot", "id": "slot-3", "phase": "running", "generation": 4, "vm": "fleet-slot-3",
 "frame_url": "ws://<host>:<port>/frames",
 "timings": {"teardown_ms": 310, "restore_ms": 4300, "ack_ms": 4300, "gpu_ms": 3100, "container_ms": 600, "first_frame_ms": 250, "total_ms": 5200},
 "error": null}
```

On every phase change. `generation` is the generation the agent is working toward or
running. Phases, in order: `stopping` (tearing the old VM down), `restoring` (fcvm started;
the snapshot restore and GPU attach run inside it), `starting` (fcvm acknowledged the
restore; the container and renderer are coming up), `running` (the renderer reported its
first frame), and `failed` (`error` says why; the agent retries with backoff).

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
{"type": "health", "id": "slot-3", "fps": 29.8, "gpu": { ... }}
```

Once a second per running slot: the renderer's `/health` `fps` and its GPU readings, passed
through.

```json
{"type": "draining", "reason": "spot interruption notice: terminate at 2026-10-22T10:00:00Z"}
```

The host is going away (EC2 `spot/instance-action` appeared). The coordinator takes the
host's slots out of the tiling at once and stops relaunching them; the agent keeps its VMs
until the host stops.

### Coordinator to agent

```json
{"type": "assign", "slots": [{"id": "slot-3", "sbdf": "0000:01:00.0", "generation": 4, "rect": {"x": 0, "y": 0, "w": 0.25, "h": 0.5}}]}
```

The complete set of slots the host runs, sent after the hello and whenever any of them
changes. For each slot the agent reconciles:

- no VM, or a VM below `generation`: tear the old one down, wait until its fcvm process
  has exited and no process holds the GPU's VFIO group, start a new one, wait for the
  renderer's `/health`, post the scene, report `running` with the timings. All slots of a
  host do this at once.
- a newer `generation` arriving mid-relaunch waits for the current relaunch and then runs
  once.
- `rect` changed, generation not: post the new scene to the running renderer, no
  relaunch. `rect: null` (standby, or the host drains): keep the VM, post nothing.
- a slot no longer listed: tear its VM down.
- a VM that exits on its own: report `failed`, relaunch after 1 s, doubling to 30 s; the
  backoff resets once a VM has run for 60 s.

```json
{"type": "scene", "epoch_unix_ms": 1761134400000, "image_aspect": 3.5555555555555554,
 "camera": [{"t": 0, "pos": [-32, 1.9, -55], "look": [-60, 1.9, -100], "fov_deg": 62}], "time_of_day": 0.3,
 "words": [{"text": "hello", "t": 12.5}], "lights": [{"pos": [1, 2, 3], "color": [1, 0.5, 0], "t": 14.0}]}
```

The scene shared by every renderer, sent after the hello and whenever it changes. The
agent posts `scene` plus the slot's `rect` to each running renderer as
`POST http://<publish ip>:<port>/scene`, with the body
`{"epoch_unix_ms", "rect", "image_aspect", "camera", "time_of_day", "words", "lights"}`.

### Running it

```
fcvm fleet agent --stdio --image <renderer image> --base-port <port>
                 [--host <name>] [--publish-ip <ip>] [--frame-host <name or ip>]
                 [--gpus <sbdf>=<name>,...] [--fcvm <path>] [--fcvm-arg <arg>]...
                 [--spot-notice-url <url>]
```

Each slot's VM is `fcvm podman run --name fleet-<slot> --kernel-profile gpu --gpu <sbdf>
--publish <publish ip>:<base port + gpu index>:8080 [--fcvm-arg ...] <image>`, run
rootless; the pre-start snapshot cache makes every launch after the first a restore. The
frame URL is `ws://<frame host>:<port>/frames` (one function, `frame_url`, to be replaced
by the slot's routed IPv6 name and TLS once fcvm has `--ipv6-address`). `--gpus` replaces
the sysfs inventory (tests and hosts without vfio-pci). `--spot-notice-url` polls an EC2
metadata URL (IMDSv2) for `spot/instance-action` every 5 s.
