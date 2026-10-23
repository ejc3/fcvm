# fleet: renderer protocol

The renderer is the program in the container of every GPU VM (fcvm and EC2
alike). It draws one rectangle of one shared image and streams it as H.264.
The agent (or, locally, any client) sets the scene with `POST /scene`;
browsers read frames from `GET /frames`, a poster frame from `GET /still`
and stats from `GET /health`.

The renderer listens on `0.0.0.0:8080` (IPv4 and IPv6, plain HTTP and
WebSocket) for host-local clients, and, given a certificate, on `:443` with
TLS for browsers (HTTPS and WSS, the same routes; see "TLS"). Source:
`fleet/renderer/`.

## Coordinates and time

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
| `time_of_day` | sun position: 0.25 sunrise, 0.5 noon, 0.75 sunset (the built-in scene uses 0.245, golden hour) | 0 to 1 |
| `words` | viewer text | optional; may be empty |
| `lights` | viewer lights | optional; may be empty; `color` components 0 to 64 |
| `budget` | pixel-samples per frame for every tile (see "Budget") | optional; 10,000 to 50,000,000; absent means the renderer's `BUDGET` |
| `layout_generation` | the coordinator's layout this rect belongs to (see "Layout generations") | optional; a non-negative integer or null |
| `rect_from_frame` | the first frame (scene time x 30) drawn with `rect` | optional; a non-negative integer or null |
| `previous_rect` | the rect drawn for frames before `rect_from_frame` | optional; null, or a rect as above; needs `rect_from_frame` |

Answers `200 {"ok":true}`, or `400 {"error":"<what is wrong>"}` naming the
field. A scene replaces the previous one whole.

### Layout generations (addition, round 10)

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
Without `layout_generation` a scene takes effect at once, as before. With it
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
- Words and the clock are laid out on the screen of the whole image, not in
  the world: each is placed along the camera's own right and up axes at a
  fixed distance in front of it, so it keeps its place in the frame while the
  camera moves, and every tile computes the same layout from the same camera.
  Changed in round 5; before, they were anchored in the world.
- **The clock** is always drawn: the time of the frame
  (`epoch_unix_ms / 1000 + t`, to the second) in America/Los_Angeles,
  labelled `PDT` or `PST` (round 7; it was UTC). Daylight time runs from
  2:00 local on the second Sunday in March (10:00 UTC) to 2:00 local on the
  first Sunday in November (9:00 UTC); the renderer applies that rule in code,
  so the guest needs no time zone data and every tile draws the same digits.
  Centred, its middle at 66% of
  the half-height above the image centre, 7.5% of the image height tall.
  Its letters are solid and dark with a bright rim and a dark halo, so it
  reads over cloud, sky and terrain (at 180 px tiles too), and nothing in the
  world hides it.
- A word appears at its `t` (fading in over 1.5 s) and fades out at 90 s.
  The newest 16 words are drawn, 5% of the image height tall, in 6 rows
  (centres from 38% of the half-height above the image centre down to 62%
  below it) and 3 columns, filled newest first down the left column, then the
  middle, then the right, each nudged sideways by a hash of its index and
  `t`. A word is moved as needed to keep it inside the middle 85% of the image
  width, so none is cut off at the edge. Terrain in front of a word hides it.
- A **light** appears at its `t` as a glowing sphere (35 m) of its `color`, and
  lights the terrain within 3 km. At most 32.
- Words are glowing glass tubes: lit by the sun and the sky, with an emissive
  core, a bright rim and a soft halo. Words, the clock and the light spheres
  are drawn every frame at that frame's `t`, without sampling noise, on top of
  the accumulated image (below).

## `GET /frames?h=<height>&q=<quality>` (WebSocket)

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

### Stream quality (addition, round 4)

| `q` | bits per pixel of the delivered picture, at 30 fps | floor | cap | preset | for |
|---|---|---|---|---|---|
| `hi` | 4 | 2 Mbit/s | 160 Mbit/s | P4 | an aggregator on the same host that decodes and re-encodes once: near-lossless, so its own encode sees no tile edges |
| `std` | 0.8 (`ENC_BPP`) | 0.6 Mbit/s | 20 Mbit/s | P6 | the default; tile edges decode within the seam ratio of the unencoded image |
| `lo` | 0.25 | 0.15 Mbit/s | 20 Mbit/s | P6 | direct mode at high tile counts, where a browser receives many small tiles |

All three are CBR with no B-frames and the same keyframe rules. Measured
seam ratios and rates per quality are in the PR and `fleet/renderer/README.md`.

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

## Sampling, accumulation and determinism

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
samples drawn), smoothed over about five frames, started again when the drawn
size changes. It is the largest budget whose predicted frame takes 70% of a
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

## `GET /still?h=<height>&quality=<1-100>` (addition, round 5)

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

## TLS (addition, round 6)

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

## `GET /raw?t=<t>` (debug)

Waits for the frame at `t` (a future slot, at most 15 s ahead) and returns this
renderer's tile of it unencoded, as a binary PPM at the height being drawn,
with a header `X-Fleet-Tile: {"t":..,"x0":..,"y0":..,"width":..,"height":..,
"full_width":..,"full_height":..,"spp":..,"layout_generation":..}` (and
`hq`, `budget`, `budget_source`); the tile is the one drawn for that frame,
under `previous_rect` before `rect_from_frame`; `404` if the renderer skipped that
slot. `tools/rawmosaic.py` uses it to stitch an unencoded mosaic that can be
compared pixel for pixel with the same frame drawn whole
(`fleet-renderer still --seq 12`). Not for browsers.

## Process

- `fleet-renderer` (or `serve`) listens at once and initializes the GPU in
  the background; `/health` answers with `"ready": false` until it is ready.
- With no subscriber it draws twice a second at a small size, so `/health`
  stays current; a new subscriber wakes it at once.
- Kernels are SASS for `sm_75` only (no PTX), so CUDA compiles nothing at
  start. Other GPU generations need a build with their `GENCODE`.
- Environment: `PORT` (8080), `FPS` (30), `BUDGET` (250000), `ADAPT` (1), `MARGIN` (16), `ENC_BPP` (0.8),
  `ACCUMULATE` (1), `TLS_CERT_FILE` (unset: no TLS listener), `TLS_PORT` (443). `FIT_SCALE`
  (1) scales the reported `budget_fit`, to test a grid of unequal GPUs; never set it in production.
