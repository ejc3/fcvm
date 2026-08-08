# Chromium shared-nothing benchmark — agent guide

Read this BEFORE running or changing anything here. Everything below was paid for
with a wrong answer that survived until an adversarial review killed it.

## THE ONE RULE

**A number you cannot defend under adversarial review is worse than no number.**
The 2026-08-07 run produced a full report whose headline claims were then retracted:
memory density, egress-mode ordering, throughput, and part of the file-vs-UFFD gap
all failed review. The measurements were real; the *methodology* made them mean
nothing. Every rule below exists because one of them broke.

## Six methodology defects — do not reintroduce

1. **Matched accounting basis.** Sum memory over the SAME process set for every
   configuration you compare. The retracted claim ("fcvm 129 MiB/req beats a warm
   container pool at 151") summed PSS over `firecracker` processes only, while the
   pool comparator summed an entire cgroup — it excluded the `fcvm` process, the
   `unshare` holder, `pasta`, and the page cache holding memory.bin. On a matched
   whole-machine basis the difference vanished and both *lost* to the pool.
   Report at least two independent bases (per-clone cgroup/PSS **and** whole-machine
   `MemAvailable` delta from a quiescent baseline) and **reconcile them**. A 2x gap
   between bases is a finding, not a rounding error.

2. **Interleave, never block.** Modes/configs must be shuffled request-by-request
   from a recorded seed, all serves running concurrently. The retracted egress
   comparison ran each mode in its own ~47 s block, so mode effect was perfectly
   confounded with wall-clock drift — a pure-orchestration probe that never touches
   egress drifted 631 → 706 ms across those same blocks. Include a **drift control
   probe** (a request that exercises none of the varied dimension) so drift is
   measurable and removable.

3. **The burst is the experimental unit.** Clones inside one burst share a 3-second
   window and one instantaneous machine state — they are pseudoreplicates, not n=16.
   Repeat bursts (>= 5) and compute CIs over bursts. Also: the retracted report led
   with a burst figure (3.98 req/s) that its own sustained phase contradicted
   (7.7 rps, 0 skipped, box not saturated). If two phases disagree, that disagreement
   is the result — explain it, don't pick the prettier one.

4. **`RUST_LOG=fcvm=debug`, always.** At `info` the exec client's retry ladder is
   invisible, which made ~70 ms of a 310 ms file-vs-UFFD gap *unresolvable* — fcvm's
   own 100 ms polling was indistinguishable from real guest latency (the UFFD arms
   sat in a suspiciously empty 100–139 ms band). The ladder now starts at 5 ms and
   logs attempts + cumulative wait: USE those lines to attribute every wait.

5. **Slopes need intercepts.** `req/GB = 1024/slope` discarded intercepts that
   differed by 330 MiB, so the claimed ordering only held asymptotically and
   *inverted* at realistic concurrency. Report slope AND intercept with uncertainty,
   state the N range measured, and give req/GB at concrete N.

6. **Quote uncertainty; round to it.** "129.0 MiB" on data carrying ±20–70 MiB is a
   false claim dressed as precision.

## Environment traps (each cost real time)

- **`/mnt/fcvm-btrfs` is ephemeral instance-store.** A stop/start wipes it: rootfs,
  kernels, initrd, snapshots, image-cache — all gone. Recovery is one command,
  `make setup-fcvm` (~5 min; it boots a VM to build the 10 GB Layer-2 rootfs and
  waits for `FCVM_SETUP_COMPLETE` on the serial console). Golden snapshots must be
  rebuilt after. Verified 2026-08-08 after a nightly restart.
- **This box is shared with other agents.** Before measuring: check `uptime` and
  `pgrep -c firecracker`. Contention silently inflates every number. If load is
  non-trivial, say so in the report or re-run — do not quietly publish.
- **After a reboot the page cache is cold.** Discard warmup iterations EXPLICITLY
  and say you did; otherwise cold-cache runs inflate p50.
- **`scripts/ci-stray-vm-guard.sh` is DESTRUCTIVE** — it SIGKILLs everything matching,
  with no interlock against a concurrent job. An agent ran it "to exercise it" and
  killed another agent's live test. Use `--dry-run`. Never run it on a shared box.
- **Poisoned CI caches are real.** A runner that dies mid-write with
  `CACHE_ON_FAILURE: true` persists a truncated cache that later restores and makes
  `cargo build` segfault in 70 ms. If a build fails impossibly fast, check cache
  sizes against a known-good ref before blaming the branch.

## Chromium findings worth preserving

- **`VK_ICD_FILENAMES` pre-seed in `entry.sh` is LOAD-BEARING. Do not delete it.**
  It works around an upstream ANGLE bug ([issues.angleproject.org/issues/543664586](https://issues.angleproject.org/issues/543664586)):
  `ScopedVkLoaderEnvironment` calls `setenv()` after threads exist, racing fontconfig's
  `getenv()`; glibc `realloc`s the environ array and frees it under the reader →
  SIGSEGV during early init. Only exposed under `--single-process`/`--in-process-gpu`.
  Root-caused from a symbolized core dump; report + patch in `upstream/`.
- **THE PARITY TRAP.** That race depends on whether glibc's realloc happens to grow
  in place. Adding *any* env var flips parity, so a clean natural run is **NOT**
  evidence of a fix — adding one dummy var gave 0/336 failures, statistically
  identical to the real fix, and 15/15 failures under amplification. Same trap
  killed a `--no-zygote` "fix". **Verify race fixes with a race amplifier**
  (a `getenv` that sleeps mid-walk), not with natural rates.
- **RSS lies about shared memory.** `--single-process` looked like it saved ~460 MB
  on RSS; on PSS the real saving was ~33 MiB idle, and it was *worse* than
  site-isolation-off while actively rendering. RSS counts each shared page once per
  process, and the baseline runs 10–11 processes vs 5. Use PSS.
- **PNG encoding, not rasterization, dominates screenshot cost** (Chromium devs
  profiled it; CDP's `optimizeForSpeed` exists for this). JPEG q80 measured −40%
  screenshot, −21% whole request in-VM.
- **`--deterministic-mode` is a trap on aarch64** — it bundles
  `--disable-skia-runtime-opts`, disabling NEON in exactly the raster/encode path
  you care about. Take its sub-flags selectively.
- **`--disable-software-rasterizer` silently kills WebGL** while CDP still reports
  success and the screenshot stays non-uniform (`"WebGL 2.0"` → `"no-webgl"`). A
  blank-screenshot detector will not catch it. Rejected for that reason.
- `--headless=old` no longer exists (removed in M132).

## fcvm internals found while building the request path

These came out of a resident-request-server design that was **superseded** by the
chosen one (expose Chromium's CDP port directly and drive it from the host — see
`entry.sh`). The design is gone; these two facts about fcvm outlive it.

### fcvm's vsock port map, and where a new port may go

Verified against source, not guessed:

| Port | Purpose | Defined in |
|---|---|---|
| 4995 | boot plan | `fc-agent/src/bootplan.rs`, `src/commands/common.rs` |
| 4996 | TTY exec | `fc-agent/src/tty.rs`, `src/commands/common.rs` |
| 4997 | output stream | `fc-agent/src/vsock.rs`, `src/commands/podman/listeners.rs` |
| 4998 | exec | `fc-agent/src/vsock.rs`, `src/commands/exec.rs` |
| 4999 | container status (`ready`, `exit:{code}`) | `fc-agent/src/vsock.rs`, `src/commands/common.rs` |
| 5000 + N | volume servers, one per volume | `src/commands/common.rs::VSOCK_VOLUME_PORT_BASE`, `src/volume/mod.rs` |
| 52000 | egress proxy | `src/network/egress_proxy.rs`, `fc-agent/src/vsock.rs` |

**Rule for a new guest-side vsock port: pick above the volume range and far below
52000.** The volume block is open-ended (one port per volume, growing upward from
5000), so anything at 5001–5010 is a future collision; anything near 52000 crowds
the egress proxy. The host side of any port is a `{uds_path}_{port}` Unix socket
next to `vsock.sock` — the same `{uds_path}_{port}` convention for both Firecracker
and Cloud Hypervisor.

### fc-agent's 2 s optimistic-accept fallback is a mio artifact, not a vsock law

`fc-agent/src/vsock.rs::accept()` wakes every 2 s to retry a non-blocking `accept4`,
because a vsock connection delivered while the VM is PAUSED can sit in the accept
queue with no readiness edge ever arriving after resume (#617). That is a real bug
and the fallback really does fix it — **but only because tokio's registration is
edge-triggered**. `mio`'s `interests_to_epoll()` starts `let mut kind = EPOLLET;`
(`mio-1.2.2/src/sys/unix/selector/epoll.rs:130`) and there is no way to ask it for
a level-triggered registration.

Under **level-triggered** epoll the failure mode cannot occur: `epoll_wait` reports
a non-empty accept queue on *every* call regardless of edges, so a queued-but-edgeless
connection is delivered on the next wait with no timer involved. Confirmed by
construction in a prototype that registered the same listener with Python's
`select.epoll` (level-triggered by default — no `EPOLLET`) and needed no fallback.

**So this is a live sleep-removal candidate the sleep audit did not catch**: register
that one listener level-triggered (raw `epoll_ctl`, or an `AsyncFd` alternative that
does not force `EPOLLET`) and the 2 s wakeup — plus its worst-case 2 s tail on the
lost-edge path — deletes itself. Do NOT just delete the fallback: with mio as-is it
is load-bearing. Fix the registration first, then prove it with the #617 repro.

## fcvm reference numbers (main @ 2026-08-08, quiet box)

Compare against these; a large deviation means contention or a regression.

| Metric | Value |
|---|---|
| clone spawn → exec-ready | **132 ms p50** (min 121, p90 133, n=10) |
| — `resolve-fc` | 0.5 ms (was 341 ms: per-clone `git ls-remote`) |
| — `listeners+state` | 2.5 ms |
| — `netns+pasta` | 38.0 ms (was ~96: inotify replaced a 50 ms poll) |
| — `snapshot-load` | 5.8 ms — the actual Firecracker restore primitive |
| — `resume→ready` | 79.3 ms (was 220: a 201 ms sleep that guarded nothing) |
| routed first-egress after restore | 0.7 ms (was 1002.5 — IPv6 DAD) |
| reflink, 12.8 GB disk | 9.4 ms (tracks extent count; the published 1.5 ms does not generalize) |
| host-native warm Chromium floor | ~245 ms |

`--exec` runs in-container via podman entry (~94–100 ms) vs `crun exec` (~4 ms) —
known, unfixed, and a real component of any end-to-end number.

### CDP handshake is now a PER-REQUEST cost — measure it, don't drop it

Under the chosen design (Chromium's DevTools port exposed and driven from the host)
the CDP connect happens on every request. Under the superseded resident-server
design it would have happened once, before the snapshot. It is therefore the one
cost that design would have removed, and it must be reported rather than quietly
dropped. `render.py`'s `connect` stage — `/json/list` + TCP + the RFC 6455 upgrade —
is what to look at.

**These are scavenged from existing logs, NOT a benchmark.** They are here so the
number is not lost and so a proper measurement has something to disagree with. All
three are the *in-guest* connect to `127.0.0.1:9222`, so they exclude the host↔guest
hop that the chosen design adds; a host-driven connect over fcvm port forwarding
will be **larger** than every figure below.

| Source | n | p50 | p90 | max |
|---|---|---|---|---|
| host container, no VM (`scratchpad/cb/*.jsonl`) | 1332 | 3.5 ms | 10.4 ms | 15.4 ms |
| restored clone, quiet box (`scratchpad/cb/vmlogs/clone-*.log`) | 104 | 10.7 ms | 13.8 ms | 15.4 ms |
| restored clone, under benchmark load (`results/20260808-corrected/requests/*.log`) | 1138 | 19.1 ms | 30.5 ms | 39.2 ms |

The single `connect_ms=13.3` sample that has been quoted around this work is one
draw from row 2 (`clone-base-png-medium-1.log`) — roughly its p85, and **not** a
container-local figure. Do not cite it as a point estimate.

Note the load sensitivity: p50 nearly doubles from quiet box to loaded box. Any
per-request CDP handshake number is only meaningful with the concurrency stated
next to it. `results/…` rows exclude the `url=noop` drift-control probe, which
prints a hardcoded `connect_ms=0.0`.

## The request-optimized path (CDP direct + fast teardown)

The measured 573 ms request breaks into ~145 ms of request-INDEPENDENT process
startup, 226 ms of actual render, and ~154 ms of teardown that runs AFTER the
screenshot already exists. The hypervisor restore is 4 ms. Two changes attack the
two non-render parts; they are separate arms in `reqbench.py` so they can be
attributed separately.

**Chromium's CDP endpoint IS the request server.** Do not wrap it. An earlier
design put a resident Python server in the guest speaking a bespoke JSON protocol
over vsock; it was deleted before it ran. CDP already returns the screenshot as
base64 in the `Page.captureScreenshot` response, already carries metadata, and is
already specified. Wrapping it adds a resident interpreter to every clone, a
custom wire format to debug, and — the expensive part — a
`VIRTIO_VSOCK_EVENT_TRANSPORT_RESET` problem to solve, since a snapshot restore
invalidates guest vsock sockets. **Chromium's listener is ordinary in-guest TCP,
which a restore does not touch, so that entire problem disappears by not being
created.**

### Reaching CDP from the host — THREE measured traps

All three were found by verifying instead of assuming, on 2026-08-08, with a
podman-only reproduction (no VM). Each fails SILENTLY.

**1. `HEALTHCHECK` is dropped by podman's default OCI image format.**
`podman build` prints `HEALTHCHECK is not supported for OCI image format and will
be ignored` as a *warning* and succeeds. The image then has no healthcheck, fcvm's
health gate never sees one, and the golden snapshot triggers on the wrong
condition. **Build with `--format docker`.** Verify:
`podman inspect <img> --format '{{json .HealthCheck}}'` must print the Test array.

**2. `--remote-debugging-address=0.0.0.0` IS IGNORED by chromium 151.0.7922.71
(Debian bookworm arm64).** The flag is present in `/proc/<pid>/cmdline` and
`/proc/net/tcp` still shows `127.0.0.1:9222` and nothing else. Reproduced with a
minimal `chromium --headless=new --no-sandbox --disable-gpu
--remote-debugging-address=0.0.0.0 --remote-debugging-port=9222`, so it is the
build, not our flag set. **The failure mode is a TCP connect that succeeds and is
then RESET** — which reads exactly like the Host-header rejection everyone warns
about, and is not it. Do not spend an hour on Host headers: check
`/proc/net/tcp` inside the container FIRST.

**3. `--publish` cannot work on its own, and `--forward-localhost` DOES NOT DO
WHAT ITS NAME SUGGESTS.** Since Chromium binds guest loopback and fcvm runs guest
containers `--network=host` (`fc-agent/src/container.rs`), `--publish 9222:9222`
forwards to the guest's eth0 address where nothing listens.

**`--forward-localhost` is GUEST -> HOST, not host -> guest.** This was written
here the wrong way round on 2026-08-08 and cost a full golden-snapshot build to
discover. The flag's own help says it: *"Enables containers to reach host-only
services via localhost"* (`src/cli/args.rs:236`). The guest side
(`fc-agent/src/network.rs:479 setup_localhost_forwarding`) makes the guest's
`127.0.0.1:<port>` dial **`10.0.2.2:<port>`**, the host gateway; the host side
(`src/network/routed.rs:715-741`) listens on that alias inside the namespace and
relays to the HOST's `127.0.0.1:<port>`.

Pointing it at 9222 does not merely fail to help — it **HIJACKS the guest's own
loopback CDP port**, so Chromium's readiness probe inside `entry.sh` is redirected
to the host, finds nothing, and the container exits 1. Observed symptom:
`fcvm::network::tcp_proxy: localhost forward connect failed error=connecting to
host loopback peer=10.0.2.100:<port>` repeating, then
`ERROR: chromium-cdp not answering ... after 300 tries`.

**There is NO fcvm feature today that exposes a guest-LOOPBACK port to the host.**
Not routed, not rootless, not bridged. Adding one is a real fcvm change with its
own PR and review; it must not be faked in a bench harness.

The way that works today, measured, is a relay inside the container plus ordinary
`--publish`:

| Hop | Mechanism | Cost |
|---|---|---|
| host -> guest eth0:9223 | `--publish 9223:9223`; clones inherit `port_mappings` from snapshot metadata (`src/commands/snapshot.rs:1001/1051/1070`) | measured in `port_wait_ms`, 0.12 ms |
| guest eth0:9223 -> guest loopback:9222 | `socat TCP-LISTEN:9223,fork` in `entry.sh` | **2.0 MiB PSS**, 0.59% of container PSS |

Verified working on `--network rootless` (no root needed). `reqbench.sh` defaults
to rootless for this reason.

### The health gate is the snapshot trigger

With no `--health-check` URL, fcvm's `Healthy` = container running AND podman's
`HEALTHCHECK` healthy (`src/health.rs`, "AND logic"). So the image's HEALTHCHECK
decides what gets frozen. `cdp_health.py` requires BOTH a warm marker (entry.sh
touches it only after a full navigate+screenshot) AND a live CDP round trip that
finds a page target. Healthy therefore means *provably able to screenshot*, not
*port is open*. Caveat to verify: podman healthchecks need systemd timers in the
guest; `src/health.rs` notes they can fail to schedule in some rootless setups.

### Fast teardown: one signal, kernel-enforced, cannot leak

`kill(fcvm, SIGKILL)` is the whole teardown. fcvm spawns Firecracker and the
namespace holder with `PR_SET_PDEATHSIG=SIGKILL`, so the kernel's
`forget_original_parent()` pass delivers SIGKILL to **both, concurrently, in one
pass** — no ordering to get wrong, and no cleanup code of ours that must survive a
SIGKILL. This is the chain PR #730 restored after ~490 VMs leaked.
Regression proof: `test_sigkill_kills_firecracker_rootless` (podman-run flavour)
and `test_bench_fast_teardown_leaks_nothing_clone` (the clone flavour the bench
actually uses, via the snapshot-restore spawn path).

Because SIGKILL cannot be caught, `cleanup_vm` never runs and the state file and
data dir survive. `reqbench.py` reaps them **synchronously** after the clock
stops — no janitor. The clone-path test asserts the state file is still there
after the kill, precisely so nobody deletes that reap step.

### Teardown is NOT free — measured, not asserted

Kernel address-space reclaim, measured VM-free on this box (64 cores, 125 GB) with
the same parent+pdeathsig topology, median of 3 per size, `/proc/<pid>/stat` read
in the `Z` state so the figure is complete (`exit_mm()` runs before
`exit_notify()`, so a zombie-state read already includes all reclaim):

| Address space | reap wall | reclaim CPU |
|---|---|---|
| 256 MiB | 15.2 ms | 30 ms |
| 512 MiB | 32.2 ms | 50 ms |
| 1024 MiB | 63.5 ms | 110 ms |
| 2048 MiB | 121.4 ms | 180 ms |

≈ **62 ms wall/GiB, ≈ 90–120 ms CPU/GiB**, linear. CPU EXCEEDS wall, so reclaim
runs on more than one core: moving it off the response path does not make it free,
it makes it concurrent. **The claim "early response converts teardown from LATENCY
into THROUGHPUT cost" is supported — "converts", never "removes".** At saturation
that ~110 ms CPU/GiB competes with new requests. Do not report the latency win as a
capacity win.

Caveat: synthetic dirty-anon pages, not Firecracker's MAP_PRIVATE file-backed
guest memory; clean file-backed pages may unmap cheaper. Corroboration: the
observed 78 ms VM teardown for a ~1 GB VM sits right on the 62 ms/GiB line.

## Claims currently REFUTED — do not re-publish without new data

- "fcvm beats a warm container pool on marginal memory (129 vs 151 MiB)"
- any egress-mode *ordering* (all modes collapsed to ~1.30 ± 0.06 s under review)
- "16 clones sustain 5.5–6.3 req/s" (n=1 bursts; sustained data said 7.7 rps)
- "7.9 vs 5.3 req/GB" (slope without intercept)
- ~70 ms of the 310 ms file-vs-UFFD gap (unattributable at `RUST_LOG=info`)

`REVIEW.md` is the ledger of what holds and what doesn't. **Update it every run.**

## Deliverables

The end product is a **readable markdown benchmark with inline visualizations**, in
the spirit of `~/src/editor-loop-bench/SUMMARY.md` (read it before writing). Match the
*idea*, not the literal format:

1. **Lead with the one idea.** That report opens with a conceptual split (O(repo) vs
   O(closure)) that makes every later number obvious. Find ours and state it first —
   a table of numbers with no thesis is a data dump, not a benchmark.
2. **Machine + versions in the first screenful**, with a pointer to the raw file that
   records them (`hostinfo.json`). Reader must be able to tell what hardware this was.
3. **Every figure traceable to a raw record** — cite the json file (and the cell) next
   to the table it came from. Extrapolations **labeled as extrapolations**.
4. **Tables organized by the question a reader has** (per-request cost, density,
   throughput, mode comparison), not by the order you ran the phases.
5. **Charts inline.** `charts/*.svg`, referenced from the markdown. **Load the
   `dataviz` skill BEFORE writing any chart code** — it is not optional, and it covers
   palette, light/dark, and stat tiles so the set reads as one system.
   Best candidates here: the per-request stage-breakdown stack, the memory-vs-concurrency
   regression (show intercept AND slope, with a CI band), the utilization→throughput
   curve from the scalability run, and a mode-comparison interval plot that shows
   overlapping CIs honestly rather than implying an ordering.
6. **Publish an artifact** (HTML via the Artifact tool) for the visual version, and
   keep the markdown as the in-repo source of record. Self-contained, theme-aware.
7. **`REVIEW.md` is a first-class deliverable**, not an appendix: what holds, what was
   refuted, what remains unmeasured.

`results/` is gitignored — commit the harness, the charts, and the findings, not the
raw output. Uncertainty goes in the tables, not just the prose.
