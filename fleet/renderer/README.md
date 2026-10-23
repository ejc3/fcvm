# fleet/renderer

The program in every GPU VM of the fleet wall: it path-traces its rectangle of
one shared procedural planet with CUDA, encodes it with NVENC and streams it
over WebSocket. The interface (`POST /scene`, `GET /frames`, `GET /still`,
`GET /health`) is
in [`../PROTOCOL.md`](../PROTOCOL.md); the design is in
[`../../docs/fleet/ARCHITECTURE.md`](../../docs/fleet/ARCHITECTURE.md).

## The scene

A planet seen from a few hundred metres to 100 km up, every pixel a pure
function of `t`, the scene and its whole-image coordinates. The built-in
scene is at golden hour (sun about 7 degrees up):

- **Terrain**, ray marched: continents and seas, mountain massifs of eroded
  fbm (octaves damped by the accumulated slope) with domain-warped ridges,
  more octaves for shading normals as the pixel footprint shrinks. The
  large-scale fields (land, massif, warp) are baked once at start into a
  filtered 2048 x 2048 texture over +-512 km, which halves the cost of a march
  step; outside it they are computed directly. Shading reads the same fields
  through a quadratic B-spline, because the hardware's bilinear filter is not
  smooth enough for normals. Bare rock gets ridged detail normals (gullies a
  few tens of metres across), faded out where a pixel is larger than them.
- **Materials** by height, slope and noise: two rock colours in strata, scree
  below cliffs, meadow, heath and dry grass, mottled forest canopy, beach, and
  snow on flatter high ground, with albedo detail down to tens of metres;
  Lambert plus GGX.
- **Water**: the sea at altitude 0 and lakes in mountain basins (the basin
  floor carved a few tens of metres down, the lake's surface a few metres
  below the floor's level). Animated wave normals with gusts (patches a few
  hundred metres across of rough and of mirror-calm water), ripples below the
  pixel turned into a blurred reflection, Fresnel reflection of terrain, sky
  and clouds, sun glitter, depth-tinted bed, and a shore line that widens with
  distance so it stays a pixel or so wide.
- **Atmosphere**: Rayleigh, Mie and ozone single scattering over a precomputed
  transmittance table, a cheap multiple-scattering term, aerial perspective on
  everything (a denser haze than before, so distant ranges fade into it), the
  sun's disc with limb darkening, stars at night. From above 60 km the march
  starts where the view ray enters the atmosphere, so the planet's limb and
  its blue rim are drawn against black space.
- **The orbital ring**: a band 120 km wide, 420 km up, around an axis through
  the planet's centre. Across it: two rails, lattice lanes at the edges, two
  open gaps crossed by trusses every half degree, dark solar lanes and plated
  lanes; along it, plates with seams and lights. It is lit by the sun and by
  the planet's day side, dark in the planet's shadow, hides the sun and stars
  where it is opaque, and shades the ground where it crosses the sun.
- **Clouds**: a cumulus layer (2.7 to 5.6 km) ray marched through a tileable
  Perlin-Worley volume (128^3, generated at start), its lookup warped by a
  ground-fixed noise and its second octave rotated, so the 14 km tile does not
  repeat visibly from high up: two octaves for the body,
  two finer ones eroding the edges (billows on top, wisps below), flat bases,
  denser cores rising higher. The march takes coarse steps through empty air
  and steps of 2% of the distance (120 to 800 m) inside clouds, so edges stay
  sharp at any distance; lit by a three-step light march toward the sun with
  three scattering orders, a strong forward lobe (silver lining) and a back
  lobe, a powder term and sky ambient that darkens toward the base. They drift
  with the wind and cast shadows on the ground.
- **Light transport**: per sample a soft sun shadow (a random point of the
  sun's disc), cloud shadow (from the first 8 km of the sun ray's crossing of
  the cloud layer, letting 14% through as diffuse light), and one bounce of
  sky or ground light; viewers' lights add point lighting.
- **The clock and the newest word**: Inter from a signed distance atlas.
  The clock (Pacific time, PDT or PST) is warm white type in the upper left,
  never occluded, over a soft shade that holds bright cloud behind it down.
  The word is drawn in its own `text_style`: lockup type above the clock,
  smoke puffs written across the sky, or solid letters standing on a ridge
  that cast shadows on the terrain. Viewers' lights are glowing spheres. All
  drawn exactly (no sampling) every frame; the landmark's shadows are in the
  path-traced samples.
- The sun's glare: three exponential lobes around the sun's direction, scaled
  by how much of the sun the camera sees (a terrain and cloud shadow ray from
  the camera, one per frame).
- A lift of the deepest shadows (up to 1.5x below about 4% of the exposed
  range), then ACES tone mapping (the fitted RRT and ODT), a warm-highlight, cool-shadow
  split tone, a vignette over the whole image, sRGB, a dither seeded by the
  whole-image pixel.

Convergence: exact accumulation while the camera holds; while it moves, each
frame averages itself with its last 7 frames reprojected through a guard band
the renderer draws around its rect, and pixels with few samples get an
edge-aware 5 x 5 filter over pixels drawn under the same rules, so moving
frames also match across tile edges. Views that would get under half a sample
per pixel (720p and 1080p at the default budget) draw a checkerboard, half or
a quarter of the pixels per frame, and fill the rest along the edge through
each gap. A cloud covering most of a pixel is reprojected at its own distance,
so near clouds keep their shape while the camera moves fast. Each stream encodes its tile plus 16
pixels of overlap and the client crops them, so a tile edge is never the edge
of an encoded picture. The rules are in PROTOCOL.md, "Sampling, accumulation
and determinism", and "crop".

## Layout

```
src/render.cu      kernels: tables, path tracer, accumulation, tone map, NV12
src/world.h        terrain and noise, shared by GPU and host (host keeps the eye above ground)
src/scene.cpp      scene JSON, camera splines and holds, tile geometry, per-frame parameters
src/pipeline.cpp   frame clock, subscribers, per-height encoders, sample budget, /health
src/server.cpp     HTTP/1.1 and WebSocket
src/nvenc.cpp      NVENC (libnvidia-encode loaded at run time; header from nv-codec-headers, MIT)
src/gpustats.cpp   NVML (libnvidia-ml loaded at run time)
tests/test_host.cpp  scene parsing, splines and holds, tile geometry, wire formats (no GPU)
tests/test_gpu.cpp   the seam test (GPU)
tools/             Python clients: smoke, measure, relaunch, mosaic, rawmosaic, seam_metric
```

## Build and run

The image builds everything in `nvidia/cuda:12.8.1-devel-ubuntu24.04` and
ships the two binaries on `ubuntu:24.04` with no NVIDIA libraries (CDI injects
the driver's). x86_64, `sm_75` (T4) SASS only.

```bash
make -C fleet/renderer image TAG=dev            # podman build --squash-all -> localhost/fcvm-renderer:dev
fcvm podman run --name r1 --kernel-profile gpu --gpu <sbdf> --publish 8080:8080 localhost/fcvm-renderer:dev
fcvm podman run --name t1 --kernel-profile gpu --gpu <sbdf> localhost/fcvm-renderer:dev test   # seam test
make -C fleet/renderer test-host                # host tests, needs only g++ and the NVENC header (NVCH=)
```

`--squash-all` and the `chown` in the Containerfile are there because rootless
fcvm exports an image's layers as the invoking user, which cannot read a
directory private to another container user (`/home/ubuntu`,
`/var/cache/apt/archives/partial`); fcvm fails with
`mkfs.ext4 -d failed: ... Permission denied`.

Other commands of the binary: `still OUT.ppm [--t T] [--h H] [--scene FILE]
[--spp N] [--frames N]` renders one frame (or a hold of N frames) to a file;
`bench` times render and encode per height.

Tools (Python 3 standard library, plus Pillow and the `ffmpeg` CLI for
decoding):

```bash
python3 tools/smoke.py HOST 8080                     # health, 60 frames, decode, GET /still
curl -o poster.jpg 'http://HOST:8080/still?h=540'    # this rectangle's next frame as JPEG
TLS=1 python3 tools/smoke.py HOST 443               # the same over HTTPS/WSS (TLS_CA=... to verify)
python3 tools/tlscost.py HOST 8080 443 8 540 hi 10   # CPU and delay per stream, plain vs TLS
Q=lo python3 tools/mosaic.py ...                     # any tool: Q=hi|std|lo picks the stream quality
python3 tools/measure.py HOST 8080 8 136 240 1080    # fps per height, moving camera
python3 tools/relaunch.py FCVM r1 <sbdf> TAG 5       # stop/start the VM, time to first frame
python3 tools/mosaic.py out 4 2 540 25 tools/scene-hold.json HOST1:8080 ... HOST8:8080
python3 tools/rawmosaic.py out 4 2 540 tools/scene-move.json HOST1:8080 ...   # unencoded tiles of one frame (GET /raw)
python3 tools/seam_metric.py out.png 4 2              # seam ratio at every tile boundary
```

## Measured (2026-10-23, g4dn.metal from the fleet host AMI, one T4 per fcvm VM)

The fps table, seams and shots are from round 7 (round 10's fps runs and
seam hashes match round 9's); the TLS numbers from round 6; the
stream-quality and subscriber numbers from round 4.

Moving camera, `tools/measure.py`, 8 s per row after 2 s to settle, default
budget (250,000 pixel-samples per frame over the drawn region), `q=std`; wall
tiles are a 1/16 x 1/9 rect of a 16:9 image, the detail view is one such tile
at 1080. Each moving pixel also averages up to 7 earlier frames; "checker" is
the checkerboard stride (1: every pixel draws).

| inner size | stream (with overlap) | fps | samples/pixel/frame | checker | render ms | encode ms |
|---|---|---|---|---|---|---|
| 146 x 146 (asked 136) | 178 x 178 | 32.5 | 5 | 1 | 11.5 | 0.82 |
| 180 x 180 | 212 x 212 | 30.2 | 3 | 1 | 9.6 | 0.71 |
| 240 x 240 | 272 x 272 | 30.0 | 2 | 1 | 8.8 | 0.75 |
| 360 x 360 | 392 x 392 | 30.0 | 1 | 1 | 7.9 | 1.12 |
| 1080 x 1080 detail view | 1112 x 1112 | 30.0 | 1 | 4 | 23.7 | 3.89 |
| 1280 x 720 whole image | 1280 x 720 | 30.0 | 1 | 4 | 27.6 | 2.54 |
| 1920 x 1080 whole image | 1920 x 1080 | 15.0 | 1 | 4 | 57.9 | 3.99 |

Two
more runs in one VM: 720p whole 30.0 and 30.0 fps (27.3 and 27.5 ms), the
detail view 30.0 and 30.0 (22.4 and 22.5 ms). In round 6 those two views sat
at the slot's edge (25.4 and 27.9 fps): round 7 ends the cloud march at a
transmittance of 0.1 (the rest of the cloud added from that step's light),
takes the light march every fourth fine step, steps 2% of the distance
inside clouds, and shortens the sun shadow march (32 steps), the bounce ray
(14 steps over 6 km) and the aerial perspective over a hit (8 steps). On the
measure scene's 720p frame the trace went from 29.6 to 27.2 ms, and on the
1080 detail tile from 95.2 to 82.8 ms (`still --seq 12`, per-kernel events).

### Grid-wide budget (round 8)

Eight VMs, one T4 each; `tools/budgetctl.py` (period 1 s) driving all eight;
the shots' JSON records `/health` read while streaming.

| tiles | camera | budget before -> after | samples/pixel | high quality | render ms (slowest tile) | fps (slowest tile) |
|---|---|---|---|---|---|---|
| 192 x 108 (100-GPU wall), sky block | moving, 20 s | 250,000 -> 840,896 | 5 -> 17 | off -> on | 5.5 -> 13.9 | 30.0 -> 29.3 |
| 192 x 108 (100-GPU wall), terrain block | moving, 40 s | 250,000 -> 500,000 | 5 -> 10 | off -> on | 13.9 -> 24.2 | 30.0 -> 30.0 |
| 480 x 540 (8-GPU wall) | moving, 40 s | 250,000 -> 707,107 | 1 -> 2 | off | 13.5 -> 19.4 | 30.0 -> 30.0 |

The slowest tile governs, and tiles differ a lot: on the 100-GPU wall a sky
tile's fit was 2 to 4 M pixel-samples and a terrain tile's 0.4 to 0.5 M. At
8-GPU size there is room for one more sample per pixel, not for the
high-quality path.

Unequal GPUs: four of eight VMs reported half their fit (`FIT_SCALE=0.5`);
the controller followed those four (fits 0.48 to 0.78 M against 0.82 to
1.15 M) and every tile drew the same budget at every slot. A moving frame of a
4 x 2 block of the 100-GPU wall, at 420,448 pixel-samples (9 samples per
pixel, high quality), stitched unencoded from the eight VMs equals the same
block drawn as one rect in another VM in all 165,888 pixels
(`shots/scale100-raw-tiles-vs-whole.png`). The seam test passes on three
GPUs with 21 checks, the new ones covering the high-quality path while the
samples per frame change mid-sequence and a high-quality hold in different
batches.

Round 9:

- **The flat near cloud.** Two-arm renders of the fast-move frame at 1080
  (12 frames of history; high quality at 16 spp and the fast path at 1 spp,
  stride 4), measuring the cloud crop's detail energy (the standard deviation
  of luminance minus a 3 px blur): base 5.76. Light march shortened near the
  eye 5.94; a finer erosion octave near the eye 5.86; fine steps of 10 or 30 m
  near the eye made the cloud disappear (the crop shows sky): with steps that
  fine the march spent its 64 to 96 fine steps without becoming opaque. The
  cloud is a deep region of thin density: where coverage is high the density
  remap rises slowly, so it takes kilometres to become opaque and reads as a
  uniform fog bank from inside or near the layer. Density now saturates at a
  third of it (`sat(3 d)`): 6.66, with shaded undersides
  (`shots/nearcloud-density-before-after.png`; the 8-VM fast shot's crop
  5.48 -> 6.67, `shots/fast-8gpu-r7-r9.jpg`). Clouds are denser and darker
  underneath everywhere, and a thin lens cloud at mid distance is grainier at
  1 spp.
- **Controller hysteresis.** Four of eight tiles slowed (`FIT_SCALE=0.5`), the
  long moving path on the 100-GPU terrain block, 2 minutes each: the round 8
  rule changed the budget 2 times (1.0 per minute, between 125,000 and
  148,651), the new rule 0 times (125,000 throughout).
- **Cost per path.** The fit now models a frame as a fixed part plus a cost per
  sample per path. On the 100-GPU terrain block, no slowed tiles, 60 s: every
  tile at 353,553 (7 samples per pixel, fast path; the slowest tile cannot
  afford the high-quality path at 8), render 12.8 to 15.0 ms against the
  23.3 ms target, 30.0 fps on every tile.
- **The detail view's 26.1 fps run (round 8).** Not reproduced in 11 runs:
  five with round 8's image in the order of `tools/measure.py`'s full table
  (three with eight VMs up, two with one), its batch order once more, and five
  with round 9's image, all 30.0. `measure.py` now records per row the slots
  the renderer skipped, the slots missing from the stream and the frame-time
  tail; round 9's detail rows: 1 skipped slot (the stream switch), no gaps,
  frames at most 27.6 to 29.6 ms. The 26.1 row has none of these, so whether it
  was the renderer skipping slots, the stream dropping frames for a slow
  client, or the client is not known.

Round 10, layout generations (`tools/retile.py`, four VMs):

- **A re-tile mid-stream.** Four tiles stream a moving camera as a 2 x 2 grid
  (layout 1, 270 px streams), then get layout 2, four columns, with
  `previous_rect` and `rect_from_frame` K 2.5 s ahead. Every tile's unencoded
  frame at K-1, stitched under the 2 x 2 layout, and at K+6, stitched under the
  four columns, equals the same frame drawn whole in another VM (12 frames of
  history for K-1, 7 for K+6) in all 518,400 pixels, in three runs
  (`shots/retile-old-new.jpg`, old layout left, new right). Each stream's last
  layout-1 frame is K-1 and its new header (`layout_generation` 2, 540 px
  tall, whole image still 540) comes with frame K, a keyframe, and no frame
  from K on is missing. The frame at K took 21.5 to 26.8 ms.
- **What the switch cost before.** Opening each new encoder on the render
  thread took 10 to 25 ms and tearing each old one down 29 to 245 ms, so the
  frame at K took 61 to 281 ms and tiles skipped 1 to 8 slots; frame K+6
  stitched under the new layout then differed from the whole frame in 222,469
  of 518,400 pixels. Encoders for a pending layout are now opened, and old
  ones torn down, on a second thread.
- **Scene then budget.** On a tile streaming at 500,000 pixel-samples per
  frame, a scene post followed 1 s later by the budget, `/health` read every
  30 ms: every frame it reported in between drew 500,000 from the schedule (31
  or 32 frames per check, same clock and new clock, two runs). With every scene
  clearing the schedule, as through round 9, the same check reports 30 of 32
  frames at the default 250,000.

- **A stream's first frames** (`tools/streamstart.py`, four VMs, a 2 x 2
  grid with the camera moving at 353,553 pixel-samples, four streams at
  270 px subscribing 70 ms apart): frames F to F+7, F the first frame every
  stream delivered, stitched unencoded, equal the same frames drawn whole
  (12 frames of history) in all 518,400 pixels, 8 of 8 frames in each of 4
  runs, with no frame missing from any stream. Round 10's renderer (`41c46e09`)
  skipped 9 to 12 slots after each stream started, so no tile drew F to F+7
  all; with the encoder opened off the render thread but no priming, the
  first frame all four drew differed in 162,911 pixels and the next four in
  99,194 to 131,044, and the slowest cell skipped every other slot.

Round 10, from the end-to-end run on eight T4s:

- **A renderer rejoining the wall** (`tools/rejoin.py`, four VMs, a moving
  2 x 2 grid, 270 px streams; one VM is killed and a fresh one started, sent
  the scene and the budget, then streamed). A fresh VM's GPU sits at its idle
  clock (585 MHz on a T4, against 1590; it takes about 1.5 s under load to
  reach full speed), so its first frames were slow: priming stopped after 7
  slots, the next slots were skipped, and frames F to F+6 of its stream
  differed from the frame drawn whole in about 55,000 of 129,600 pixels.
  Now a posted scene keeps the renderer drawing every slot (warming it), and
  priming goes on while its frames are still getting faster. At 707,107
  pixel-samples: first frame 0.28 s after subscribing, frames F to F+7 equal
  the whole frame's region in all pixels, in 3 runs; 2 runs skipped no slot
  in 5 s and 1 skipped one slot 1.6 s after the first frame (that budget is
  75% of a slot at full clock, above the controller's 70%). At 353,553: 2
  runs, 0 pixels different, no slot skipped.
- **The grid budget** (`tools/pathrun.py`: eight VMs as a 4 x 2 wall at 540 px
  tiles, fleet-control's directed camera with seed 1, `budgetctl.py` every
  2 s, 7 minutes). With the fit from the latest frames the slowest tile's
  fit swung 444 k to 716 k and the budget changed 7 times (420,448 and
  500,000 in turn). With the fit from a 180 s window's 95th percentile it
  stayed within 448 k to 489 k and the budget changed once (353,553 to
  420,448 at 225 s), 0 times in the last 3 minutes; the latest frames' fit on
  the same run still swung 447 k to 711 k. No frame took over 27.0 ms.

Round 12, text (PROTOCOL.md "Words, the clock and lights"):

- `shots/text-styles.jpg`: each message style (`words[].text_style`), a
  hold (128 samples) and a moving frame at 1080, and the 8-GPU wall's whole
  image at 360 (180 px tiles), the message "Hello from 8 GPUs".
  `shots/text-clock-cloud.jpg`: the clock and a lockup message on a path
  climbing through the cloud layer, at 1080 and at 360.
- Seam test on three GPUs: 33/33 each, the same hashes on all three. Each
  style stitches with 0 pixels different, moving and as a hold accumulated
  in different batches, and draws its word; the landmark letters darken the
  terrain in the path-traced samples (the overlay off); and over the cloud
  that passes behind the clock on the shots' moving path, its letters are
  brighter than the pixels around them by at least 0.3 (8-bit luminance /
  255). Without the clock's shade, 6 km out, that check reads 0.28; with
  round 11's clock, 0.17.
- fps with the camera moving, each style against no word (the clock only)
  on the same wall tile, the one its word falls in, two runs each, render ms:

  | style | 180 px tile | 720p whole | 1080 detail of that tile | 1080 whole |
  |---|---|---|---|---|
  | lockup | 4.5 (4.4) | 25.6 (25.5 to 25.6), 30.0 fps | 13.7 to 13.8 (13.6), 30.0 fps | 56.8 (56.5 to 56.8), 15.0 fps |
  | skywriting | 5.6 (5.4) | 25.9 (25.6), 30.0 fps | 14.2 (13.9 to 14.0), 30.0 fps | 57.0 to 57.2 (56.7 to 56.9), 15.0 fps |
  | landmark | 14.4 to 14.5 (14.1) | 26.8 to 26.9 (25.7), 30.0 fps | 58.4 to 58.5 (57.9), 15.0 fps | 58.3 to 58.6 (56.8 to 56.9), 14.9 fps |

  The landmark's tile is terrain the default budget draws at 15 fps in the
  detail view with or without the letters.

Stream quality at 180 px tiles (4 x 2 grid, 160 x 180 inner, 192 x 212
delivered; one moving frame unencoded and as the eight streams decode it,
cropped; `tools/seam_metric.py`):

| q | Mbit/s per tile (mean of 8) | seam ratio, decoded | largest increase over unencoded at one boundary |
|---|---|---|---|
| unencoded | | 0.51 to 1.57 | |
| `hi` | 3.99 | 0.58 to 1.61 | 0.13 |
| `std` | 0.86 | 0.64 to 1.60 | 0.43 |
| `lo` | 0.27 | 0.57 to 2.00 | 0.59 |

Eight `hi` subscribers at 540 px, one per VM on one host: 15 to 30 Mbit/s per
stream, every renderer at 29.9 fps with no slot missed after the first second,
render 10.2 to 13.0 ms, encode 1.4 to 2.0 ms per frame (std: 6.6 to 7.0 Mbit/s,
encode 1.1 to 1.9 ms).

Startup, process start to first encoded frame (`fleet-renderer startup`, three
runs each, in one VM, round 2): 1441 to 1663 ms as fcvm's main branch leaves
the GPU; 307 to 330 ms with `nvidia-persistenced` running in the guest.
Relaunch (round 2, cache-hit `fcvm podman run`): 6.15 to 6.28 s from the
command to the first frame at the client.

Seams (round 7): the seam test passes in VMs on three different T4s with the
same hashes (`fnv64 7bcd6010e2391cb4`, `99df125930c40c0f`, `81b124346d058065`).
A moving frame captured unencoded from eight VMs (`tools/rawmosaic.py`)
equals the same frame drawn whole in another VM in all 2,073,600 pixels, with
every pixel drawing and with stride-4 checkerboard frames. At 540 px tiles its
seam ratio is 0.75 to 1.32 unencoded and 0.81 to 1.40 as the eight `std`
streams decode it, cropped (largest increase from encoding 0.28). Holds,
decoded: golden hour 0.87 to 1.39, 80 km 0.65 to 1.55. In the fast-move shot
the clock puts a digit's edge on the middle top boundary (6.8 there).

TLS (round 6, one renderer VM, `tools/tlscost.py`, 15 s per arm, camera
moving, the same streams over `ws://` and then `wss://`; the renderer's CPU
from `/health cpu_s`, delay = arrival minus the frame's slot time):

| streams | per stream | renderer CPU, ws -> wss (% of one core) | added per stream | delay p50, ws -> wss |
|---|---|---|---|---|
| 8 x `hi` 540 px | 21.7 Mbit/s | 38.6 -> 44.0 | 0.67% | 15.4 -> 16.3 ms |
| 8 x `std` 180 px | 0.83 Mbit/s | 40.2 -> 40.5 | 0.04% | 14.2 -> 14.1 ms |
| 1 x `hi` 540 px | 21.7 Mbit/s | 39.3 -> 41.2 | 1.9% | 15.5 -> 16.0 ms |

A TLS connection's first request took 3.2 ms against 0.5 ms plain (one
stream, handshake included). A WSS stream carried on across a certificate
replacement (450 frames, 352 after the swap, largest gap 42 ms) and new
handshakes got the new certificate; a TLS 1.1 client is refused.

`GET /still?h=540` from the eight VMs, 12 s after the first request with no
stream running, in the 80 km hold: 363 to 382 samples per pixel, stitched in
`shots/still-high-8gpu.jpg`.

Shots stitched from eight VMs on eight GPUs are in `shots/`:
`mosaic-hold-8gpu.jpg` (golden hour, held), `mosaic-high-8gpu.jpg` (80 km
up, held), `mosaic-move-8gpu.jpg` (moving), `mosaic-fast-8gpu.jpg`
(`tools/scene-fast.json`, 1.2 km/s in the cloud layer), the two holds at 180 px tiles,
`raw-move-8gpu.jpg` and `decoded-move-8gpu.jpg` (one moving frame, unencoded
and decoded).
