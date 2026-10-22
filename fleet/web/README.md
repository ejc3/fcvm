# fleet/web: the GPU wall page

The page for `docs/fleet/ARCHITECTURE.md`: one image tiled across every GPU, fcvm on the
left half and EC2 on the right, live from each tile's own renderer, with the
relaunch-all race, the shared camera, viewers' words and lights, a stats switch and a
shared auto-relaunch setting. It runs against a mock fleet (`mock/`) until the
coordinator and renderers are deployed.

![The wall at rest](docs/screenshots/wall-desktop.jpg)

| | |
|---|---|
| ![Relaunch all, 4 s in](docs/screenshots/race-mid.jpg) | ![Hovering a tile](docs/screenshots/wall-hover.jpg) |
| ![The stats switch](docs/screenshots/wall-stats.jpg) | ![An opened tile](docs/screenshots/tile-opened.jpg) |

<img src="docs/screenshots/phone.jpg" alt="The wall on a phone" width="260">

## Layout

```
core/      the rules, pure TypeScript: no DOM, no clock, no randomness, no I/O
           (lint and a tsconfig with no DOM types enforce it). Protocol parsing, the
           reducer, pending intents, clock sync, layout, the common-t choice, view
           models and every visible string.
app/       Next.js (App Router). Draws view models and sends actions:
           _wall/Wall.tsx           the page, input handling
           _wall/frames.worker.ts   frame sockets, WebCodecs decoders, one OffscreenCanvas
           _wall/wall-client.ts     the coordinator socket and the store
           wall.css                 the surface's own stylesheet (no component library)
mock/      a mock coordinator and mock renderers, Node only
e2e/       Playwright tests against the mock (Chromium; a tagged subset in WebKit)
scripts/   dev-mock.ts (run the page on the mock), measure.ts (decode and draw cost)
```

## Running it

Ports come from the environment or a gitignored `.env.ports`; nothing uses a framework
default. On a machine with the port registry, claim a block first
(`ports claim fcvm-fleet-web "$PWD"`) and write it down:

```
FLEET_WEB_PORT=<block+0>
FLEET_MOCK_COORD_PORT=<block+1>
FLEET_MOCK_RENDER_PORT=<block+2>
FLEET_TEST_BASE_PORT=<block+10>      # tests use it and the next two
FLEET_MEASURE_BASE_PORT=<block+15>   # measure.ts uses it and the next two
```

```
pnpm install
pnpm dev:mock         # the page on the mock fleet, next dev
pnpm build && pnpm start:mock    # the same with a production build
pnpm lint && pnpm typecheck && pnpm test     # 89 unit tests (vitest)
pnpm build && pnpm test:e2e                  # 20 browser tests (Playwright)
FLEET_SCREENSHOT_DIR=$PWD/docs/screenshots pnpm test:e2e   # also rewrites the pictures
pnpm build && pnpm measure [16 50 100]       # decode and draw cost
```

On a small machine, `FLEET_E2E_VIEW=1024x640 FLEET_MOCK_FPS=10 FLEET_MOCK_THREADS=3`
makes the mock's work fit (CI uses these on a 4-vCPU runner). The mock needs `ffmpeg` with libx264. Its knobs (`FLEET_MOCK_TILES`, `FLEET_MOCK_FCVM_MS`,
`FLEET_MOCK_EC2_MS`, `FLEET_MOCK_FPS`, ...) are listed at the top of `mock/main.ts`.
In production the page reads the coordinator's URL from `FLEET_COORD_URL` per request.

## How the page works

**Two kinds of connection.** One WebSocket to the coordinator's `/live` for snapshots,
receipts and clock answers, and one per tile to its renderer for frames. The frame
sockets, decoders and the canvas live in a worker, so the React thread never sits
between a frame and the screen.

**Shared state.** Every intent carries a client UUID and is shown at once as pending
(a relaunched tile goes dark for its viewer before the coordinator answers, a setting
shows its new value). It stays pending until its receipt has arrived and a snapshot at
the receipt's version has been accepted, so the overlay never flickers back. Snapshots
older than the last accepted version are dropped. After a reconnect the page sends the
intents that have no receipt again, with the same ids; the coordinator applies an id
once and answers a repeat with the first receipt.

**Clock.** Pings go out five times quickly, then every 2 s. The offset comes from the
sample with the smallest round trip of the last eight. Race clocks, dark-tile clocks,
the auto-relaunch countdown and frame age all use the coordinator's time.

**One common t.** Each tile keeps up to six decoded frames. Every draw picks T, the
frame time held by the most keeping-up tiles (the latest, on a tie), and draws each
tile's frame at exactly T. A tile more than 0.35 s behind the newest, or whose frames the
others no longer hold, is drawn dimmed from its newest frame before T; a tile that just
joined stays dark until the image reaches its first frame; T never goes back. A decoder
holding more than eight undecoded chunks drops input until the next keyframe. Two different times are never drawn side by
side undimmed. Draws happen only when T or the plan changes, so the canvas redraws at
the renderers' rate, not the display's.

**Layout.** The image keeps the coordinator's `image_aspect` and is fitted inside the
space the page has. Tile edges are snapped to device pixels from the image's own edges,
so neighbours share an edge exactly. Each tile asks its renderer for the height it
occupies in device pixels, rounded up to one of 72, 90, 108, 144, 180, 216, 270, 360,
432, 540, 720 or 1080.

**Seams on demand.** With the pointer over the image every tile boundary fades in as a
1-device-pixel line at 25% white, and the tile under the pointer gets an outline in its
half's colour and a label (slot, kind, GPU, host, last relaunch). A long press does the
same on a touch screen; keyboard users tab into a grid of the tiles and move with the
arrow keys. All of it is drawn by the page over the frames, never in them.

**An opened tile** is drawn into a larger box with the tile's own aspect, and only its
renderer is asked for a taller stream (`{"h": ...}` on the open socket). The page
accepts either a new text header or a keyframe of the new size after a height change.

## Measurements

Decode and draw cost in Playwright's headless Chromium 156 on this development machine
(a 96-core Arm Neoverse-V2 server with no GPU, so decoding and canvas drawing are both
software). The mock renderers ran on the same machine. These numbers are indicative
only: a laptop or phone decodes in hardware and composites on its GPU. Mock streams are
30 frames per second, on the renderer's t = k/30 grid.

| tiles | viewport | stream height | decoded frames/s (of 30 x tiles) | draw per frame, p50 / p95 | display frame interval, p50 / p95 | browser CPU (all processes) | CPU per decoded frame |
|---|---|---|---|---|---|---|---|
| 16 | 1440x900 | 216 | 479 of 480 | 5.7 / 6.0 ms | 16.7 / 16.8 ms | 0.45 cores | 0.94 ms |
| 50 | 1440x900 | 180 | 1500 of 1500 | 7.3 / 8.3 ms | 16.7 / 17.1 ms | 0.69 cores | 0.46 ms |
| 100 | 1440x900 | 90 | 3000 of 3000 | 9.4 / 9.8 ms | 16.7 / 17.5 ms | 1.01 cores | 0.34 ms |
| 100 | 2560x1440 | 144 | 2930 of 3000 | 18.9 / 20.9 ms | 19.8 / 31.5 ms | 1.45 cores | 0.49 ms |

Command: `FLEET_MOCK_THREADS=40 node scripts/measure.ts 16 50 100` (8 s warm-up, 15 s
measured) and `FLEET_MEASURE_VIEW=2560x1440 FLEET_MOCK_THREADS=60 node scripts/measure.ts
100`, with the machine's load average at 26 from other work. At 1440x900 every run held
all tiles at one t with none behind, and decode latency (decode call to output frame)
stayed under 1.5 ms at p95. Draw time follows the pixels drawn, about 5 ms per million in
this software canvas, more than the number of tiles: 100 small tiles cost 1.6x what 16
large ones do. At 2560x1440 a draw (18.9 ms) no longer fits a 60 Hz display frame, the
display loop slows to 20 to 32 ms, and the page decoded 2930 of the 3000 frames sent per
second: this software canvas is the limit there. On this evidence 100 tiles fit one
browser's decoding budget; the measurement in `docs/fleet/ARCHITECTURE.md` (a laptop and
a phone, with GPU compositing, Chrome and Safari) should set `max_tiles`.

**Safari.** According to WebKit's release notes, WebCodecs `VideoDecoder` shipped in Safari 16.4 (macOS and iOS), with H.264
among its codecs, as did `OffscreenCanvas` with a 2D context. Playwright's WebKit build
(WebKit 27.2 on Linux, not Apple's Safari) reports H.264 decoding supported, and it runs
the mosaic, seam and hover tests here (the `@engines` tests). If a worker has no
`requestAnimationFrame`, the page ticks it from the main thread. Apple's Safari itself is
not tested by this PR.

## Interface notes

The page and the mock follow `fleet/PROTOCOL.md` as the coordinator PR (#1146,
"Coordinator") and the renderer PR (#1144, "Renderer") write it: tiles are places
(`fcvm:3`) with their slot (`slot-3`) beside them, the snapshot carries `image_aspect`,
`gpu` is `{sbdf, name}` with readings in `gpu_stats`, timings are `<part>_ms`, receipts
are `committed` or `refused`, the clock answer echoes `client_unix_ms`, `{"type":
"error"}` exists, snapshots may skip versions, frames fall on `t = k/30`, the stream
header carries `full_width`, `full_height`, `x0`, `y0` and `fps`, the stream may be
larger than asked (the encoder's 146 x 50 minimum) and the page scales it into the
tile, `{"h": N}` brings a new header and a keyframe, and a stream carries a margin
around its rectangle with `crop: {left, top, right, bottom}` in its header (zeros when
absent) so separately encoded tiles meet without an H.264 edge. The page draws only
the inner rectangle: it draws the whole frame scaled so the inner rectangle covers the
tile's box, clipped to the box, because WebKit ignores a VideoFrame's source rectangle
and `visibleRect` when drawing. The mock encodes an 8 px margin on each inner side. The
page never reads `/health` (so not `slots_missed`) and never sets the scene's `budget`. The brief's older shapes
(`fleet.hosts` as a number, `waiting_for_capacity`, `applied`) are still read.

What the page does where the protocol leaves a choice:

- **Camera drag** sends `{"drag": {"yaw_deg", "pitch_deg", "dive"}}`: a drag across
  the whole image width is 90 degrees of yaw (negative when dragging right: the drag
  grabs the world), down the height 50 degrees of pitch, and a wheel notch one unit
  of dive. The sign convention should be agreed with the coordinator.
- **A dropped light** is sent as `{"pos": [x, altitude, z], "color": [r, g, b]}`: the
  page takes the camera at the drawn t from the snapshot's keys (linear between keys;
  the renderer's Catmull-Rom spline differs only while the camera moves) and places
  the light 2 km along the ray through the clicked pixel.
- **Input-to-pixel latency** needs to know which camera key an intent produced. Keys
  carry no intent id, so the page takes the first key newer than any it held when it
  sent the intent, once the receipt's version is accepted. An `"intent"` field on a key
  would make it exact; the page uses one when present.
- **The opened tile and the grid** draw each frame into the tile's rectangle on screen,
  not by `x0` / `full_width`; the two differ by under one stream pixel.

Mismatches between the other two that the page cannot settle:

- `image_aspect` defaults to 32/9 in the coordinator and 16/9 in the renderer. At 32/9
  the image is a strip 405 px tall on a 1440x900 screen.
- `time_of_day` is hours in [0, 24) at the coordinator (the page sends hours) and a
  fraction of a day in the renderer's `POST /scene`: the coordinator has to convert.
- Words: the coordinator accepts 24 characters, the renderer draws 28.
- `actions` carry `summary` but not the arguments, so the ticker words what it can from
  the summary; arguments would let it say more without parsing.
- A viewer's own name is not in the snapshot, so the page cannot say "you".

The mock encodes H.264 baseline with libx264 rather than High with NVENC, reports EC2
phases `launching` and `starting` with `launch_ms` and `ready_ms` as the coordinator's
EC2 driver does, and shows `relaunching` for 150 ms before a tile's first reported
phase.

## Not done here

- Deploying: the Cloudflare Workers build (OpenNext) and the staging Worker behind Access.
  The client bundle uses no Node APIs; the server page reads one environment variable.
- Running against the real coordinator and renderers, and the interface decisions above.
- The `max_tiles` measurement on a laptop and a phone in Chrome and Safari.
