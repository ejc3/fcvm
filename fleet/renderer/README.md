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
- **Words and the clock**: tubes along a stroke font, ray marched as 3D
  distance fields and laid out on the screen (the clock centred near the top,
  words in a grid below it, inside a safe area). Words are glowing glass; the
  clock is solid dark metal with a bright rim and a dark halo, readable over
  cloud and sky. Viewers' lights are glowing spheres. All drawn exactly (no
  sampling) every frame.
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

The fps table, seams, shots and TLS numbers are from round 6; the
stream-quality and subscriber numbers are from round 4.

Moving camera, `tools/measure.py`, 8 s per row after 2 s to settle, default
budget (250,000 pixel-samples per frame over the drawn region), `q=std`; wall
tiles are a 1/16 x 1/9 rect of a 16:9 image, the detail view is one such tile
at 1080. Each moving pixel also averages up to 7 earlier frames; "checker" is
the checkerboard stride (1: every pixel draws).

| inner size | stream (with overlap) | fps | samples/pixel/frame | checker | render ms | encode ms |
|---|---|---|---|---|---|---|
| 146 x 146 (asked 136) | 178 x 178 | 30.2 | 5 | 1 | 12.8 | 0.59 |
| 180 x 180 | 212 x 212 | 30.0 | 3 | 1 | 11.0 | 0.73 |
| 240 x 240 | 272 x 272 | 30.0 | 2 | 1 | 10.8 | 0.79 |
| 360 x 360 | 392 x 392 | 30.0 | 1 | 1 | 9.5 | 1.16 |
| 1080 x 1080 detail view | 1112 x 1112 | 27.9 | 1 | 4 | 27.8 | 4.15 |
| 1280 x 720 whole image | 1280 x 720 | 25.4 | 1 | 4 | 30.1 | 2.58 |
| 1920 x 1080 whole image | 1920 x 1080 | 12.2 | 1 | 4 | 62.4 | 4.45 |

The 720p whole image and the detail view sit at the slot's edge: render plus
encode is 31 to 33 ms of the 33.3 ms slot, and a frame over it costs a slot.
A second run in one VM gave 720p at 23.1 fps (29.6 ms) and the detail view
at 30.0 (26.1 ms). Round 5 measured 29.5 and 26.9 ms on another box. A lower
`budget` does not help these two: they already draw a quarter of the pixels
with one sample, the floor. Per frame, the trace is nearly all of it (720p,
moving, per-kernel events: trace 31 ms; sky 0.1, fill 0.2, resolve 0.2,
finish 0.4); within the trace the cloud march took 5.3 ms, the bounce ray
2.7 and the sun's shadow march 2.6 before both marches were shortened (20
bounce steps, round 4's shadow octaves), which took that frame's trace from
30.9 to 29.5 ms; the table above is after that change.

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

Seams (round 6): the seam test passes in VMs on three different T4s with the
same hashes (`fnv64 d29502859cc7552a`, `a6dabe4b727ac567`, `19c1b42fb6c2c616`).
A moving frame captured unencoded from eight VMs (`tools/rawmosaic.py`)
equals the same frame drawn whole in another VM in all 2,073,600 pixels, with
every pixel drawing and with stride-4 checkerboard frames. At 540 px tiles its
seam ratio is 0.75 to 1.26 unencoded and 0.81 to 1.35 as the eight `std`
streams decode it, cropped (largest increase from encoding 0.33, at a boundary
in flat forest). In the holds the centred clock puts a digit's edge on the
middle boundary, which reads 6.3 (golden hour) and 7.6 (80 km) there and 0.61
to 1.41 everywhere else; a 4x crop shows the digit, no line.

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
stream running, in the 80 km hold: 359 to 382 samples per pixel, stitched in
`shots/still-high-8gpu.jpg`.

Shots stitched from eight VMs on eight GPUs are in `shots/`:
`mosaic-hold-8gpu.jpg` (golden hour, held), `mosaic-high-8gpu.jpg` (80 km
up, held), `mosaic-move-8gpu.jpg` (moving), `mosaic-fast-8gpu.jpg`
(`tools/scene-fast.json`, 1.2 km/s in the cloud layer), the two holds at 180 px tiles,
`raw-move-8gpu.jpg` and `decoded-move-8gpu.jpg` (one moving frame, unencoded
and decoded).
