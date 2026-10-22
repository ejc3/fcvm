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
  step; outside it they are computed directly.
- **Materials** by height, slope and noise: rock, meadow, dry grass, forest,
  beach, snow on flatter high ground; Lambert plus GGX.
- **Water** at sea level with animated wave normals, Fresnel reflection of
  terrain, sky and clouds, sun glitter, depth-tinted seabed and shore foam.
- **Atmosphere**: Rayleigh, Mie and ozone single scattering over a precomputed
  transmittance table, a cheap multiple-scattering term, aerial perspective on
  everything, the sun's disc with limb darkening, stars at night.
- **Clouds**: a cumulus layer (2.3 to 4.9 km) ray marched through a tileable
  Perlin-Worley volume (128^3, generated at start), lit by a light march toward
  the sun with three scattering orders, dual-lobe phase and sky ambient; they
  drift with the wind and cast shadows on the ground.
- **Light transport**: per sample a soft sun shadow (a random point of the
  sun's disc), cloud shadow, and one bounce of sky or ground light; viewers'
  lights add point lighting.
- **Overlays**: the UTC clock and viewers' words as glowing stroke-font
  letters, and viewers' lights as glowing spheres, drawn exactly (no sampling)
  every frame.
- ACES-like tone mapping, sRGB, a dither seeded by the whole-image pixel.

Convergence: exact accumulation while the camera holds, reprojected history
while it moves; the rules are in PROTOCOL.md, "Sampling, accumulation and
determinism".

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
tools/             Python clients: smoke, measure, relaunch, mosaic
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
```

## Measured (2026-10-22, g4dn.metal, one T4 per fcvm VM, driver 580.196.07)

Moving camera over the mountains, `tools/measure.py`, 8 s per row after 2 s
to settle; wall tiles are a 1/16 x 1/9 rect of a 16:9 image:

| stream | fps | samples/pixel/frame | GPU ms/frame (render) | encode ms |
|--------|-----|---------------------|-----------------------|-----------|
| 146 x 146 (asked 136) | 30.0 | 60.6 | 23.4 | 0.40 |
| 180 x 180 | 30.0 | 57.8 | 23.3 | 0.53 |
| 240 x 240 | 30.0 | 38.5 | 23.5 | 0.49 |
| 360 x 360 | 30.0 | 20.0 | 23.8 | 0.62 |
| 1280 x 720 whole image | 30.0 | 1 | 23.9 | 1.74 |
| 1920 x 1080 whole image | 15.0 | 1 | 51.1 | 3.74 |

Relaunch (`tools/relaunch.py`, cache-hit `fcvm podman run`, four runs): 6.07
to 6.27 s from the command to the first frame at the client. Inside that, the
renderer's own process start to first frame sent is 1.91 to 1.97 s, of which
CUDA initialization is 1.51 to 1.56 s (the GPU is not held initialized between
fc-agent's `nvidia-smi` and the renderer).

The seam test passes in VMs on three different T4s, and the three print the
same image hashes. Shots stitched from eight VMs on eight GPUs are in `shots/`.
