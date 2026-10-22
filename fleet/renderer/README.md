# fleet/renderer

The program in every GPU VM of the fleet wall: it path-traces its rectangle of
one shared procedural planet with CUDA, encodes it with NVENC and streams it
over WebSocket. The interface (`POST /scene`, `GET /frames`, `GET /health`) is
in [`../PROTOCOL.md`](../PROTOCOL.md); the design is in
[`../../docs/fleet/ARCHITECTURE.md`](../../docs/fleet/ARCHITECTURE.md).

## The scene

A planet seen from a few hundred metres to a few kilometres up, every pixel a
pure function of `t`, the scene and its whole-image coordinates:

- **Terrain**, ray marched: continents and seas, mountain massifs of eroded
  fbm (octaves damped by the accumulated slope) with domain-warped ridges,
  more octaves for shading normals as the pixel footprint shrinks. The
  large-scale fields (land, massif, warp) are baked once at start into a
  filtered 2048 x 2048 texture over +-512 km, which halves the cost of a march
  step; outside it they are computed directly. Shading reads the same fields
  through a quadratic B-spline, because the hardware's bilinear filter is not
  smooth enough for normals.
- **Materials** by height, slope and noise: rock, meadow, dry grass, forest,
  beach, snow on flatter high ground, with albedo detail down to tens of
  metres; Lambert plus GGX.
- **Water** at sea level with animated wave normals, Fresnel reflection of
  terrain, sky and clouds, sun glitter, depth-tinted seabed and shore foam.
- **Atmosphere**: Rayleigh, Mie and ozone single scattering over a precomputed
  transmittance table, a cheap multiple-scattering term, aerial perspective on
  everything, the sun's disc with limb darkening, stars at night.
- **Clouds**: a cumulus layer (2.7 to 5.6 km) ray marched through a tileable
  Perlin-Worley volume (128^3, generated at start) with two finer octaves
  eroding the edges (billows on top, wisps below), flat bases and rounded tops;
  lit by a six-step light march toward the sun with three scattering orders, a
  strong forward lobe (silver lining) and a back lobe, a powder term and sky
  ambient that darkens toward the base; they drift with the wind and cast
  shadows on the ground.
- **Light transport**: per sample a soft sun shadow (a random point of the
  sun's disc), cloud shadow, and one bounce of sky or ground light; viewers'
  lights add point lighting.
- **Words and the clock**: glowing glass tubes along a stroke font, ray
  marched as 3D distance fields, lit by the sun and the sky with a specular
  highlight, an emissive core, a bright rim and a halo; viewers' lights are
  glowing spheres. All drawn exactly (no sampling) every frame.
- ACES-like tone mapping, sRGB, a dither seeded by the whole-image pixel.

Convergence: exact accumulation while the camera holds; while it moves, each
frame averages itself with its last 7 frames reprojected through a guard band
the renderer draws around its rect, and pixels with few samples get an
edge-aware 5 x 5 filter over pixels drawn under the same rules, so moving
frames also match across tile edges. The rules are in PROTOCOL.md, "Sampling, accumulation and determinism".

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
tools/             Python clients: smoke, measure, relaunch, mosaic, rawmosaic
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
python3 tools/smoke.py HOST 8080                     # health, 60 frames, decode
python3 tools/measure.py HOST 8080 8 136 240 1080    # fps per height, moving camera
python3 tools/relaunch.py FCVM r1 <sbdf> TAG 5       # stop/start the VM, time to first frame
python3 tools/mosaic.py out 4 2 540 25 tools/scene-hold.json HOST1:8080 ... HOST8:8080
python3 tools/rawmosaic.py out 4 2 540 tools/scene-move.json HOST1:8080 ...   # unencoded tiles of one frame (GET /raw)
```

## Measured (2026-10-22, g4dn.metal, one T4 per fcvm VM, driver 580.196.07)

Moving camera over the mountains, `tools/measure.py`, 8 s per row after 2 s
to settle, default budget (500,000 pixel-samples per frame); wall tiles are a
1/16 x 1/9 rect of a 16:9 image. Each moving pixel also averages up to 7
earlier frames, so its effective sample count is up to 8 times the per-frame
one.

| stream | fps | samples/pixel/frame | render ms | encode ms |
|--------|-----|---------------------|-----------|-----------|
| 146 x 146 (asked 136) | 30.0 | 13 | 12.7 | 0.55 |
| 180 x 180 | 30.0 | 9 | 12.8 | 0.84 |
| 240 x 240 | 30.0 | 5 | 13.0 | 0.80 |
| 360 x 360 | 30.0 | 2 | 10.5 | 1.08 |
| 1280 x 720 whole image | 30.0 | 1 | 26.6 | 2.76 |
| 1920 x 1080 whole image | 15.0 | 1 | 55.8 | 4.56 |

Startup, process start to first encoded frame (`fleet-renderer startup`, three
runs each, in one VM): 1441 to 1663 ms as fcvm's main branch leaves the GPU
(CUDA ready at 1349 to 1557 ms); 307 to 330 ms with `nvidia-persistenced`
running in the guest (CUDA ready at 230 to 233 ms).

Relaunch (`tools/relaunch.py`, cache-hit `fcvm podman run`, four runs): 6.15
to 6.28 s from the command to the first frame at the client, of which the
renderer's own process start to first frame sent is 1.90 to 1.97 s.

Seams: the seam test passes in VMs on three different T4s with the same
hashes. A moving frame captured unencoded from eight VMs (`tools/rawmosaic.py`)
equals the same frame drawn whole in another VM in all 2,073,600 pixels. Shots
stitched from eight VMs on eight GPUs are in `shots/`.
