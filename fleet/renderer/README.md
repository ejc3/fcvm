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
  denser cores rising higher, and a soft fringe where the eroded edge thins
  out. The march takes coarse steps through empty air and steps of 2% of the
  distance (120 to 800 m) inside clouds, from a jittered start; lit by a
  four-step light march toward the sun with four scattering orders, each a
  dual-lobe Henyey-Greenstein phase (a strong forward lobe for the silver
  lining, a weak back lobe), the later orders reaching deeper so the inside of
  a cloud keeps a gradient; a powder term on rims facing the sun; sky light
  through the cloud above each point (two samples straight up), which darkens
  the underside of every lump against its top. They drift with the wind and
  cast shadows on the ground.
- **Weather** (the scene's `weather` keys, PROTOCOL.md "Weather"): cloud
  cover, towering cumulus (the cells of strongest coverage grow to 9.6 km),
  crepuscular rays (haze below the cloud base lit by the sun only through the
  gaps, 12 jittered steps over the first 40 km) and the snow line, eased
  between keys.
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
the renderer draws around its rect (over the sky, clamped to the frame's own
colors around each pixel, so a near cloud leaves no trail), and pixels with
few samples are denoised by an edge-aware a-trous filter over the radiance
divided by an albedo guide, guided by the first hit's distance and normal,
over pixels drawn under the same rules, so moving frames also match across
tile edges. Views that would get under half a sample
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
src/subscriber.cpp a subscriber's queue: header and keyframe rules, a slow reader dropped to the next keyframe
src/server.cpp     HTTP/1.1 and WebSocket
src/threads.h      starting a connection's threads when the system has none to give
src/nvenc.cpp      NVENC (libnvidia-encode loaded at run time; header from nv-codec-headers, MIT)
src/gpustats.cpp   NVML (libnvidia-ml loaded at run time)
tests/test_host.cpp  scene parsing, splines and holds, tile geometry, wire formats (no GPU)
tests/test_gpu.cpp   the seam test, and a moving frame's error against a held reference (GPU)
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
`bench` times render and encode per height. `quality OUTDIR --t T [--scene
FILE] [--h H] [--cols C --rows R]` draws a moving frame of a C x R grid, each
tile in a fresh renderer with its history, against the same pose held to 1024
samples, and prints each tile's samples per pixel, render ms and RMSE
(`BUDGET`, `MARGIN` as for `serve`); it writes the stitched frame and
reference as PPM. With `--encode SH` every moving frame also goes through the
stream path at height SH, as `serve` sends it (the tile plus its overlap, scaled
from the drawn frame), into `tile<I>-lo.h264`, `-std` and `-hi`, and the log
gives each stream's crop and the bit rate it used over the second half of the
frames; `tile<I>-samples.pgm` holds the samples each pixel resolved, history
included (capped at 255).

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

`tools/viewerload.cpp` (C++ and OpenSSL, built with the `g++` line in
"Viewers per renderer") opens many frame streams as the page does and reports
each stream's frame rate, frame delay, and frames dropped for a slow reader.

## The word's edges (round 17, 2026-10-30)

The owner's staging screenshot showed the lockup word with a hard light outline,
broken where a ridge cuts it. Two causes in `draw_type` and the overlay's
occlusion (`src/render.cu`):

- **The cut.** A word was hidden where the pixel's first hit was nearer than it,
  and that hit came from one jittered sample: the edge was a stair of whole
  pixels, with notches, that moved from frame to frame. Now, where the first
  hits of the pixel and its 8 neighbours lie on both sides of the word's
  distance, the ground is marched up to the word at four fixed points of the
  pixel, and the word shows through the share of them that meet no ground. A
  pixel whose own first hit is a cloud in front of the word hides it, as
  before (its cloud bit, from round 16, is checked before the ground). Every input is a
  whole-image pixel and the frame's camera, so tiles still match.
- **The outline.** The glow was added to what lay behind, falling off over 0.05
  em at up to 0.63 of the type's light: a bright rim hugging each letter, which
  the encoder kept as a hard outline. It is now drawn toward the letters' own
  color, so it never lights a pixel past them, over 0.12 em at up to 0.28.

Measured on a T4 (the seam test's new check 10c): a lockup word behind a ridge
in one tile of a 4 x 4 wall at h=432, 1 sample per pixel with history, the
camera all but still, drawn with and without the word. Pixels whose word effect
changed by more than 40 between two consecutive frames: 80 on main, 35 here
(bound 60). The rim, the mean effect 1 to 2 pixels outside the letters over dark
ground and sky less the mean 4 to 8 pixels out: 8.19 on main, 3.33 here (bound
6.0).

The march costs frame time on a tile where terrain cuts a word, and nothing
elsewhere: that tile, `still --seq 30`, mean of frames 20 to 29, one VM at a
time, two runs each: 8.63 and 8.75 ms on main, 11.07 and 11.12 ms here. Starting
the march at 0.9 of the nearest neighbour's hit, or 64 steps instead of 260,
saved 0.4 and 1.7 ms but left 53 and 67 pixels flickering. With the old glow, a
tent filter over the neighbours' first hits instead of the march cost nothing
and left 69 of main's 80.

`shots/text-cut-glow-r17.jpg`: before and after, unencoded.

## Speckle at cloud and shading edges (round 16, 2026-10-30)

The owner's staging screenshots (a 3460 x 1946 wall of 16 tiles) showed
single-pixel speckle along cloud edges and along shading edges on snow and
rock, which the encoder turned into blotches, and a flat, smeared look.
Measured which stage makes them, on one g4dn.metal from the fleet host AMI
(spot, no other job on it), each run in an fcvm VM on its own T4:
`fleet-renderer quality --h 432 --cols 4 --rows 4 --frames 48 --encode 432`
(the height that wall asks, the staging budget of 420,448 pixel-samples, so 1
sample per pixel), each tile's moving frame against the same pose held to 1024
samples, and the same frames through NVENC at lo, std and hi, decoded with
ffmpeg. Speckle: the share of pixels more than 12 (8-bit luminance) from the
median of their 3 x 3 neighbourhood.

- **The speckle is the renderer's.** Golden-hour clouds over terrain: 0.93%
  of the unencoded frame's pixels against 0.15% in the reference. Encoding
  does not add it (lo 0.67%, std 0.80%, hi 0.88%); it blurs part of it into
  blotches. 74% of the speckle was in the 7.5% of pixels that resolved fewer
  than 8 frames of history (`tile<I>-samples.pgm`), where the speckle rate was
  9.2% against 0.26% elsewhere.
- **Two filters skipped exactly those pixels.** Where a cloud covers a pixel
  over land, its first hit is the cloud at its transmittance-weighted depth,
  which one sample places hundreds of metres differently from its neighbours
  and from the frame before, and at the cloud's edge the first hit flips
  between the cloud and the ground. The reprojection's 4% distance rule
  dropped those pixels' history, and the denoiser's tangent-plane stop (a
  footprint of metres) gave them no neighbours. On terrain, the denoiser's
  normal stop (cosine to the 64th) cut its weights at every turn of the
  detail normals, so shading edges on rock and snow kept their raw samples.
- **The flat look is the page's bandwidth rule.** At 3460 x 1946 each tile is
  486 device pixels tall and asks `h=432`; 16 tiles at std need 135 Mbit/s,
  over the page's 80 Mbit/s budget, so every tile streams at lo (0.25 bits per
  pixel, 2.7 Mbit/s). Not changed here.

What changed (`src/render.cu`): a pixel whose first hit is a cloud carries a
bit in its packed normal. The denoiser treats it like the sky (one surface,
stopped only by luminance). Reprojection does not hold it, or an earlier
frame's cloud texel, to the 4% distance rule, and clamps that history to this
frame's 3 x 3 colors of the same hit kind, as it already did for the sky. The
denoiser's normal stop is the cosine to the 8th. A stream taller than the
drawn frame is scaled up bilinearly: the area filter's box, narrower than a
drawn pixel, picked one drawn pixel per stream pixel and magnified the frame
into squares (an opened tile asking 720 while its frame is drawn at a wall
height).

| scene (t 5 of the flight) | speckle, unencoded, before -> after (reference) | PSNR / SSIM against the reference, before -> after | decoded lo against the unencoded frame |
|---|---|---|---|
| golden-hour clouds over terrain, from 7 km (time 0.78) | 0.925% -> 0.193% (0.154%) | 32.88 / 0.949 -> 33.86 / 0.961 | 37.19 -> 38.73 dB |
| sunset cloud deck from 6.3 km (time 0.792) | 0.835% -> 0.126% (0.085%) | 34.00 / 0.953 -> 35.97 / 0.968 | 36.95 -> 39.04 dB |
| snow and rock under a low sun (time 0.79) | 0.213% -> 0.128% (0.105%) | 39.79 / 0.962 -> 40.02 / 0.967 | 39.28 -> 39.86 dB |

Decoded at lo, the speckle went from 0.67% to 0.12% (clouds), 0.66% to 0.08%
(cloud deck) and 0.14% to 0.09% (snow), at the same rate (2.7 to 2.8 Mbit/s
per tile): a cleaner frame also costs the encoder less.
Render time is unchanged: one VM at a time on one GPU, two runs each, the
mean of the 16 tiles' mean frame was 17.84 and 17.90 ms before, 17.90 and
17.89 after (clouds), 10.22 and 10.26 against 10.29 and 10.31 (snow), 15.98
and 16.02 against 16.02 and 16.05 (cloud deck). No frame of delay is added.

Variants measured on the same frames, 24 frames each (speckle, clouds /
snow; this change 0.19% / 0.13%): the cloud bit in the denoiser only 0.60% /
0.21%; the normal stop at ^8 only 0.87% / 0.13%; with the rest, the normal
stop at ^4 0.19% / 0.13%, the luminance stop at 3 or 6 standard deviations
0.20% / 0.14% and 0.20% / 0.13%; the tangent-plane stop at 2 footprints only
0.86% / 0.20%.

`shots/speckle-before-after-r16.jpg`: crops of the three scenes decoded at
lo, reference, before and after.

The seam test passes on a T4 (90 checks). Its new checks: speckle in a
moving frame of a cloud deck (1.46% before, 0.64% after, bound 1.0%) and of
snow and rock (0.48%, 0.31%, bound 0.40%), and the share of the luma's change
along rows that falls at drawn pixels' edges in a stream 2.5 times taller
than its frame (0.70 nearest, 0.44 bilinear, bound 0.6); the three fail on
main's renderer. Its moving-flight check reads rmse 5.93 and grain 3.60
(5.95 and 3.95 before).

## Fluffier clouds and weather (round 15, 2026-10-27)

Why the clouds read flat on the staging wall (the owner's phone video, and
the same frames at 1024 samples): every lump was a hard surface lit in flat
patches. Density reached its full 60 per km at a third of the remapped noise,
so a ray became opaque inside one fine step and was lit at one point; the sun's
optical depth there jumped from 0 to over 5 within about 80 m, and with three
scattering orders that fell off quickly the shading was either lit or one
flat shade; the powder term used the local density, which is 60 per km
everywhere inside, so it changed nothing; sky light depended on the height in
the layer, not on the cloud above the point; and where the coarse march's
back-up stopped at the layer's entry every pixel started its fine steps at
the same distance, so flat bases showed steps. Not the denoiser: the held
1024-sample reference shows the same patches, and a tighter luminance stop
for the sky (0.4 of `DENOISE_SIGMA_L`) changed nothing visible, so it is not
in this change.

What changed (`src/render.cu`): density is `sat(4 d)^2` (a fringe instead of
a step); four scattering orders, each deeper (`b` x 0.35) with a dual-lobe
phase computed once per ray; a fourth light step (1.46 km toward the sun);
powder from the sun's optical depth, faded out toward the sun; sky light
through two samples of the cloud above each lit point; a jittered start for
the fine steps. The scene gains `weather` (cover, towers, rays, snow line).

Measured on one g4dn.metal (spot, nothing else on it: no other VM, the busiest
process at 0.3% CPU before the run), each run in an fcvm VM on its own T4.
`fleet-renderer quality` for the staging wall, a 4 x 4 grid of 640 x 360
tiles (`h=360`) at 420,448 pixel-samples, 24 frames of history, against the
same pose held to 1024 samples; flights built from the director's viewpoints
(a 7 s hold, then the flight). Before is main at `2e2181cf`, after this
change, each pair on the same GPU. rmse: 8-bit RGB over the whole image.

| view (scene time) | rmse before -> after | slowest tile, render ms |
|---|---|---|
| high-lakes -> peak-seam (t 16 of 32) | 3.95 -> 3.83 | 14.8 -> 15.5 |
| peak-seam -> sunset-ridge (t 13.5 of 27) | 4.55 -> 4.83 | 13.0 -> 13.6 |
| dawn-valley -> peak-close (t 16 of 32) | 5.00 -> 5.56 | 13.8 -> 14.1 |
| under the layer near sunset-ridge, looking up (t 15) | 4.75 -> 4.32 | 15.7 -> 16.4 |
| from the coast, time 0.33 (t 15) | 5.85 -> 4.19 | 16.2 -> 19.5 |

With weather (after only, slowest tile): towers 1 on the first flight 18.4 ms,
towers 1 from the coast 19.9, rays 1 toward a low sun 17.9 and 18.9, snow line
0.6 km on the third flight 13.7, cover 0.15 on the first flight 14.0. Every
case stays under the controller's 23.3 ms target at 1 sample per pixel. Without
longer fine steps in a towering layer (x 1.6) the coast's towers took 25.5 ms
and ran out of fine steps before crossing the deeper clouds.

`fleet-renderer bench` (the built-in golden-hour scene, moving camera, 420,448
pixel-samples), render ms per view:

| view | before | after |
|---|---|---|
| wall-like tile, 360 x 360 (2 samples per pixel) | 12.05 | 14.86 |
| whole image 1280 x 720 (stride 4) | 36.99 | 42.27 |
| whole image 1920 x 1080 (stride 4) | 67.93 | 76.65 |

Computing the four orders' phase once per ray instead of at every fine step
took the 720p view from 45.0 to 42.2 ms; one sky-light sample instead of two
and a three-step light march saved nothing measurable.

`shots/clouds-before-after-r15.jpg`: cloud crops of the third flight, the
coast and under the layer, before (left) and after (right), moving frames as
the wall draws them. `shots/weather-r15.jpg`: the first flight with the
default weather, towers 1 and cover 0.15; the third flight with the snow line
at 0.6 km; rays toward a low sun; towers from the coast.

The seam test passes on two T4s with the same hashes (87 checks); its new
check draws a weather sequence easing from the defaults to towers, cover
0.62, rays and a 0.8 km snow line, moving with history and as a hold in
different batches, and requires every tile to match the whole image and the
weather to change more than a quarter of the pixels. Its moving-flight check
reads rmse 5.95 and grain 3.95 (6.10 and 4.45 before).

## Viewers per renderer (round 14, 2026-10-27)

Every viewer subscribes to every tile, so the wall serves as many viewers as
its weakest tile. Measured in isolation, not on staging:

- **fcvm half:** one g4dn.metal (on demand) from the fleet host AMI, eight
  renderer VMs started as the agent starts them (`--network routed` at the
  slot's IPv6 address, TLS in the VM on 443, the IPv4 relay published on the
  host, 2 vCPUs and 1 GiB), each drawing one 1/4 x 1/4 rect of the fcvm half of
  a 4 x 4 wall, camera moving.
- **EC2 half:** one g4dn.xlarge from the fleet tile AMI (the renderer behind
  nginx's TLS on 443), drawing one such rect.
- **Viewers:** `tools/viewerload.cpp` on two c8g.24xlarge in another region,
  subscribing over IPv6 as the page does. The page's asks for this wall
  (`fleet/web/core/layout.ts`, `bandwidth.ts`): a 1920 x 1080 desktop asks
  `h=270`, `q=std` (496 x 286 delivered, 2.4 to 3.7 Mbit/s per tile by its
  content); a phone, 390 x 844 at DPR 3 in portrait, asks `h=180`, `q=std`
  (336 x 196, 0.8 to 1.8 Mbit/s).

```bash
g++ -O2 -std=c++17 -pthread -o viewerload tools/viewerload.cpp -lssl -lcrypto
./viewerload --epoch-ms <the scene's epoch_unix_ms> --conns 500 --h 270 --warmup 8 --secs 15 \
  'wss://[<slot address>]:443/frames' ...       # each URL gets --conns viewers
```

A step passes when every viewer of every tile gets at least 29.5 frames a
second, no frame is dropped for a slow reader (no gap in frame numbers, no
repeated stream header), and the frame delay's 95th percentile is at most 50 ms
above the tile's fastest frame in the same run (absolute delays carry each VM's
clock offset, so delays are compared within a tile). 15 s per step after 5 to 8
s to settle. No step dropped a frame for a slow reader, so the columns below are
frame rate and delay.

**Viewers of the whole wall**, at that criterion:

| | desktop (`h=270`) | phone (`h=180`) |
|---|---|---|
| fcvm half, as deployed (2 vCPUs per VM) | 500 (750 fails) | 500 (750 fails) |
| fcvm half, 4 vCPUs per VM and RPS (below) | 750 misses by one tile at 29.47 fps (1000 fails) | 750 (1000 fails) |
| EC2 half (g4dn.xlarge) | 750 (1000 fails) | not measured |
| coordinator (0.5 vCPU, 1 GiB) | about 5,000 (memory) | same |

So the wall as deployed serves about 500 simultaneous viewers, desktop or
phone; the fcvm half is the first to fail.

All eight fcvm VMs at once, as deployed:

| viewers per tile | Mbit/s per viewer per tile | host NIC | slowest tile, fps | delay p95 over fastest | busiest VMM thread | busiest renderer process |
|---|---|---|---|---|---|---|
| desktop 250 | 2.98 | 6.4 Gbit/s | 29.93 | 14 ms | 29% | 67% |
| desktop 500 | 2.97 | 12.7 Gbit/s | 29.60 | 27 ms | 55% | 98% |
| desktop 750 | 2.63 | 16.8 Gbit/s | 20.53 | 485 ms | 100% | 130% |
| phone 500 | 1.48 | 6.3 Gbit/s | 29.93 | 25 ms | 39% | 92% |
| phone 750 | 1.45 | 9.3 Gbit/s | 28.33 | 50 ms | 65% | 119% |
| phone 1000 | 1.12 | 9.6 Gbit/s | 14.93 | 64 ms | 81% | 122% |

**The first bottleneck is the VM's two vCPUs, with the VMM's event-loop
thread close behind.** One VM (an interior tile, desktop asks):

| viewers | fps | slots missed in 14 s | render thread's frame time, p50 | renderer process | VMM thread |
|---|---|---|---|---|---|
| 1 | 30.0 | 0 | 13.7 ms | 39% | |
| 250 | 29.93 | 0 | 17.1 ms | 61% | |
| 500 | 29.87 | 1 | 20.9 ms | 88% | |
| 750 | 27.93 | 28 | 26.8 ms | 114% | 77% |
| 1000 | 16.80 | 183 | 34.6 ms | 92% | 71% |

The render thread draws, encodes and queues each frame for every viewer, and
each viewer has a writer thread and a reader thread. Its frame time grows by
about 20 us per viewer (13.7 ms alone, 34.6 ms at 1000), and once it passes the
33.3 ms slot the renderer skips every other slot (15 fps). Most of that is
waiting, not queueing work: the guest's scheduler counters at 1000 viewers had the render
thread runnable but waiting for a CPU 26% of the time (1.32 s of 5 s; the
2,010 renderer threads together waited 431 s). Waking all writers with one
futex call per frame, instead of one per viewer, did not move it (16.6 fps at
1000 viewers with RPS, against 15.1 without the change), so that change is not
here. Heavier tiles (terrain, longer render) fail first, and phones, at half
the bytes, fail at the same count.

The VMM's one event-loop thread carries everything the guest sends: a frame
written to the TAP crosses the namespace bridge, the veth, routing and the
software segmentation ENA leaves to the kernel (ENA has no TSO) inside that
`write`. A bpftrace profile of a 4-vCPU VM at 1000 viewers had it 98% busy,
nearly all in the kernel (`tun_get_user` 14%, `tcp_gso_segment` 10%,
`ena_start_xmit` 7%). It
caps a VM at about 2.8 Gbit/s (desktop 750, all eight VMs: two VMM threads at
98 to 100%, delay p95 485 ms over the fastest frame). fcvm now spreads the
TAP's receive processing over every host CPU (RPS, `veth::create_tap_in_ns`),
so the VMM's `write` hands each packet to another CPU:

| one VM, desktop | fps | delay p95 over fastest | VMM thread | throughput |
|---|---|---|---|---|
| 2 vCPUs, 750 viewers | 27.93 | 49 ms | 77% | 2.62 Gbit/s |
| 2 vCPUs, 750 viewers, RPS | 29.53 | 14 ms | 43% | 2.75 Gbit/s |
| 4 vCPUs, 1000 viewers | 18.80 | 622 ms | 98% | 2.62 Gbit/s |
| 4 vCPUs, 1000 viewers, RPS | 29.93 | 21 ms | 69% | 3.71 Gbit/s |
| 4 vCPUs, 1500 viewers, RPS | 19.87 | 31 ms | 63% | 3.69 Gbit/s |

With RPS and 2 vCPUs the delays stay low but the render thread still starves
(all eight VMs, desktop 750: slowest tile 15.0 fps, delay p95 28 ms over the
fastest), so the agent's VMs need 4 vCPUs as well (`--fcvm-arg=--cpu
--fcvm-arg=4` in the agent's arguments; the vCPU count is part of the snapshot,
so the host AMI's warmed store needs the same). At 4 vCPUs one VM tops out at
about 30,000 frame sends a second (1000 viewers at 30 fps, 1500 at 20, 2000 at
15) with neither the VMM thread nor the vCPUs saturated.

All eight VMs at once, 4 vCPUs each:

| viewers per tile | RPS | Mbit/s per viewer per tile | host NIC | slowest tile, fps | delay p95 over fastest | busiest VMM thread |
|---|---|---|---|---|---|---|
| desktop 750 | no | 2.68 | 17.1 Gbit/s | 20.33 | 799 ms | 100% |
| desktop 750 | yes | 3.00 | 19.2 Gbit/s | 29.47 | 41 ms | 62% |
| desktop 1000 | yes | 2.38 | 20.3 Gbit/s | 19.93 | 249 ms | 93% |
| phone 750 | yes | 0.76 | 4.9 Gbit/s | 29.73 | 50 ms | 44% |
| phone 1000 | yes | 1.42 | 12.1 Gbit/s | 27.00 | 67 ms | 65% |

The phone 750 step ran at a point of the flight whose tiles encoded at half
the usual rate; the per-viewer cost the renderer pays is per frame more than
per byte, so it is listed as measured.

**The IPv4 relay** (the corner tile through the host's IPv4 address and fcvm's
relay, desktop asks, 1.6 Mbit/s per viewer for that sky tile): 750 viewers
passed (29.80 fps, delay p95 15 ms over the fastest), and the relay's fcvm
process used 44% of one core at 1.17 Gbit/s, spread over its tokio workers.
Not a bottleneck at these counts.

**The EC2 half** (g4dn.xlarge, desktop asks, about 3.6 Mbit/s per viewer):

| viewers | fps | delay p95 over fastest | renderer | nginx | instance CPU (4 vCPUs) | NIC |
|---|---|---|---|---|---|---|
| 250 | 30.00 | 15 ms | 55% | 54% | 28% | 0.96 Gbit/s |
| 500 | 30.00 | 28 ms | 81% | 120% | 53% | 1.93 Gbit/s |
| 750 | 29.67 | 40 ms | 111% | 193% | 79% | 2.85 Gbit/s |
| 1000 | 27.60 | 104 ms | 139% | 222% | 93% | 3.52 Gbit/s |
| 1500 | the renderer terminated | | | | | |

CPU binds it: nginx's TLS proxy takes more than the renderer. The network is
far from the instance's 5 Gbit/s baseline. At 1500 the renderer died: podman's
default pids limit (2048) refused a thread at about the 1,020th viewer, the
uncaught `std::system_error` terminated the process, and every viewer of the
tile lost its stream. The renderer now refuses that one connection instead
(`start_thread`, `src/threads.h`), and the tile's unit raises the limit
(`--pids-limit=32768`). With both, 1500 viewers: the renderer kept running at
19.7 fps (CPU), and nginx refused 36 handshakes ("worker_connections are not
enough": the stock 768 per worker, two per proxied stream); with the fix and
the old limit, 1,032 connections were upgraded and the rest refused, a few of
the upgraded ones got no frames (their writer thread was refused after the
upgrade), the others streamed at 29.4 fps on average, and the renderer did not
restart.

**The coordinator** (`fcvm fleet coord --ec2 fake`, so 16 places, at the
Fargate task's 0.5 vCPU through a cgroup CPU quota, on Graviton; without the
`cloudflared` sidecar that shares the task): N `/live` connections, one of them
dropping a light every 2 s, timed from the intent to each viewer's snapshot at
or past its version.

| viewers | snapshot | to every viewer, p50 / p95 / max | sent | resident memory |
|---|---|---|---|---|
| 1,008 | 9.6 KB, 0.65 a second | 45 / 57 / 60 ms | 51 Mbit/s | not read |
| 2,512 | 11.7 KB, 0.55 a second | 34 / 51 / 57 ms | 135 Mbit/s | 408 MB |
| 5,008 | 13.7 KB, 0.70 a second | 73 / 102 / 145 ms | 351 Mbit/s | 793 MB (peak 844 MB) |

Memory binds first: about 160 KB per connection, so the 1 GiB task, which
`cloudflared` shares, holds about 5,000 viewers, ten times what the fcvm half
serves. CPU averaged 11% of a core over the 5,008 run (the quota is 50%). Connecting 5,008 viewers at
once took up to 29 s (every join is a new version sent to everyone).

## Moving frames at the wall's size (round 13, 2026-10-26)

One g4dn.metal, each run in an fcvm VM on its own T4. The staging wall is a
4 x 4 grid of 640 x 360 tiles (a 2560 x 1440 image, `h=360`); its grid
budget was 420,448 pixel-samples, which gives each tile's 321,456 drawn
pixels 1 sample per pixel (2 samples need 500,000, and at 707,107 the slowest
tile took 27.0 ms against the controller's 23.3 ms target, so the budget
cannot rise at this size). `fleet-renderer quality` on three flights of the
directed camera, each frame the 16 tiles drawn in fresh renderers with 24
frames of history, against the same pose held to 1024 samples. rmse: 8-bit
RGB over the whole image; grain: RMS of the luminance error minus its 5 x 5
mean; SSIM of luminance, 7 x 7 windows.

| flight, frame | rmse before -> after | grain | SSIM | slowest tile, render ms |
|---|---|---|---|---|
| high-lakes -> peak-seam, t 16 (9 s into 18) | 7.04 -> 4.24 | 4.42 -> 3.14 | 0.929 -> 0.957 | 13.1 -> 15.5 |
| peak-seam -> sunset-ridge, t 13.5 (6.5 of 13) | 15.43 -> 4.55 | 6.49 -> 3.10 | 0.821 -> 0.939 | 11.5 -> 12.7 |
| dawn-valley -> peak-close, t 16 (9 of 18) | 21.93 -> 5.17 | 5.90 -> 3.24 | 0.836 -> 0.941 | 12.3 -> 13.6 |

Of the rmse, the sky's history clamp alone took the second flight from 15.43
to 7.78 and the third from 21.93 to 7.13 (the trail of near clouds); the
denoiser alone (an earlier 5 x 5 variant, without the clamp) 7.04 -> 6.61 on
the first. Drawn pixels
per tile rise from 321,456 to 354,944 (the denoiser's 16-pixel reach), still 1
sample per pixel at 420,448. The denoiser itself (5 passes over the resolved
region) took 0.44 ms on a wall tile and 1.15 ms on a whole 720p frame.
Larger views pay more: the whole 720p image (stride 4) 28.1 -> 30.7 ms render,
a 1080 detail tile 27.9 -> 32.3 ms, a claimed tile 720 tall at 420,448 34.8 ->
40.0 ms. `shots/moving-denoise-before-after.jpg` (terrain of the first two
flights) and `shots/moving-clouds-before-after.jpg` (the third flight's sky):
before, after, reference. The seam test passes on two T4s with the same
hashes (36 checks), and its new check pins the improvement: on the second
flight at 960 x 540, 1 sample per pixel, rmse 15.34 and grain 7.02 before,
6.10 and 4.45 after (bounds 9.0 and 5.5; without the clamp rmse is 14.21,
without the denoiser grain is 9.94).

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

The height a tile draws (round 15, 2026-10-27). A renderer draws one frame
per slot at the tallest height its streams ask and scales it to the others.
An opened 16:9 tile asks 720 against the wall's 432 (2.65x the drawn pixels).
On staging, slot-6 (terrain) with one 720 subscriber: render 29 ms, frame
36 ms at the lowest budget (stride 4, 1 sample), 15 fps, 374 slots skipped in
45 s, and the page, which shows every tile at one scene time, drew the whole
wall at that tile's rate. Its wall frames at 432 took 13 ms. A renderer now
draws at the tallest asked height whose frame fits its slot (`HeightGuard`,
budget.h; PROTOCOL.md `/health` `drawn_h`), and a taller stream is scaled up
from it.

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
