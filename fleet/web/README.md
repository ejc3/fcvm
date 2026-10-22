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
pnpm lint && pnpm typecheck && pnpm test     # 85 unit tests (vitest)
pnpm build && pnpm test:e2e                  # 19 browser tests (Playwright)
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
20 frames per second; every run held all tiles at one t with none behind.

| tiles | viewport | stream height | decoded frames/s | draw per frame, p50 / p95 | browser CPU (all processes) | CPU per decoded frame |
|---|---|---|---|---|---|---|
| 16 | 1440x900 | 216 | 320 | 5.7 / 7.8 ms | 0.32 cores | 1.01 ms |
| 50 | 1440x900 | 180 | 1000 | 7.3 / 8.2 ms | 0.49 cores | 0.49 ms |
| 100 | 1440x900 | 90 | 2000 | 9.4 / 11.4 ms | 0.72 cores | 0.36 ms |
| 100 | 2560x1440 | 144 | 1994 | 18.4 / 20.8 ms | 1.02 cores | 0.51 ms |

Command: `FLEET_MOCK_THREADS=32 node scripts/measure.ts 16 50 100` (8 s warm-up, 15 s
measured) and `FLEET_MEASURE_VIEW=2560x1440 FLEET_MOCK_THREADS=48 node scripts/measure.ts
100`. Decode latency (decode call to output frame) stayed under 1 ms at p50 and 3 ms at
p95 in every run. Draw time follows the pixels drawn, about 5 ms per million in this
software canvas, more than the number of tiles: 100 small tiles cost less than 2x what 16
large ones do. At 100 tiles one software browser kept 2000 frames per second on one
core, so on this evidence 100 is not the limit; the measurement in
`docs/fleet/ARCHITECTURE.md` (a laptop and a phone, Chrome and Safari) should set
`max_tiles`.

**Safari.** According to WebKit's release notes, WebCodecs `VideoDecoder` shipped in Safari 16.4 (macOS and iOS), with H.264
among its codecs, as did `OffscreenCanvas` with a 2D context. Playwright's WebKit build
(WebKit 27.2 on Linux, not Apple's Safari) reports H.264 decoding supported, and it runs
the mosaic, seam and hover tests here (the `@engines` tests). If a worker has no
`requestAnimationFrame`, the page ticks it from the main thread. Apple's Safari itself is
not tested by this PR.

## Interface notes

The brief's interfaces and the coordinator's `fleet/PROTOCOL.md` (in the coordinator PR)
differ in places. The page reads the coordinator's documented shapes and the brief's
where they are compatible:

- **Tile identity:** tiles are places (`fcvm:3`) with the GPU's `slot` (`slot-3`) beside
  them. The page sends the tile id in `relaunch` and shows the slot name.
- **`fleet.hosts`** is a list in PROTOCOL.md and a number in the brief; both are read.
- **`image_aspect`** is a top-level field; the page uses it (default 16/9). PROTOCOL.md's
  default is 32/9 while the renderer's is 16/9: one of them should change. At 32/9 the
  image is a strip 405 px tall on a 1440x900 screen.
- **`time_of_day`** is hours in [0, 24) in PROTOCOL.md (the page uses hours) but
  `0..1` in the renderer's scene parser.
- **`camera` drag** is `{"drag": {"yaw_deg", "pitch_deg", "dive"}}`. The page sends a drag
  across the whole image width as 90 degrees of yaw (negative when dragging right: the
  drag grabs the world), down the height as 50 degrees of pitch, and a wheel notch as one
  unit of dive. The sign convention needs agreeing with the renderer.
- **Input-to-pixel latency** needs to know which camera key an intent produced. Keys
  carry no intent id in PROTOCOL.md, so the page takes the first key newer than any it
  held when it sent the intent, once the receipt's version is accepted. Tagging each key
  with the intent id (`"intent": "<id>"`) would make it exact; the page uses the tag when
  present.
- **`light`**: PROTOCOL.md takes a world position `[x, y, z]`, which a browser cannot
  know. The page sends the image point it was dropped on, `{"pos": {"u", "v", "t"},
  "color": [r, g, b]}`, and the coordinator (or renderer) should place it.
- **`gpu`** is `{sbdf, name}` with readings in a separate `gpu_stats`; the page reads
  `temp_c`, `power_w` and `util_pct` from `gpu_stats` (the renderer's names) and ignores
  negative values.
- **`last_relaunch.phases`** holds `<part>_ms` keys in alphabetical order, with
  `restore_ms` containing `gpu_ms` and `total_ms` a sum; the page orders the parts and
  splits the GPU out of the restore. EC2 timing keys are not defined yet; the mock uses
  `request_ms`, `boot_ms`, `container_ms`, `first_frame_ms`.
- **`actions`** carry `summary` and `outcome`, not the arguments. The page words the
  ticker from the name and the summary; arguments would let it say more without parsing.
- **Receipts:** `committed` and `refused` (also `applied`, `accepted`, `ok`); a
  `{"type": "error"}` message is kept, not counted as unreadable.
- **The clock answer** echoes `client_unix_ms` in PROTOCOL.md; the brief's did not. The
  page uses the echo and otherwise pairs the answer with its one outstanding ping.
- **Phases:** `waiting_capacity` (PROTOCOL.md) and `waiting_for_capacity` both read as
  "waiting for capacity"; `pending` and `relaunching` have their own words.
- **A viewer's own name** is not in the snapshot, so the page cannot say "you".

## Not done here

- Deploying: the Cloudflare Workers build (OpenNext) and the staging Worker behind Access.
  The client bundle uses no Node APIs; the server page reads one environment variable.
- Running against the real coordinator and renderers, and the interface decisions above.
- The `max_tiles` measurement on a laptop and a phone in Chrome and Safari.
