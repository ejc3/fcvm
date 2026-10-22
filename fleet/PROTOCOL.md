# fleet: renderer protocol

The renderer is the program in the container of every GPU VM (fcvm and EC2
alike). It draws one rectangle of one shared image and streams it as H.264.
The agent (or, locally, any client) sets the scene with `POST /scene`;
browsers read frames from `GET /frames` and stats from `GET /health`.

The renderer listens on `0.0.0.0:8080` (IPv4 and IPv6, plain HTTP and
WebSocket; TLS comes later). Source: `fleet/renderer/`.

## Coordinates and time

- **World units are kilometres.** A position is `[x, altitude, z]`: `x` and
  `z` are horizontal, the second number is the altitude above sea level. The
  renderer bends the world into a planet (radius 6360 km) itself, so altitudes
  stay altitudes anywhere on the map. The default flight works around
  `x, z in [-60, 0]`; land, sea and mountains continue in every direction
  (the GPU's baked terrain covers |x|, |z| < 512 km and is computed directly
  beyond, at a higher cost per frame).
- The eye is kept at least 60 m above the ground and the sea; a keyframe
  below that is lifted.
- **`t`** is seconds since `epoch_unix_ms` on the VM's clock (chrony keeps the
  VMs in step): `t = (now_unix_ms - epoch_unix_ms) / 1000`.
- **Frames are drawn on a fixed grid of times:** `t = k / 30` for integer `k`
  (`FPS`, default 30). Every renderer draws the same `t` values, so frames of
  different tiles with equal `t` are the same instant; a renderer that cannot
  keep up skips slots rather than drawing between them. The `t` in a frame
  header is that exact double.

## `POST /scene`

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
| `time_of_day` | sun position: 0.25 sunrise, 0.5 noon, 0.75 sunset | 0 to 1 |
| `words` | viewer text | optional; may be empty |
| `lights` | viewer lights | optional; may be empty; `color` components 0 to 64 |

Answers `200 {"ok":true}`, or `400 {"error":"<what is wrong>"}` naming the
field. A scene replaces the previous one whole.

### Camera interpolation and holds

- Between two keyframes that differ, `pos` and `look` follow a Catmull-Rom
  spline (tangents scaled to each segment's duration) and `fov_deg` is linear.
- **A hold** is a run of consecutive keyframes with identical `pos`, `look`
  and `fov_deg`. Inside it the camera is exactly still. Before the first
  keyframe and after the last, the nearest keyframe holds (and joins any hold
  it starts or ends).
- Next to a hold the tangent is zero, so motion eases out of and into holds.
- To let the image converge, include holds: two equal keyframes a few seconds
  apart.

### Words, the clock and lights

- **Text** is uppercased; characters outside the font (A-Z, 0-9 and
  `: . , - + ! ? ' " / # & @ ( ) = * < > _`) become `?`; at most 28 characters.
- A word appears at its `t` (fading in over 1.5 s) as large glowing letters in
  the sky about 8 km ahead of where the camera was at that `t`, placed sideways
  by a hash of its index and `t`, and fades out at 90 s. Each viewer sees it
  face their current camera. The newest 16 words are drawn.
- **The clock** is always drawn: the UTC time of the frame
  (`epoch_unix_ms / 1000 + t`, to the second), 12 km ahead of the camera and
  above the horizon.
- A **light** appears at its `t` as a glowing sphere (35 m) of its `color`, and
  lights the terrain within 3 km. At most 32.
- Words, the clock and the light spheres are drawn every frame at that frame's
  `t`, without sampling noise, on top of the accumulated image (below).

## `GET /frames?h=<height>` (WebSocket)

`h` is 16 to 2160. The tile's size follows from `h` and the rect; see
"Tile geometry". The first message is text:

```json
{"codec":"avc1.64001f","width":480,"height":540,
 "full_width":1920,"full_height":1080,"x0":480,"y0":0,"fps":30}
```

`codec`, `width`, `height` are the contract. The renderer adds `full_width`,
`full_height` (the whole image's pixel grid at this height), `x0`, `y0` (this
tile's top-left pixel in that grid) and `fps`, so a client can place a tile
exactly.

Then binary messages, each one access unit:

| bytes | content |
|-------|---------|
| 0-3 | ASCII `FCVF` |
| 4 | version, 1 |
| 5 | flags: bit 0 keyframe |
| 6-7 | width, u16 LE |
| 8-9 | height, u16 LE |
| 10-17 | `t`, f64 LE |
| 18-21 | frame number, u32 LE (the renderer's own count; it skips numbers for frames a subscriber did not get) |
| 22- | one H.264 Annex-B access unit; SPS and PPS precede every keyframe |

- H.264 High profile, no B-frames, NVENC with ultra-low-latency tuning,
  BT.709 limited range (signalled in the VUI). CBR at about 0.2 bits per pixel
  at 30 fps, at least 0.6 Mbit/s.
- The first binary message is a keyframe. Keyframes follow at least every 2 s.
- The client may send text `{"h": <height>}`. The renderer then sends a new
  text header and continues from a keyframe at the new size.
- A subscriber that falls more than 90 messages or 16 MiB behind has its queue
  dropped and resumes at the next keyframe (which the renderer then forces).
- **Minimum size:** NVENC's H.264 encoder on a T4 takes nothing narrower than
  145 px or shorter than 49 px. When the requested height gives a smaller
  tile, the stream is the smallest larger even height that fits (a 1/16 x 1/9
  rect of a 16:9 image asked at `h=136` streams at 146 x 146). The header
  says the actual size; the client scales.
- The renderer draws once per frame at the largest height any subscriber
  wants, scales it down (area filter) for the smaller ones, and encodes only
  heights that have subscribers.

### Tile geometry

For a requested height, `H` is `h` clamped to 16..2160 and rounded down to
even. Then

```
full_h = round(H / rect.h)          full_w = round(full_h * image_aspect)
y0     = round(rect.y * full_h)     x0     = round(rect.x * full_w)
W      = round(rect.w * full_w), rounded up to even
```

and the tile is whole-image pixels `[x0, x0+W) x [y0, y0+H)`. Tiles of a grid
asked at one height share `full_w x full_h`; neighbours meet with no gap and at
most one pixel of overlap, and an overlapping pixel is the same whole-image
pixel in both, drawn identically.

## `GET /health`

```json
{"fps":30.0,"heights":[240],
 "gpu":{"name":"Tesla T4","temp_c":38,"power_w":64.2,"util_pct":70,"mem_used_mb":648.3},
 "t":1234.5,"frame":5678,
 "ready":true,"render_ms":23.2,"encode_ms":0.5,"spp":38,"hold":false,"hold_samples":0,
 "startup_ms":{"gpu_ready":1515,"first_frame_drawn":1845,"first_frame_sent":1899}}
```

The first six keys are the contract. GPU numbers come from NVML, sampled once
a second; a value NVML does not give is `null`. The rest: `ready` (the GPU is
initialized), `error` (only when the renderer has failed), `render_ms` and
`encode_ms` of the last frame, `spp` (samples per pixel drawn in it),
`hold`/`hold_samples` (see below), and `startup_ms`, milliseconds from the
process start (read from `/proc/self/stat`) to CUDA being ready, the first
frame drawn and the first frame sent to a subscriber (`-1` until it happens).
`Access-Control-Allow-Origin: *` is set on this route only.

## Sampling, accumulation and determinism

Every pixel is a path-traced estimate. Its random numbers are seeded by
(whole-image pixel, seed, sample index), never by the tile or the GPU, and no
value is reduced across pixels, so a pixel's samples are the same wherever it
is drawn.

**Budget.** Each frame spends a GPU-time budget: the renderer draws as many
samples per pixel (1 to 64) as keep a frame's GPU time near 70% of its 33 ms
slot, adjusting after every frame (`ADAPT=0` fixes it at `BUDGET`
pixel-samples per frame, default 1,000,000). A small tile therefore gets many
samples per frame and a 1080p view one.

**While the camera holds** (see "Camera interpolation and holds"):

- The hold has a seed computed from the hold's start `t`, its pose, the time of
  day, the whole-image size and the lit lights (a light appearing, or any
  change to those, starts a new hold). The rect is not part of it.
- Sample `k` of a pixel in a hold is a pure function of (pixel, hold seed, `k`),
  drawn at the hold's start time (clouds and water stand still during a hold).
  Each frame adds the next samples to a per-pixel sum in order, so the sum
  after `n` samples does not depend on how they were split across frames.
- Accumulation stops at 1024 samples per pixel.
- While fewer than 256 samples are in, the motion history from before the hold
  (below) is blended in with a weight that falls linearly to zero at 256
  samples, so the switch from motion to hold does not flash noisy.
- **Result:** once a renderer has at least 256 samples of a hold, its pixels
  depend only on (scene, hold, sample count, whole-image pixel) and, for the
  overlays, on `t`. Two renderers with the same sample count give identical
  pixels; at 1024 every renderer gives the same image. A relaunched renderer
  starts at 0 and catches up, showing as a noisier patch that heals.

**While the camera moves:** every frame draws fresh samples at its own `t`
seeded by (pixel, `t`, sample index) and blends them into a history
reprojected from the previous frame this renderer drew (as in TAA): the
pixel's first hit is projected into the previous camera, the four history
texels around it are used where their hit kind (sky, land, water) matches and
their depth is within 4%, and the history counts as at most 16 samples. History
from outside this renderer's own rect does not exist, so pixels whose
reprojection leaves the tile start fresh.

This part is not identical across GPUs, and it is impractical to make it so:
the history depends on which frames a renderer drew (a slower one skips slots)
and on pixels it never had (next to a tile edge, motion brings in pixels from
the neighbour's rect). What holds is that each frame's own samples are the
same function everywhere, so tiles differ only in how much noise is left, not
in what they show; at the next hold they converge to identical pixels.

**Accumulation off** (`ACCUMULATE=0`, and the seam test): each frame is the
average of its own samples, a pure function of (scene, `t`, samples per pixel,
whole-image pixel).

**The seam test** (`fleet-renderer test`, run inside the container on a GPU)
draws a 960 x 540 image whole and as a 2 x 2 grid of tiles and compares them
pixel for pixel, 8-bit and float: once with accumulation off on a moving frame,
and once in a hold with 16 samples split 4 x 4 for the whole image and 2 x 8 for
the tiles. Two controls (a tile drawn one pixel off, another `t`) must differ.

## Process

- `fleet-renderer` (or `serve`) listens at once and initializes the GPU in
  the background; `/health` answers with `"ready": false` until it is ready.
- With no subscriber it draws twice a second at a small size, so `/health`
  stays current; a new subscriber wakes it at once.
- Kernels are SASS for `sm_75` only (no PTX), so CUDA compiles nothing at
  start. Other GPU generations need a build with their `GENCODE`.
- Environment: `PORT` (8080), `FPS` (30), `BUDGET`, `ADAPT` (1),
  `ACCUMULATE` (1).
