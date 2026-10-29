# Phone GPU remoting: the guest's GL stream over a network

Measured 2026-10-29. The question: could a viewer's own phone be the GPU of an Android phone
emulated in an fcvm VM? gfxstream's host decoder would run in the viewer's browser (WebAssembly
and WebGL2) instead of on the VM's Tesla T4, and the guest's GL command stream would cross the
network. Today the T4 renders and the viewer gets video: the phone's screen in NVENC H.264 at
720x1280 and 30 fps, CBR at 0.8 bits per pixel capped at 20 Mbit/s
(`fleet/renderer/src/nvenc.cpp`). 0.8 bits per pixel of this screen is 22.1 Mbit/s, so the
phone's configured rate is the 20 Mbit/s cap. The encoder's actual output was not measured
here.

Two designs are compared:

- **Phone as the only renderer.** Every point where the guest waits for the host (a call with
  output, a sync object it creates, a readback) becomes a network round trip.
- **Mirror.** The T4 keeps rendering and answers every round trip. The phone receives a copy
  of the command stream and replays it, so only one-way bandwidth matters.

## Result

Ten scripted workloads, each a 60 s window after a launch, on the fleet's phone stack (AOSP
`android-15.0.0_r36`, the emulator built from source, gfxstream on a T4). The table is the
first pass ("as shipped"). A second pass differs from the first by at most 12% on bytes, 7% on
round trips a second and 4% on round trips per frame; compression ratios differ by up to 38%
with what the dump caught.

| workload | stream (Mbit/s) | after zstd -3 | video at CRF 23 | round trips/s | main frames/s | round trips per main frame | frames/s at 20 / 50 / 100 ms RTT |
|---|---|---|---|---|---|---|---|
| Robot Park | 7.2 | 0.46 | 3.68 | 240 | 58.7 | 1.0 | 16.7 / 6.7 / 3.3 |
| Launcher3 | 2.6 | 0.11 | 0.54 | 232 | 28.3 | 3.3 | 15.1 / 6.0 / 3.0 |
| Settings | 14.8 | 0.75 | 1.36 | 355 | 46.6 | 4.6 | 10.9 / 4.3 / 2.2 |
| Notification shade | 16.9 | 0.91 | 0.47 | 324 | 44.0 | 3.9 | 12.7 / 5.1 / 2.5 |
| Firefox, article | 936 | 22.4 | 2.27 | 784 | 38.6 | 15.7 | 3.2 / 1.3 / 0.64 |
| Firefox, images | 841 | 240 | 2.67 | 840 | 51.4 | 13.2 | 3.8 / 1.5 / 0.76 |
| Wikipedia app | 12.3 | 0.89 | 4.27 | 7,153 | 23.3 | 302 | 0.17 / 0.07 / 0.03 |
| Organic Maps | 33.2 | 1.9 | 1.57 | 484 | 45.5 | 1.7 | 16.7 / 6.7 / 3.3 |
| VLC 720p | 0.04 | 0.00 | 0.60 | 72 | 24.0 | 3.0 | 16.7 / 6.7 / 3.3 |
| SuperTuxKart | 1,156 | 168 | 1.02 | 253 | 83.3 | 1.0 | 16.7 / 6.7 / 3.3 |

- *stream*: everything the guest sent to the host decoder, all threads and processes. *after
  zstd -3*: that rate divided by the ratio zstd -3 reached on a dump of up to 10 s of the
  same window, each render thread's stream compressed on its own. The first pass capped a
  dump at 512 MiB, which held 3.7 to 5.1 s of Firefox and SuperTuxKart; the second pass's cap
  was 1.5 GB. Each table gives its sample's size.
- *video at CRF 23*: the same 60 s of screen, captured from the emulator at 30 fps (the
  newest frame at each tick; the streamer's NVENC stream is 30 fps too) and encoded with x264
  (preset medium, High profile, CRF 23, keyframe every 2 s). The configured CBR is 20 Mbit/s
  for every workload.
- *main frames/s*: the frames the app's main rendering thread swapped (VLC: the frames the
  composer composed, as the video never reaches the app's GL).
- *frames/s at RTT*: the most frames a second the main thread and the composer could each
  reach if every round trip cost one RTT and nothing else in a frame took time, the lower of
  the two: `min(frames/s, 1 / (round trips per frame x RTT))`. The composer makes 3 round
  trips per composed frame in every workload (3.1 to 3.2 for the launcher), so no workload
  can pass 16.7, 6.7 and 3.3.

**Phone as the only renderer does not work.** An app's frame waits on the host on two threads,
the app's and the composer's; VLC's decoded video waits only on the composer's. The app's
eglSwapBuffers creates a native fence with `rcCreateSyncKHR`, a call with output: one round
trip, which is the whole of Robot Park's and SuperTuxKart's per-frame cost. The composer makes
three per composed frame (`rcCreateSyncKHR`, `rcDestroySyncKHR`, `rcCompose`; 3.1 to 3.2 for
the launcher) in every workload. That alone caps the screen at 16.7 updates a second at 20 ms
of RTT, 6.7 at 50 ms and 3.3 at 100 ms. The apps add more: 3 to 5 round trips per frame for
HWUI's list scrolling (most of them `glBufferDataSyncAEMU`, a buffer allocation the guest waits
for), 13 to 16 for Firefox, and 302 for the Wikipedia app, whose WebView asks the host for
`glGetError` and `glGetIntegerv` 6,300 to 6,500 times a second. Bringing any of this under a
frame would take changes on the guest side (answer state queries from shadowed state, create
fences and sync objects with guest-allocated handles and no reply), not just moving the
decoder.

**A mirror's stream is smaller than the video for UI, and far larger for the browser and the
game.** With the T4 answering every round trip the phone only needs the stream. These are
bandwidths only: replaying the stream on a phone was not measured. Robot Park, the launcher,
Settings, the shade and the Wikipedia app send 0.3 to 2.1 MB/s, and zstd brings that to 0.1 to
1.3 Mbit/s, far below the 20 Mbit/s CBR. Against a CRF 23 encode of the same screen it is lower
for four of the five (Robot Park 0.46 against 3.7 Mbit/s, the Wikipedia app 0.89 against 4.3)
and twice the video for the shade (0.91 against 0.47). Organic Maps is at parity (1.9 against
1.6 Mbit/s). Firefox and SuperTuxKart send 100 to 145 MB/s, nearly all uploads that a GL stream
must carry and a video does not: Firefox's GeckoView maps buffers, and the guest encoder ships
the whole mapped range on every unmap (`glUnmapBufferAEMU`, 100 to 110 a second of about 1 MB
each); SuperTuxKart draws from client-side vertex arrays, resent on every draw
(`glVertexAttribPointerData`, `glDrawElementsData`). Compressed, that is 15 to 240 Mbit/s, 7 to
165 times the video at CRF 23. Against the 20 Mbit/s CBR the article is about level (22 Mbit/s
in the first pass, 15 in the repeat), and the image page is 11 to 12 times it. A mirror would
also have to carry decoded video (VLC's frames are decoded on the host and never cross the GL
stream) and, for Vulkan, the host-visible memory the guest writes directly (below).

## Method

### Counters

The counters are in gfxstream's host decoder, on the branch `remoting-stats` of ejc3/gfxstream
(draft PR ejc3/gfxstream#3). They are off unless `GFXSTREAM_STATS` names a file. When it does,
a logger thread appends one JSON line a second with that second's deltas for every render
thread. The guest reaches gfxstream through `opengles` pipe channels, one render thread per
guest connection, and the counters sit at that channel:

| counter | where | what |
|---|---|---|
| `read` | `RenderThread::main`, after `ReadBuffer::getData` | bytes the guest sent on the channel |
| `gles1`, `gles2`, `rc`, `vk` | after each decoder's `decode()` | bytes of whole packets each decoder consumed: GLES 1, GLES 2 and 3, renderControl (gfxstream's EGL and gralloc calls), Vulkan |
| per opcode: calls, bytes, replies, reply bytes | the same place, walking the packets' `[opcode][length]` headers | which calls the bytes are |
| `replies`, `reply_bytes` | `ChannelStream::commitBuffer` | every non-empty write back to the guest, against the opcode being decoded (each decoder stores it in a thread-local per packet) |
| `fence_signals` | `SyncThread`, each guest sync timeline increment | host fences that the guest's sync fds wait on |

A reply is the only way the host answers a guest call on this transport. The guest's encoder
sends a call that has output and then blocks in `readback()` until the reply has arrived, so
replies are the blocking round trips. Fence signals are the other thing the guest waits on. The
host signals them asynchronously through the sync device and the guest waits on them through
sync fds, not in the encoder, so they are reported separately and not counted as round trips.
`GFXSTREAM_STATS_DUMP` writes the decoded stream to a file, as records of (render thread,
decoder, length, bytes), for the compression measurement.

Not on the channel, so not counted:

- Vulkan host-visible memory, which the guest writes directly through its mapping.
- The payloads of `rcUpdateColorBufferDMA` and `rcReadColorBufferDMA`, which use the goldfish
  DMA region. No workload called the first. The second was called 13 and 12 times in the two
  passes, boot included, and its payload is not counted.
- Video decoded by the emulator's codec (`c2.goldfish.h264.decoder`, which VLC uses), whose
  frames reach host color buffers through the media pipe.

### Validation

The counters were checked against strace inside the guest. Robot Park was stopped with SIGSTOP,
strace attached to all of its threads and recorded every `read` and `write` on its goldfish
pipe descriptors with timestamps, and the app then ran for 30 s and was stopped again
(`scripts/phone/remoting/validate.sh` and `validate.py` on the measurement branch). The app's
puid is the one whose traffic stops in both stopped spans. Over the 29 whole seconds inside the
strace capture, with the guest clock's offset measured by five `date` round trips (32 ms at
best):

| | host counters | strace in the guest |
|---|---|---|
| bytes guest to host | 26,191,079 | 26,191,079 written |
| bytes host to guest | 41,736 | 41,736 read |
| round trips | 1,739 replies | 1,739 reads |

Over the whole span from the first stop to the resume the host counted 48 frames more than
strace (1,846 against 1,798). Those are the frames the app drew in the partial seconds before
the first stop and after the resume, which are outside strace's capture. Per frame, both sides
saw 15,061 bytes and one 24-byte reply. The reply count also matches what the guest code says
it should be: `egl_window_surface_t::swapBuffers` (`guest/egl/egl.cpp`) sends
`rcFlushWindowColorBufferAsyncWithFrameNumber`, which has no output, and then
`createNativeSync`, whose `rcCreateSyncKHR` does, and Robot Park makes no queries of its own.

### Setup

- One g4dn.metal (8 Tesla T4, driver 580.196.07) launched for this with
  `fleet/tools/gpubox.sh`, nothing else running on it; the 1-minute load during the runs was
  1.3 to 4.0 on its 96 vCPUs, which is the phone VM.
- One phone VM of the `phone` profile: one T4, 6 vCPU, 10 GiB, restored from its pre-start
  snapshot as every GPU VM is, running the fleet's phone container.
- The emulator built from source (`scripts/phone/emulator.sh`) with the instrumented
  gfxstream, and the AOSP image the fleet runs (`scripts/phone/aosp.sh`, build a255e314f1c7).
- The fleet's phone: 720x1280, density 320, 4 cores. It had 4 GiB of RAM instead of 2 GiB, so
  that the browser and the map would not be killed for memory.
- Each pass is one cold boot (`phone.sh boot`, `-grpc 8554` for the screen capture), then the
  ten workloads in order. The measurement branch `phone-gpu-remoting-bench` of this
  repository (05a4d6cb) holds the emulator pin, the scripts (`scripts/phone/remoting/`) and
  the streamer's `capture` mode.

**Renderer.** The emulator sets `ro.boot.debug.hwui.renderer=skiagl`, which `init.ranchu.rc`
copies to `debug.hwui.renderer`, so HWUI runs on Skia GL as shipped. Guest Vulkan exists
(`ro.boot.hardware.vulkan=ranchu`), but HWUI does not use it. Every one of the 20 runs reported
`Pipeline=Skia (OpenGL)` in `dumpsys gfxinfo`. The second pass boots with `-prop
debug.hwui.renderer=skiagl`, which is therefore the same configuration and serves as the
repeat. Organic Maps picks Vulkan by itself and segfaults on this emulator within a second of
creating its device, on every launch: a `memmove` in its render thread faults on an unmapped
address (`SEGV_MAPERR`). The cause was not investigated. It was set to OpenGL ES 3 in its own
settings (`PreferredGraphicsAPI=OpenGLES3` in `settings.ini`) and reports `Api = OpenGLES3 |
Renderer = Android Emulator OpenGL ES Translator (Tesla T4/PCIe/SSE2)`. No workload sent any
Vulkan.

**HWUI on Vulkan, for comparison.** A third boot set `debug.hwui.renderer` to `skiavk` at run
time, and the launcher and Settings (restarted by their runs) drew with `Pipeline=Skia
(Vulkan)`. The same scripts:

| | Launcher3, GL | Launcher3, Vulkan | Settings, GL | Settings, Vulkan |
|---|---|---|---|---|
| guest->host (MB/s) | 0.32 | 0.14 (Vulkan 0.09) | 1.85 | 0.28 (Vulkan 0.27) |
| frames/s (`gfxinfo`) | 28.6 | 27.8 | 46.7 | 46.6 |
| the app's round trips (/s) | 94 | 259 | 214 | 359 |
| the app's round trips per frame | 3.3 | 9.3 | 4.6 | 7.7 |

On Vulkan HWUI makes more blocking calls per frame, not fewer: `vkGetFenceStatus`,
`vkResetCommandPool`, `vkResetFences`, `vkCreateSemaphore` and `vkQueueWaitIdle` all wait for
the host. Its stream is smaller partly because the vertex and uniform data it writes to
host-visible memory is not in the stream (above); how much that memory carries was not
measured. A WebGL2 renderer could not take this stream in any case. The Wikipedia app on Vulkan
drew 7 frames a second and reported no pipeline, and SystemUI could not be restarted without
root, so neither is reported.

### Workloads

Each workload is one run of `wl.sh run <workload>`. It force-stops the app, goes home, launches
it with `am start -W` (whose TotalTime is the time to the first frame), waits 15 s, and then
repeats the same `adb shell input` steps until 60 s have passed. During the window the screen
is read from the emulator's gRPC screen stream, which delivers every frame the display posts
(the *display updates/s* row, up to 60). The capture writes the newest of them every 1/30 s as
raw NV12, the streamer's NVENC frame rate, and x264 encodes those 30 fps afterward. The stream
is dumped for the 10 s from 20 s in. Rates are over the whole seconds inside the window: 59 to
64, as the last step may run past 60 s. F-Droid APKs were checked against the SHA-256 in
F-Droid's index.

| workload | app | in the 60 s window |
|---|---|---|
| Robot Park | `fleet/phone-app`, GLES 3 | no input; the scene animates |
| Launcher3 | AOSP | swipe up to the app drawer, fling down and up, Home, swipe between home pages |
| Settings | AOSP | three flings down, three up |
| Notification shade | AOSP SystemUI | pull the notifications, pull again for quick settings, swipe closed |
| Firefox, article | Fennec F-Droid 158.0.2 (x86_64) | `https://en.wikipedia.org/wiki/Android_(operating_system)`; ten flings down, ten up |
| Firefox, images | the same | `https://commons.wikimedia.org/wiki/Commons:Featured_pictures/Animals/Mammals`; the same flings |
| Wikipedia app | org.wikipedia r/50614 | the article Moon (which has an animated figure); the same flings |
| Organic Maps | 2026.09.29-32 F-Droid | Chico, California, downloaded offline (Organic Maps offered it from the phone's location); pan four ways, then zoom in twice and out twice with the on-screen buttons |
| VLC | 3.7.2 (x86_64) | a local 1280x720 H.264 High file, 24 fps, 1.6 Mbit/s: 100 s of Big Buck Bunny (Blender Foundation, CC BY 3.0); plays, no input |
| SuperTuxKart | 1.4 | a Novice race on Nessie's Pond with 3 AI karts and auto-acceleration; hold left, then right, 1.5 s each |

The pages came over the VM's own internet access. SuperTuxKart's launch is followed by the same
menu taps and 20 s of loading and countdown before the window, so its startup bytes include the
race's first seconds.

## Per workload

Both passes. *Startup* is from the launch: to the end of the second holding the first frame,
and the whole 15 s before the window. It counts whole stats seconds of every process, so the
first-frame figure includes up to a second past the first frame and varies between passes.
Shaders are `glShaderString` and the program binary calls; textures are
`gl[Compressed]Tex[Sub]Image{2,3}D`; buffers are `glBufferData`, `glBufferSubData` and the
mapped-buffer writes (`glUnmapBufferAEMU`, `glFlushMappedBufferRangeAEMU`).

### Robot Park

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 7.2 | 7.2 |
|   GLES / renderControl / Vulkan (Mbit/s) | 7.1 / 0.12 / 0.0 | 7.1 / 0.12 / 0.0 |
|   texture uploads (Mbit/s) | 0.03 | 0.03 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 0.02 | 0.02 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 39.6 | 36.8 |
| blocking round trips, all threads (/s) | 240 | 238 |
| top round trips (/s) | `rcCreateSyncKHR` 118, `rcDestroySyncKHR` 59, `rcCompose` 59, `glBufferDataSyncAEMU` 2 | `rcCreateSyncKHR` 118, `rcDestroySyncKHR` 59, `rcCompose` 58, `glBufferDataSyncAEMU` 2 |
| main rendering thread: frames/s | 58.7 | 58.5 |
| main thread round trips per frame | 1.0 | 1.0 |
| all app threads' round trips per main frame | 1.0 | 1.0 |
| composer: composed frames/s, round trips per frame | 58.7, 3.0 | 58.5, 3.0 |
| fence signals to the guest (/s) | 118 | 118 |
| startup: `am start` TotalTime (ms) | 452 | 490 |
| startup: bytes to first frame (MB) | 4.51 | 1.26 |
| startup: bytes in the 16 s before the window (MB) | 18.2 | 16.8 |
|   of which shaders / textures / buffers (MB) | 0.08 / 2.03 / 2.03 | 0.08 / 1.11 / 2.04 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 58.7 | 58.5 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 3.68 | 3.67 |
| command stream after zstd -3 / lz4 (Mbit/s) | 0.46 / 1.3 | 0.46 / 1.3 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 15.8 / 5.6, 8.7 | 15.6 / 5.6, 8.5 |
| frames/s if each round trip costs 20 ms: main thread, composer | 50.0, 16.7 | 50.0, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 20.0, 6.7 | 20.0, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 10.0, 3.3 | 10.0, 3.3 |

### Launcher3: home pages and app drawer

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 2.6 | 2.5 |
|   GLES / renderControl / Vulkan (Mbit/s) | 2.5 / 0.10 / 0.0 | 2.4 / 0.10 / 0.0 |
|   texture uploads (Mbit/s) | 0.01 | 0.01 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 1.70 | 1.63 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 38.2 | 35.8 |
| blocking round trips, all threads (/s) | 232 | 215 |
| top round trips (/s) | `rcCreateSyncKHR` 81, `glBufferDataSyncAEMU` 71, `rcDestroySyncKHR` 50, `rcCompose` 24 | `rcCreateSyncKHR` 74, `glBufferDataSyncAEMU` 68, `rcDestroySyncKHR` 43, `rcCompose` 25 |
| main rendering thread: frames/s | 28.3 | 27.9 |
| main thread round trips per frame | 3.3 | 3.3 |
| all app threads' round trips per main frame | 3.3 | 3.3 |
| composer: composed frames/s, round trips per frame | 24.4, 3.2 | 25.3, 3.1 |
| fence signals to the guest (/s) | 60 | 59 |
| startup: `am start` TotalTime (ms) | - | - |
| startup: bytes to first frame (MB) | 1.22 | 0.33 |
| startup: bytes in the 15 s before the window (MB) | 3.8 | 1.6 |
|   of which shaders / textures / buffers (MB) | 0.03 / 1.16 / 1.76 | 0.03 / 1.12 / 0.27 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 28.5 | 28.5 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 0.54 | 0.53 |
| command stream after zstd -3 / lz4 (Mbit/s) | 0.11 / 0.43 | 0.11 / 0.38 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 22.6 / 6.0, 3.6 | 23.0 / 6.4, 3.5 |
| frames/s if each round trip costs 20 ms: main thread, composer | 15.1, 15.8 | 15.1, 16.0 |
| frames/s if each round trip costs 50 ms: main thread, composer | 6.0, 6.3 | 6.0, 6.4 |
| frames/s if each round trip costs 100 ms: main thread, composer | 3.0, 3.2 | 3.0, 3.2 |

### Settings list

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 14.8 | 14.6 |
|   GLES / renderControl / Vulkan (Mbit/s) | 14.6 / 0.14 / 0.0 | 14.5 / 0.14 / 0.0 |
|   texture uploads (Mbit/s) | 0.04 | 0.02 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 12.25 | 12.16 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 45.4 | 45.1 |
| blocking round trips, all threads (/s) | 355 | 352 |
| top round trips (/s) | `glBufferDataSyncAEMU` 166, `rcCreateSyncKHR` 93, `rcDestroySyncKHR` 47, `rcCompose` 47 | `glBufferDataSyncAEMU` 164, `rcCreateSyncKHR` 92, `rcDestroySyncKHR` 46, `rcCompose` 46 |
| main rendering thread: frames/s | 46.6 | 46.2 |
| main thread round trips per frame | 4.6 | 4.6 |
| all app threads' round trips per main frame | 4.6 | 4.6 |
| composer: composed frames/s, round trips per frame | 46.6, 3.0 | 46.2, 3.0 |
| fence signals to the guest (/s) | 93 | 92 |
| startup: `am start` TotalTime (ms) | 524 | 516 |
| startup: bytes to first frame (MB) | 1.50 | 0.83 |
| startup: bytes in the 16 s before the window (MB) | 2.5 | 2.5 |
|   of which shaders / textures / buffers (MB) | 0.01 / 1.22 / 0.98 | 0.01 / 0.50 / 1.59 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 46.6 | 46.3 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 1.36 | 1.42 |
| command stream after zstd -3 / lz4 (Mbit/s) | 0.75 / 3.7 | 0.74 / 3.7 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 19.7 / 4.0, 16.7 | 19.8 / 4.0, 16.8 |
| frames/s if each round trip costs 20 ms: main thread, composer | 10.9, 16.7 | 10.9, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 4.3, 6.7 | 4.4, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 2.2, 3.3 | 2.2, 3.3 |

### Notification shade and quick settings

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 16.9 | 16.7 |
|   GLES / renderControl / Vulkan (Mbit/s) | 16.8 / 0.11 / 0.0 | 16.6 / 0.11 / 0.0 |
|   texture uploads (Mbit/s) | 0.77 | 0.87 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 8.58 | 8.53 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 77.5 | 89.4 |
| blocking round trips, all threads (/s) | 324 | 325 |
| top round trips (/s) | `glBufferDataSyncAEMU` 125, `rcCreateSyncKHR` 94, `rcDestroySyncKHR` 44, `rcCompose` 44 | `glBufferDataSyncAEMU` 124, `rcCreateSyncKHR` 94, `rcDestroySyncKHR` 44, `rcCompose` 44 |
| main rendering thread: frames/s | 44.0 | 43.9 |
| main thread round trips per frame | 3.9 | 4.0 |
| all app threads' round trips per main frame | 7.4 (all processes: the app was already running) | 7.4 (all processes: the app was already running) |
| composer: composed frames/s, round trips per frame | 44.3, 3.0 | 44.1, 3.0 |
| fence signals to the guest (/s) | 94 | 94 |
| startup: `am start` TotalTime (ms) | - | - |
| startup: bytes to first frame (MB) | 0.00 | 0.00 |
| startup: bytes in the 15 s before the window (MB) | 1.0 | 0.14 |
|   of which shaders / textures / buffers (MB) | 0.01 / 0.30 / 0.52 | 0.00 / 0.06 / 0.03 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 44.1 | 44.1 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 0.47 | 0.47 |
| command stream after zstd -3 / lz4 (Mbit/s) | 0.91 / 3.9 | 0.92 / 3.9 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 18.6 / 4.3, 21.6 | 18.1 / 4.3, 21.4 |
| frames/s if each round trip costs 20 ms: main thread, composer | 12.7, 16.7 | 12.5, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 5.1, 6.7 | 5.0, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 2.5, 3.3 | 2.5, 3.3 |

### Firefox (Fennec F-Droid): long article

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 936 | 894 |
|   GLES / renderControl / Vulkan (Mbit/s) | 936 / 0.17 / 0.0 | 893 / 0.16 / 0.0 |
|   texture uploads (Mbit/s) | 0.02 | 0.02 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 934 | 891 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 269 | 257 |
| blocking round trips, all threads (/s) | 784 | 750 |
| top round trips (/s) | `glBufferDataSyncAEMU` 319, `rcCreateSyncKHR` 132, `glUnmapBufferAEMU` 111, `rcDestroySyncKHR` 81 | `glBufferDataSyncAEMU` 303, `rcCreateSyncKHR` 126, `glUnmapBufferAEMU` 106, `rcDestroySyncKHR` 78 |
| main rendering thread: frames/s | 38.6 | 37.2 |
| main thread round trips per frame | 15.7 | 15.6 |
| all app threads' round trips per main frame | 17.0 | 16.9 |
| composer: composed frames/s, round trips per frame | 42.3, 3.0 | 40.8, 3.0 |
| fence signals to the guest (/s) | 132 | 126 |
| startup: `am start` TotalTime (ms) | 962 | 1,013 |
| startup: bytes to first frame (MB) | 3.88 | 0.50 |
| startup: bytes in the 16 s before the window (MB) | 3.9 | 3.9 |
|   of which shaders / textures / buffers (MB) | 0.02 / 0.35 / 3.32 | 0.02 / 0.33 / 3.32 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 42.1 | 40.9 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 2.27 | 2.29 |
| command stream after zstd -3 / lz4 (Mbit/s) | 22.4 / 45.6 | 15.5 / 31.4 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 41.8 / 20.5, 537 | 57.8 / 28.5, 796 |
| frames/s if each round trip costs 20 ms: main thread, composer | 3.2, 16.7 | 3.2, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 1.3, 6.7 | 1.3, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 0.64, 3.3 | 0.64, 3.3 |

### Firefox (Fennec F-Droid): image-heavy page

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 841 | 817 |
|   GLES / renderControl / Vulkan (Mbit/s) | 841 / 0.20 / 0.0 | 817 / 0.20 / 0.0 |
|   texture uploads (Mbit/s) | 0.00 | 0.00 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 839 | 815 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 443 | 566 |
| blocking round trips, all threads (/s) | 840 | 809 |
| top round trips (/s) | `glBufferDataSyncAEMU` 321, `rcCreateSyncKHR` 157, `rcDestroySyncKHR` 104, `glUnmapBufferAEMU` 100 | `glBufferDataSyncAEMU` 309, `rcCreateSyncKHR` 151, `rcDestroySyncKHR` 100, `glUnmapBufferAEMU` 97 |
| main rendering thread: frames/s | 51.4 | 49.5 |
| main thread round trips per frame | 13.2 | 13.2 |
| all app threads' round trips per main frame | 13.3 | 13.3 |
| composer: composed frames/s, round trips per frame | 52.4, 3.0 | 50.5, 3.0 |
| fence signals to the guest (/s) | 157 | 151 |
| startup: `am start` TotalTime (ms) | 932 | 981 |
| startup: bytes to first frame (MB) | 1.48 | 4.74 |
| startup: bytes in the 16 s before the window (MB) | 127 | 144 |
|   of which shaders / textures / buffers (MB) | 0.24 / 1.23 / 125 | 0.26 / 1.21 / 142 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 52.5 | 50.5 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 2.67 | 2.57 |
| command stream after zstd -3 / lz4 (Mbit/s) | 240 / 519 | 223 / 487 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 3.5 / 1.6, 537 | 3.7 / 1.7, 672 |
| frames/s if each round trip costs 20 ms: main thread, composer | 3.8, 16.7 | 3.8, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 1.5, 6.7 | 1.5, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 0.76, 3.3 | 0.76, 3.3 |

### Wikipedia app: article

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 12.3 | 13.7 |
|   GLES / renderControl / Vulkan (Mbit/s) | 12.2 / 0.08 / 0.0 | 13.6 / 0.08 / 0.0 |
|   texture uploads (Mbit/s) | 7.84 | 9.36 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 1.65 | 1.65 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 715 | 695 |
| blocking round trips, all threads (/s) | 7,153 | 6,937 |
| top round trips (/s) | `glGetError` 4,608, `glGetIntegerv` 1,862, `glGetBooleanv` 256, `glGetFloatv` 186 | `glGetError` 4,475, `glGetIntegerv` 1,808, `glGetBooleanv` 249, `glGetFloatv` 181 |
| main rendering thread: frames/s | 23.3 | 22.6 |
| main thread round trips per frame | 302 | 302 |
| all app threads' round trips per main frame | 304 | 304 |
| composer: composed frames/s, round trips per frame | 23.3, 3.0 | 22.6, 3.0 |
| fence signals to the guest (/s) | 47 | 45 |
| startup: `am start` TotalTime (ms) | 1,236 | 1,227 |
| startup: bytes to first frame (MB) | 2.34 | 2.10 |
| startup: bytes in the 16 s before the window (MB) | 3.1 | 3.1 |
|   of which shaders / textures / buffers (MB) | 0.02 / 1.65 / 0.73 | 0.03 / 1.66 / 0.71 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 23.3 | 22.6 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 4.27 | 3.98 |
| command stream after zstd -3 / lz4 (Mbit/s) | 0.89 / 2.4 | 1.3 / 3.5 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 13.8 / 5.2, 6.4 | 10.4 / 3.9, 4.0 |
| frames/s if each round trip costs 20 ms: main thread, composer | 0.17, 16.7 | 0.17, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 0.07, 6.7 | 0.07, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 0.03, 3.3 | 0.03, 3.3 |

### Organic Maps: pan and zoom, offline region

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 33.2 | 30.5 |
|   GLES / renderControl / Vulkan (Mbit/s) | 33.0 / 0.20 / 0.0 | 30.3 / 0.20 / 0.0 |
|   texture uploads (Mbit/s) | 0.04 | 0.06 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 18.56 | 16.50 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 65.0 | 62.9 |
| blocking round trips, all threads (/s) | 484 | 467 |
| top round trips (/s) | `glBufferDataSyncAEMU` 137, `rcCreateSyncKHR` 137, `glGenBuffers` 66, `rcDestroySyncKHR` 55 | `rcCreateSyncKHR` 135, `glBufferDataSyncAEMU` 131, `glGenBuffers` 61, `rcDestroySyncKHR` 54 |
| main rendering thread: frames/s | 45.5 | 45.1 |
| main thread round trips per frame | 1.7 | 1.6 |
| all app threads' round trips per main frame | 6.9 | 6.6 |
| composer: composed frames/s, round trips per frame | 54.9, 3.0 | 54.5, 3.0 |
| fence signals to the guest (/s) | 137 | 135 |
| startup: `am start` TotalTime (ms) | 446 | 527 |
| startup: bytes to first frame (MB) | 0.67 | 0.67 |
| startup: bytes in the 17 s before the window (MB) | 98.6 | 103 |
|   of which shaders / textures / buffers (MB) | 0.09 / 10.96 / 62.20 | 0.09 / 10.93 / 63.61 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 55.2 | 55.0 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 1.57 | 1.56 |
| command stream after zstd -3 / lz4 (Mbit/s) | 1.9 / 6.3 | 1.8 / 5.8 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 17.5 / 5.3, 22.4 | 16.9 / 5.2, 22.5 |
| frames/s if each round trip costs 20 ms: main thread, composer | 29.0, 16.7 | 30.3, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 11.6, 6.7 | 12.1, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 5.8, 3.3 | 6.1, 3.3 |

### VLC: local 720p H.264

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 0.04 | 0.04 |
|   GLES / renderControl / Vulkan (Mbit/s) | 0.00 / 0.04 / 0.0 | 0.00 / 0.04 / 0.0 |
|   texture uploads (Mbit/s) | 0.00 | 0.00 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 0.00 | 0.00 |
|   client-side vertex arrays (Mbit/s) | 0.00 | 0.00 |
| host->guest replies (kbit/s) | 9.3 | 9.3 |
| blocking round trips, all threads (/s) | 72 | 72 |
| top round trips (/s) | `rcCreateSyncKHR` 24, `rcDestroySyncKHR` 24, `rcCompose` 24 | `rcCreateSyncKHR` 24, `rcDestroySyncKHR` 24, `rcCompose` 24 |
| main rendering thread: frames/s | 24.0 | 24.0 |
| main thread round trips per frame | 3.0 | 3.0 |
| all app threads' round trips per main frame | 0.0 | 0.0 |
| composer: composed frames/s, round trips per frame | 24.0, 3.0 | 24.0, 3.0 |
| fence signals to the guest (/s) | 24 | 24 |
| startup: `am start` TotalTime (ms) | 461 | 486 |
| startup: bytes to first frame (MB) | 0.58 | 0.54 |
| startup: bytes in the 16 s before the window (MB) | 0.90 | 0.81 |
|   of which shaders / textures / buffers (MB) | 0.00 / 0.22 / 0.27 | 0.00 / 0.22 / 0.24 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 24.0 | 24.0 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 0.60 | 0.61 |
| command stream after zstd -3 / lz4 (Mbit/s) | 0.00 / 0.01 | 0.00 / 0.01 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 10.2 / 5.7, 0.06 | 10.0 / 5.7, 0.06 |
| frames/s if each round trip costs 20 ms: main thread, composer | 16.7, 16.7 | 16.7, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 6.7, 6.7 | 6.7, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 3.3, 3.3 | 3.3, 3.3 |

### SuperTuxKart: race

| | as shipped | GL forced (repeat) |
|---|---|---|
| HWUI renderer | Skia (OpenGL) | Skia (OpenGL) |
| guest->host, all threads (Mbit/s) | 1,156 | 1,120 |
|   GLES / renderControl / Vulkan (Mbit/s) | 1,156 / 0.13 / 0.0 | 1,120 / 0.13 / 0.0 |
|   texture uploads (Mbit/s) | 0.01 | 0.00 |
|   buffer uploads: BufferData, BufferSubData, mapped writes (Mbit/s) | 0.00 | 0.00 |
|   client-side vertex arrays (Mbit/s) | 1,106 | 1,071 |
| host->guest replies (kbit/s) | 37.7 | 36.7 |
| blocking round trips, all threads (/s) | 253 | 247 |
| top round trips (/s) | `rcCreateSyncKHR` 140, `rcDestroySyncKHR` 56, `rcCompose` 56 | `rcCreateSyncKHR` 136, `rcDestroySyncKHR` 55, `rcCompose` 55 |
| main rendering thread: frames/s | 83.3 | 80.5 |
| main thread round trips per frame | 1.0 | 1.0 |
| all app threads' round trips per main frame | 1.0 | 1.0 |
| composer: composed frames/s, round trips per frame | 56.3, 3.0 | 55.4, 3.0 |
| fence signals to the guest (/s) | 140 | 136 |
| startup: `am start` TotalTime (ms) | 521 | 535 |
| startup: bytes to first frame (MB) | 4.11 | 3.98 |
| startup: bytes in the 48 s before the window (MB) | 4,405 | 4,241 |
|   of which shaders / textures / buffers (MB) | 0.04 / 87.34 / 0.18 | 0.04 / 87.33 / 0.17 |
| display updates/s (the emulator's screen stream; the capture is 30 fps) | 56.3 | 55.3 |
| video, configured NVENC CBR (Mbit/s) | 20.0 | 20.0 |
| video, x264 CRF 23 of the 30 fps capture (Mbit/s) | 1.02 | 1.01 |
| command stream after zstd -3 / lz4 (Mbit/s) | 168 / 634 | 164 / 615 |
|   ratio zstd -3 / lz4, on a sample of (MB) | 6.9 / 1.8, 537 | 6.8 / 1.8, 630 |
| frames/s if each round trip costs 20 ms: main thread, composer | 50.0, 16.7 | 50.0, 16.7 |
| frames/s if each round trip costs 50 ms: main thread, composer | 20.0, 6.7 | 20.0, 6.7 |
| frames/s if each round trip costs 100 ms: main thread, composer | 10.0, 3.3 | 10.0, 3.3 |
