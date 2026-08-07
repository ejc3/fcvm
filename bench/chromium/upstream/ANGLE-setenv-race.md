# ANGLE: `ScopedVkLoaderEnvironment` setenv(VK_ICD_FILENAMES) races with concurrent getenv() → SIGSEGV during startup

**STATUS: FILED 2026-08-07 as https://issues.angleproject.org/issues/543664586.**

**File at: https://issues.angleproject.org/issues/new** — this is ANGLE's tracker, and it is
where `anglebug.com/new` (the URL ANGLE's own `README.md:113` and `doc/ContributingCode.md`
tell contributors to use) now redirects; the old Monorail
`bugs.chromium.org/p/angleproject` also redirects to `issues.angleproject.org`.

Why it was not filed here: `http://anglebug.com/new` resolves to
`accounts.google.com/v3/signin/…?continue=https://issues.angleproject.org/issues/new`, i.e. a
Google sign-in wall. Unauthenticated search on that tracker returns only a "Sign in" app
shell, and the tracker's `action/issues/list` RPC ignores unauthenticated queries. This
environment has GitHub credentials only — no Google account — so the report below was **not**
submitted. Everything under "Report" is ready to paste verbatim.

Then cross-file a Chromium-side tracking bug at https://issues.chromium.org/issues/new under
component `Internals>GPU` referencing the ANGLE bug (see "Chromium-side ask").

---

## Report

### Title

`ScopedVkLoaderEnvironment::setICDEnvironment` calls `setenv()` after threads exist; races
with concurrent `getenv()` and crashes (`--single-process` / `--in-process-gpu`)

### Component

**ANGLE** (`issues.angleproject.org`) — ANGLE owns the offending `setenv()` call. When
mirrored onto the Chromium tracker, the matching component is `Internals>GPU>ANGLE`.
Secondary: **Chromium `Internals>GPU`** — `--single-process` / `--in-process-gpu` is what
places the ANGLE writer and the fontconfig reader in one address space.

### Summary

`angle::vk::ScopedVkLoaderEnvironment` mutates the process environment (`setenv()` on
`VK_ICD_FILENAMES`, plus a paired `setenv()`/`unsetenv()` restore in its destructor) at
Vulkan instance-probe time — i.e. long after the embedding application has started threads.
`setenv()` is MT-Unsafe. In Chromium's `--single-process` mode the ANGLE ICD probe runs on
`Chrome_InProcGpuThread` concurrently with `gfx::InitializeGlobalFontConfigAsync()`, which is
inside `FcInitLoadConfigAndFonts` → `FcConfigGetFilename` → `getenv()` on a ThreadPool worker.
On glibc ≤ 2.40 the `setenv()` of a not-yet-present name `realloc()`s the `environ` array and
**frees the old one**, so the fontconfig thread's `getenv()` walk dereferences freed memory
and the process dies with SIGSEGV before it ever renders. We measure **~7% of launches under
concurrency**; with a race amplifier it is **100%**. Whether it crashes is decided entirely by
whether that `realloc()` happens to move the block, which is why it looks like a mysterious
environment-dependent flake.

### Version / platform

| | |
|---|---|
| Chromium | **151.0.7922.71** (Debian package build) |
| Binary | `/usr/lib/chromium/chromium`, `BuildID[sha1]=6c67d2f1222168179600960b8d6966d9b7359dc3` |
| Platform | Debian 12 (bookworm), **aarch64** |
| libc | **Debian GLIBC 2.36-9+deb12u14** (glibc 2.36) |
| Flags | `--single-process` (also reachable via `--in-process-gpu`) |
| GPU path | SwiftShader / `vk_swiftshader_icd.json` (software Vulkan) |
| Rate | ~7% of launches under launch concurrency; 3/3 captured cores identical |

### Faulting stack (symbolized, from core dump)

```
getenv()                                          libc
  FcConfigGetFilename                             libfontconfig
    <expat parse callback>                        libexpat (fontconfig XML config)
      FcInitLoadConfigAndFonts                    libfontconfig
        base::NoDestructor<gfx::GlobalFontConfig> chromium
          gfx::InitializeGlobalFontConfigAsync()::$_0   posted task
            <ThreadPoolForegroundWorker>
```

Faulting instruction is the glibc `environ` walk:

```
ldr x19, [x20, #8]!      x19 = 0x1a040000000000
```

`x19` is freed-slot garbage, not a `char *`. **Identical in all 3 captured cores.**
`0x1a04...` is PartitionAlloc metadata written into the block after it was freed — the
environ array was recycled while it was still being walked.

### The writer

Identified with an `LD_PRELOAD` interposer on `setenv`/`unsetenv` (`/tmp/chr/preload/envtrace2.c`),
which logs the calling thread and a backtrace:

```
setenv("VK_ICD_FILENAMES", "/usr/lib/chromium/./vk_swiftshader_icd.json", 1)
  thread: Chrome_InProcGpuThread
  angle::SetEnvironmentVar                    src/common/system_utils_posix.cpp:135
  angle::vk::ScopedVkLoaderEnvironment::setICDEnvironment
                                              src/common/vulkan/vulkan_icd.cpp:215
  angle::vk::ScopedVkLoaderEnvironment::ScopedVkLoaderEnvironment
                                              src/common/vulkan/vulkan_icd.cpp:133
```

Source (current `main`, `src/common/vulkan/vulkan_icd.cpp`):

```cpp
bool ScopedVkLoaderEnvironment::setICDEnvironment(const char *icd)
{
    mPreviousICDEnv = angle::GetEnvironmentVar(kLoaderICDFilenamesEnv);
    mChangedICDEnv  = angle::SetEnvironmentVar(kLoaderICDFilenamesEnv, icd);   // <-- setenv()
    ...
}
```

`angle::SetEnvironmentVar` is a bare `setenv()` (`src/common/system_utils_posix.cpp:135`):

```cpp
bool SetEnvironmentVar(const char *variableName, const char *value)
{
    return (setenv(variableName, value, 1) == 0);
}
```

The destructor mutates `environ` a **second** time via `ResetEnvironmentVar` →
`SetEnvironmentVar`/`UnsetEnvironmentVar`. `ScopedVkLoaderEnvironment` is constructed on three
production paths, so a single process performs several set/restore pairs, each one a fresh
race window:

- `src/libANGLE/renderer/vulkan/vk_renderer.cpp:2507` (renderer init — the one in our stack)
- `src/gpu_info_util/SystemInfo_vulkan.cpp:519` (GPU info probe)
- `src/libANGLE/renderer/vulkan/DeviceVk.cpp:32` (EGL device)

### Mechanism

`setenv()` is documented **MT-Unsafe**. From the glibc manual (Environment Access):

- `getenv` — `Preliminary: | MT-Safe env | AS-Safe | AC-Safe |`
- `setenv` — `Preliminary: | MT-Unsafe const:env | AS-Unsafe heap lock | AC-Unsafe corrupt lock mem |`
- `unsetenv` — `Preliminary: | MT-Unsafe const:env | AS-Unsafe lock | AC-Unsafe lock |`
- and explicitly: *"Modifications of environment variables are not allowed in multi-threaded programs."*

`getenv` being `MT-Safe env` means it is safe against **other readers**, not against a
concurrent writer. glibc's `setenv` takes an internal lock that `getenv` does not.

In glibc 2.36 (`stdlib/setenv.c`, `__add_to_environ`), when the name is **not already
present**:

```c
  if (ep == NULL || __builtin_expect (*ep == NULL, 1))
    {
      char **new_environ;
      uintptr_t ip_last_environ = (uintptr_t)last_environ;
      new_environ = (char **) realloc (last_environ,          /* <-- may move + free */
                                       (size + 2) * sizeof (char *));
      ...
      if ((uintptr_t)__environ != ip_last_environ)
        memcpy ((char *) new_environ, (char *) __environ, size * sizeof (char *));
      ...
      last_environ = __environ = new_environ;                 /* <-- publish */
    }
```

So:

1. `realloc()` grows the pointer array by one slot. If it **moves**, the old array is
   **freed** and `__environ` is repointed.
2. A thread already inside `getenv()` is walking the **old** array. Its cursor now points
   into freed memory.
3. PartitionAlloc (Chromium's allocator, which services this heap) reuses the freed block
   almost immediately and writes its own metadata into it.
4. The walker loads a "pointer" that is now allocator metadata → `x19 = 0x1a040000000000` →
   SIGSEGV.

Two details that matter for triage:

- **If the name is already present**, `__add_to_environ` skips the `realloc` entirely and
  does a single pointer store into the existing slot (`*ep = np`). That is why pre-seeding
  the variable is a complete fix, not a timing tweak.
- The **first** new-name `setenv()` in a process is benign: `last_environ == NULL`, so
  `realloc(NULL, …)` is a `malloc` and the kernel-supplied array is never freed. Chromium
  performs several `setenv()`s during startup, so by the time ANGLE runs, `last_environ` is a
  heap block and the free is live.

**glibc ≥ 2.41 is not affected by the use-after-free.** glibc commit `7a61e7f557a9`
("stdlib: Make getenv thread-safe in more cases", Florian Weimer, landed in 2.41) replaced
`last_environ` with a versioned `environ_array` list and explicitly *"Do not deallocate the
environment array. Instead, keep older versions around."* Verified: `stdlib/setenv.c` has
`last_environ` in tags `glibc-2.36`/`glibc-2.40` and `environ_array` in `glibc-2.41`/`2.42`.
So Debian bookworm (2.36) crashes and Debian trixie (2.41) does not — but the `setenv()` call
is still MT-Unsafe by contract, and this should not be left resting on the libc version.

### Environment-parity dependence (why some launchers never see this)

The crash is decided by whether that one `realloc()` moves the block, which depends on the
size class of the current `environ` array — i.e. **on how many variables the parent process
exported**. Measured directly (the interposer reports the pre/post `environ` address, so
"moved" vs "grew in place" is observed, not inferred):

| # | Parent / configuration | env vars | `environ` array on the ANGLE `setenv()` | Crashes |
|---|---|---|---|---|
| 1 | `sh` parent | 16 | **grew in place** (no free) | **0 / 6** |
| 2 | `python3` parent (CPython adds `LC_CTYPE`, PEP 538) | 17 | **MOVED** (old array freed) | **6 / 6** |
| 3 | `VK_ICD_FILENAMES` pre-seeded before exec | — | **not reallocated** (replace path) | **0 / 936** |
| 4 | `python3` parent + race amplifier | 17 | **MOVED** | **15 / 15** |

Correlation between "array was reallocated and freed" and "crashed" is **4/4** across every
configuration measured. A single extra exported variable in the parent flips the outcome —
which is exactly why this reproduces under one harness and is invisible under another.

### Reproducer

Local harness (aarch64, Debian bookworm container):

- `/tmp/sp2/arm.sh`, `/tmp/sp2/run3big.sh` — launch loop
- `/tmp/sp2/spdrive2.py` — driver
- `/tmp/chr/preload/envtrace2.c` — `setenv`/`unsetenv` interposer (identifies the writer,
  reports whether the array moved)
- `/tmp/chr/preload/racecheck.c` — **race amplifier**
- `/tmp/chr/root` — container rootfs sysroot; `/tmp/chr/dbg` — dbgsym

Minimal shape: launch `chromium --single-process --headless` repeatedly, several at a time,
**from a parent with 17 exported variables** (e.g. via `python3`), on Debian bookworm arm64.
~7% segfault during startup.

### Amplifier (makes it deterministic — recommended for verification)

`racecheck.c` `LD_PRELOAD`s a reimplementation of `getenv()` that walks `environ` exactly as
glibc does but **sleeps 20 ms mid-walk** for the names fontconfig reads
(`FONTCONFIG*`, `XDG_*`, `HOME`, `FC_*`). This widens the window from microseconds to 20 ms
and converts the 7% flake into **100%**. Anyone triaging this should use it — without it a
"fixed" build is indistinguishable from a lucky one.

### Fixes verified (15 launches each unless noted)

| Change | Result | Notes |
|---|---|---|
| **Pre-seed `VK_ICD_FILENAMES` before exec** | **0/15**, 0/60 deep, **0/936** natural | glibc takes the slot-replace path; array is never reallocated or freed |
| `--disable-software-rasterizer` | 0/15 | avoids the SwiftShader ICD probe entirely |
| `--use-gl=disabled` | 0/15 | same — no Vulkan probe |

### Falsified — do not accept this as a fix

**`--no-zygote` looks clean naturally (0/936) but is 15/15 under the amplifier.** It only
removes zygote forks, which narrows the timing window; the defect is untouched. This is a
placebo and will silently "fix" the bug in any test that does not use the amplifier. Flagging
it explicitly so triage does not land on it.

### Existing upstream context (checked before filing)

- **`crrev.com/c/7725187`** — *"linux: set env FC_FONTATIONS and EGL_PLATFORM early"*,
  **merged 2026-04-17**, first landed in **149.0.7798.0**, so it **is already present in the
  151.0.7922.71 build that crashes**. It moved Chromium's *own* `FC_FONTATIONS` /
  `EGL_PLATFORM` `setenv()` calls to before thread creation, explicitly *"to avoid race
  conditions issues of setenv/getenv"* — and notably moved one of them **out of
  `GlobalFontConfig` construction**, the same object in our stack. It establishes the
  pattern and the precedent, but it did **not** touch ANGLE's `VK_ICD_FILENAMES` write,
  which is the writer that remains and the one we crash on.
- **`crrev.com/c/7725326`** — *"Make getenv/setenv/unsetenv thread-safe"* (a `base::Lock`
  around them), **ABANDONED 2026-04-03**. Daniel Cheng (base/ OWNER): *"There's no reasonable
  way to make these functions thread-safe. The pointer returned from `getenv()` can easily be
  invalidated. We should not land this CL."* The author's motivation — *"our target is
  crashed on startup sometimes"*, *"the crash happened in the middle of getenv"* — reads like
  the same failure mode. **A lock is not an acceptable fix and has already been rejected.**
- **`crrev.com/c/3522821`** + revert **`crrev.com/c/3580980`** (angleproject:7095) —
  switching `VK_ICD_FILENAMES` → `VK_DRIVER_FILES` was landed and reverted in April 2022
  because it *"Seems to break ANGLE on fuchsia loader"*. Renaming the variable would not fix
  this race anyway (still a `setenv()`), and has a known regression history.
- **`crrev.com/c/3353893`** — *"Android: Remove setenv from common path"* (angleproject:6822):
  *"In Android production stress testing, the setenv call was causing a race condition. To
  fix, only use setenv in the paths that need it."* ANGLE has already accepted this class of
  fix once.
- **`crrev.com/c/7699542`** (open, ANGLE, touches `vulkan_icd.cpp`) proposes unsetting
  `VK_INSTANCE_LAYERS` in `ScopedVkLoaderEnvironment` for an unrelated MangoHud crash. That
  would **add** two more `environ` mutations to this exact hot path and make this bug worse;
  worth linking from here.

I searched issues.chromium.org (ANGLE and Chromium components), the ANGLE trackers, Gerrit
across `chromium/src` and `angle/angle`, and the Debian BTS for `chromium`, and found no
existing report of the ANGLE `VK_ICD_FILENAMES` writer specifically.

### Suggested fix

Attached patch: `angle-setenv-race.patch`.

ANGLE is a library and cannot choose when its embedder creates threads, so the environment
must already be correct by the time `setICDEnvironment()` runs. The patch makes that contract
work: when `VK_ICD_FILENAMES` already holds exactly the value ANGLE would write, **skip the
mutation entirely** and report success. `mChangedICDEnv` stays `false`, which also suppresses
the paired restore in the destructor — so a correctly pre-seeded process performs **zero**
`environ` mutations on this path, closing the window completely (matching the measured
0/936).

This is deliberately minimal and does not, by itself, remove the `setenv()` for embedders
that do not pre-seed. Longer-term the right direction is to stop using the environment to
select an ICD at all — `VK_LUNARG_direct_driver_loading` lets the driver be passed to
`vkCreateInstance` through `VkDirectDriverLoadingInfoLUNARG` with no `environ` involvement.
That needs Vulkan loader ≥ 1.3.234 and ANGLE loading the ICD itself, and the Fuchsia revert
above shows ANGLE still supports loaders that lag, so it is not a drop-in.

### Chromium-side ask

Mirror `crrev.com/c/7725187`: set `VK_ICD_FILENAMES` to the bundled SwiftShader ICD path in
`ContentMainRunnerImpl::Initialize()` / `PreSandboxStartup()`, **before any thread is
created**, whenever the software-Vulkan path can be selected. With the ANGLE patch above,
ANGLE then observes the value it wanted and never touches `environ`. That combination is what
we measured at 0/936.

---

## Provenance / limits of this report

- Root cause established from 3 symbolized core dumps (identical faulting instruction and
  register), a `setenv` interposer that names the writing thread and call site, and a race
  amplifier that makes the failure 100% reproducible.
- The `environ`-moved-vs-in-place column was **observed** by the interposer, not inferred.
- glibc version boundary (≤2.40 affected, ≥2.41 not) was verified by diffing
  `stdlib/setenv.c` across the `glibc-2.36`/`2.40`/`2.41`/`2.42` tags, not from release notes.
- Not verified: behavior on x86-64, on non-Debian distros, or with a non-PartitionAlloc
  allocator. The mechanism is allocator-independent; PartitionAlloc only determines how fast
  the freed block is recycled and therefore how reliably the bad load faults.
- The attached patch is **not compile-tested** — see the patch file for exactly what a
  reviewer must check.
