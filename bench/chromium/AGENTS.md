# Chromium shared-nothing benchmark — agent guide

Read this BEFORE running or changing anything here. Everything below was paid for
with a wrong answer that survived until an adversarial review killed it.

## THE ONE RULE

**A number you cannot defend under adversarial review is worse than no number.**
The 2026-08-07 run produced a full report whose headline claims were then retracted:
memory density, egress-mode ordering, throughput, and part of the file-vs-UFFD gap
all failed review. The measurements were real; the *methodology* made them mean
nothing. Every rule below exists because one of them broke.

## Running the gated request benchmark end to end

The phases are separate make targets and the order is not optional. The
dependency graph is IN the Makefile now, so make — not the operator — walks
it: the golden depends on the container image build and on `setup-default`
(kernel, rootfs, initrd, firecracker; their absence is the runtime failure
"Custom firecracker not found ... Run: fcvm setup"), and the image build
depends on the fcvm binary build. Drive the phases through make; the raw
`bash bench/chromium/reqbench.sh <phase>` route skips the dependency graph
and is how goldens died on half-set-up boxes (2026-08-13, twice in one day):

```bash
cd ~/src/fcvm
make bench-chromium-request-golden       # deps: fcvm build -> image build -> setup-default -> golden
make bench-chromium-request-verify       # prove all three hops on a RESTORED clone before measuring
make bench-chromium-request-diag DIAG_EXPECT_IPS=127.0.0.1 DIAG_MAX_LOAD_MS=15000   # what holds the load event, per clone
make bench-chromium-request-run BACKEND=uffd REPS=200 RESULTS=/mnt/fcvm-btrfs/reqbench-uffd-200
make bench-chromium-request-run BACKEND=file REPS=200 RESULTS=/mnt/fcvm-btrfs/reqbench-file-200
```

Knobs pass as make command-line variables (make exports them to the recipe
environment): `TAG=`, `HUGEPAGES=1`, `NETMODE=`, `UFFD_MODE=`,
`UFFD_PREFETCH=`, `REPS=`, `WARMUP=`, `ARMS=`, `RESULTS=`, and for the diag
`DIAG_URLS=`, `DIAG_REPS=`, `DIAG_EXPECT_IPS=`, `DIAG_MAX_LOAD_MS=`. Hugepage goldens
are part of the snapshot identity — give them their own tag
(`make bench-chromium-request-golden TAG=cb-req-golden-huge HUGEPAGES=1`;
the same `TAG=` must then be passed to `-verify` and `-run`, or they select
the default snapshot).

`GUEST_ENV=` (golden only) bakes extra container environment into the
snapshot: comma-separated `KEY=VALUE` entries, one `fcvm podman prepare
--env` each, recorded as `guest_env` in `reqbench-provenance.json`, carried
into every run's meta and analysis cell (a golden whose provenance lacks it
is refused, like one without `guest_dns`), and treated by campaign_summary
like a baked resolver: a run with a non-empty `guest_env` needs its
`diag/summary.json` to be indexed. The
resolver-rule A/B is the one use today:
`make bench-chromium-request-golden TAG=cb-req-golden-resolve GUEST_ENV=BENCH_RESOLVE_ALL_TO=10.0.2.2`
makes `entry.sh` launch Chromium with
`--host-resolver-rules=EXCLUDE localhost, EXCLUDE 127.0.0.1, EXCLUDE ::1, MAP * 10.0.2.2`
as one argv element (the knob is the IP alone because the rule holds a space,
which the container env word-split when the whole flag was passed). The
exclusions keep the container's own warmup page, which entry.sh navigates at
`http://127.0.0.1:8000/warmup.html` before it writes the ready marker, off
the map: Chromium maps before it resolves, so without them that navigation
goes to `10.0.2.2:8000` and the container never becomes healthy. An IPv6
knob is emitted bracketed (`MAP * [fd00::2]`), the only spelling Chromium's
rule parser reads as an address, and a scoped address (`fe80::1%eth0`) is
refused at the knob. The
entries change what the snapshot does, so such a golden needs its own `TAG=`.
The host control takes the same variable directly:
`make bench-chromium-hostcdp BENCH_RESOLVE_ALL_TO=10.0.2.2`, recorded as
`resolve_all_to` in its `run.json` (null when unset).

`bench-chromium-request-diag` (and its `bench-webkit-request-diag` twin)
answers what holds a page's load event inside a restored clone, on the
golden the run uses, on the run's backend and UFFD mode (`BACKEND=`,
`UFFD_MODE=`, `TAG=`), without a measured arm. Its serve always runs
`--uffd-prefetch off`, and a `UFFD_PREFETCH=` set to anything else is
refused: with replay on, the server would record the diag's renders into
`memory.bin.working-set` beside the golden, the file the measured run
replays, so the run would restore the diag's working set instead of the
golden's own. One clone per URL in
`DIAG_URLS=` (comma-separated; default the run's URL) times `DIAG_REPS=`
(default 3): clone, one render, teardown. On Chromium the render is
cdpdrive.py with `--net-trace`, which the measured arms never send; WebKit
renders through wddrive without a trace. Each render's record and trace land
under `$RESULTS/diag/`, and `summary.json` there carries, per URL, the render
count, the slowest load event, the most requests still open at the load
event, every remote IP with its request count and every failure text, plus a
violations list, `passed`, `runtime_bundle_intact`, `uffd_prefetch` (`"off"`;
`null` on the file backend, which has no serve), the
`snapshot_generation_id` and `snapshot_config_sha256` of the snapshot it
diagnosed, and `runtime_bundle_sha256`, the sealed runtime it rendered from
(the hash the measured run records; `null` when the phase ran outside a
staged bundle). The phase exits non-zero, and `passed` is false, on any remote IP
outside `DIAG_EXPECT_IPS=` (comma-separated, when set), a trace naming no
remote address at all while it is set, any `net::ERR_NAME_NOT_RESOLVED` in a
trace, any load event over `DIAG_MAX_LOAD_MS=` (when set), any failed render
(a record for another URL, one not saying ok, one written under a non-zero
driver exit status, or one without a load event timing), any clone whose
teardown was not clean, and a sealed bundle that changed during the phase.
Each render record keeps the driver's exit status as `driver_status`. One
diag owns `$RESULTS/diag` at a time: the phase takes an exclusive lock on
`$RESULTS/diag/.lock` before it removes anything and holds it past the
summary's rename, and a second invocation over the same `RESULTS` waits
`DIAG_LOCK_WAIT` seconds (default 60) and then refuses with exit 3 rather
than interleaving records with the holder. The generation lock does not
serialize them: two `TAG`s are two different locks over one output
directory. Stale `.tmp` records from an invocation that died mid-render are
swept under that lock, since diag_render adopts a `.tmp` that parses as an
object. A knob is checked before the generation lock and the hugepage pool
are touched, and the webkit diag refuses `DIAG_EXPECT_IPS=` because its render
has no trace to hold it to. The corpus campaign (`make bench-chromium-corpus`)
runs it after the verify that follows the golden, with every corpus URL,
`DIAG_EXPECT_IPS=10.0.2.2` on Chromium and `DIAG_MAX_LOAD_MS` defaulting to
15000 (the campaign refuses a `DIAG_MAX_LOAD_MS` above `STALL_MAX_MS` before
building anything, since the index refuses the diag that would produce),
and refuses to measure when it fails; `DIAG_ONLY=1` stops the
campaign after golden, verify and diag, for a throwaway golden round, and
`DIAG_REPS=` passes through to the diag. The campaign hands the diag
`UFFD_PREFETCH=off` whatever the measured run's setting, so the diag's
renders (every corpus URL, three clones each) do not warm the sidecar the run
replays and a fresh golden still measures as one. campaign_summary.py
refuses a corpus run without its `diag/summary.json`, a summary that names
another snapshot generation, config, tag, engine, backend or UFFD mode than
the run's analysis cell, one whose `runtime_bundle_sha256` is not the run's
sealed bundle or is absent (a standalone diag staged from edited sources
leaves a summary that is intact and matches every snapshot field, and it
rendered with other code), one whose `uffd_prefetch` is not `"off"` (`null`
on the file backend), including a summary from before the field existed,
and one whose `limits` were not armed for the run: `passed` is true when
nothing the diag was asked to check went wrong, and a standalone diag over
the same `RESULTS` with the knobs unset replaces the campaign's summary with
one that allowed every remote address and held no load event to a limit. So
on Chromium `limits.expect_ips` must be exactly the address set the run's
records name (the verify brackets' resolver answers, `BENCH_RESOLVE_ALL_TO`,
and any IP-literal URL host; a run whose records name no address cannot hold
a diag to anything and is refused), on WebKit it must be `null`,
`limits.max_load_ms` must be a positive integer no larger than the run's
`stall_gate.max_ms`, and every measured URL must have rendered `reps` times.

`verify`, `diag` and `run` deliberately have NO build dependency: reqbench.sh
seals fcvm + fc-agent + its five sources into a hash-bound runtime bundle, and
the run refuses a golden whose provenance records a different bundle hash.
Rebuilding — or editing any sealed file — between golden and run therefore
invalidates the chain; regolden instead of fighting the seal. The structural
pin for all of this is `MakefileBenchGraph` in `test_reqbench.py`
(`make test-chromium-request`).

The acceptance gate is >=200 measured requests PER BACKEND (`uffd` and `file`)
at zero failures. For anything long-running on a remote box, write the chain
into a script file, run it detached with output to a log, and append an
explicit `RC=$?` marker line the poller can key on — a dropped ssh/SSM session
must not kill the run, and "the log stopped growing" is not a completion
signal. Fresh-box prerequisites (packages, NVMe store, sibling checkouts) are
in the repo-root AGENTS.md quickstart.

To withdraw a run after the fact, add a file named `WITHDRAWN` to its results
directory whose first line is the reason; `campaign_summary.py` refuses the run
and quotes that line, and refuses an `analysis.json` carrying `"withdrawn":
true` the same way. The marker is tracked (`!results/**/WITHDRAWN` in
`bench/chromium/.gitignore`) and is never removed, because a withdrawn run
stays unquotable (REVIEW.md).

A corpus campaign's run directory also keeps the replay server's two logs,
`corpus-dns.log` and `corpus-access.log`, tracked because `dns-evidence.json`
records their sha256 and `campaign_summary.py` refuses the run unless both are
present and match. Expect them to be large: the DNS log has one line per query
and the access log one line per HTTP request the server answered, from the
campaign's startup probes through the three verify brackets and every rep of
every arm of the measured run, so it is the biggest record in the directory
and a short one means the server was not serving the whole run.

The evidence is only as good as the checks behind it, and four of them are
easy to get wrong:

- The server's exit status. `corpus_serve.py` exits 1 when a log line could
  not be written, after the response bytes went out, and `sudo -b` discards
  that status. The launch wrapper waits for the server and writes the status
  to `corpus-serve.status` in the run directory; `stop_corpus_serve` waits
  for the file, `write_dns_evidence` records it as `corpus_serve_exit_status`
  and is unclean unless it is 0, and `campaign_summary.py` refuses evidence
  that does not carry 0. A liveness poll cannot see this case.
- The owner samples. `campaign_summary.py` parses every `dns-owner.log` line
  and holds it to the rule the campaign applied at the verdict (owner is
  `serve_pid`, dnsmasq inactive, the load column adds up to `load_samples`
  and `load_max_1min`). The evidence's `first_mismatch: null` is a claim
  about those lines, not proof of them.
- Proxies in the container exec. fc-agent runs every `fcvm exec -c` under
  the host's saved `HTTP_PROXY`/`HTTPS_PROXY`, and `urllib` honours them by
  default, so HOP D's URL probe would fetch the live site through the proxy
  while the hostname check beside it resolved through the replay resolver.
  The probe installs an empty `ProxyHandler`, clears every `*_proxy`
  variable first, and reports what it ignored; `verify-dns.json` carries
  `proxies_disabled` and `run_verify` refuses a bracket without it. Any new
  in-guest probe that opens a URL needs the same treatment.
- The verify brackets themselves. `passed: true` is also what HOP D writes
  when it was given nothing to check, and a bracket is a plain file in the
  run directory. `write_dns_evidence` records each bracket's sha256 as
  `verify_file_sha256`; `campaign_summary.py` rereads each bracket, refuses
  one whose hash moved since the verdict, and holds its contents to the
  run's resolver, `proxies_disabled`, and every host and URL answered
  through that resolver. Evidence carrying no bracket hashes is refused.

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
- **All phases must use ONE podman store.** `build` and `golden` have to run under
  the same identity fcvm runs as. The harness invokes fcvm through `$SUDO`, so with
  `SUDO=sudo` from an unprivileged shell the image is built in the *rootless* store
  and consumed from *root's* — two stores, two different images under one tag. The
  golden phase's identity guard catches it and is right to:
  `snapshot image disk '<...>/67de414c….storage-v2.img' does not match inspected
  cache key '592250fc…'`. That is not a corrupt cache; it is the tag resolving to a
  different image in the store fcvm actually read. Run the whole chain as one user.
- **Rootless podman needs a runtime dir, and says something else when it lacks one.**
  On a fresh box `podman info` fails with `default OCI runtime "crun" not found:
  invalid argument` while `/usr/bin/crun` is installed and executable. `--log-level=debug`
  names the real cause: `Configured OCI runtime crun initialization failed: creating
  OCI runtime exit files directory: mkdir /run/user/1000: permission denied`. A user
  with no login session has no `XDG_RUNTIME_DIR`; `sudo -u <user>` does not create one.
  Fix is `loginctl enable-linger <user>`. Do not go looking for crun.
- **`fcvm setup` must have run, and `build` will not tell you.** Without the fc-agent
  initrd the golden phase dies one second in with `setting up fc-agent initrd:
  fc-agent initrd not found`, after `build` reported success — the two phases check
  nothing about each other.
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
  profiled it; CDP's `optimizeForSpeed` exists for this). JPEG q80 measured
  −28.8% on the screenshot STAGE (−52.9 ms, CI −63.8…−44.8) and −8.3% on the
  whole request (−65.8 ms, CI −83.1…−42.6), n=12, `corrected.json` ->
  `screenshot_format`. This line used to read "−40% screenshot, −21% whole
  request"; both figures are contradicted by the record run and REVIEW.md marks
  the −21% claim **REFUTED AS STATED**. The screenshot is only ~18% of the
  request, which is why a −29% stage win is an −8% request win.
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

### UFFD queue depth is not observable today, and one `read(2)` per fault is why

`drain_events` (`src/uffd/server.rs`) pulls faults with `uffd.read_event()`, which is
one `read(2)` per message, inside a loop bounded by `MAX_EVENTS_PER_BATCH` (128). So
the batch bound is the only depth signal available, and it cannot distinguish "128
queued" from "10,000 queued" — a saturated server looks the same as a busy one. The
userfaultfd crate's `read_events(EventBuffer)` fills a buffer in a single `read(2)`
and returns how many messages came back, which both cuts the syscall count under load
and makes true queue depth histogrammable. Worth doing when UFFD serving is next on
the critical path; nothing in the tree measures it now.

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
"Large" is only decidable against a spread, so every row carries one — a row with
a bare point estimate cannot tell a reader whether their 45 ms is a regression or
a draw. Defect 6 applies to this table as much as to a report.

**Basis for the whole-request row: n=10 draws, one clone at a time, quiet box.**
The decomposed stages below it were recorded in the SAME run but were not
re-sampled per stage, so they carry the whole's dispersion, not their own. They
are quoted to the precision that supports — whole ms — and they sum to ~126 ms
against a whole of 132, a difference well inside the 12 ms spread of the whole.
Do not read the stage split as ten independent measurements.

| Metric | Value |
|---|---|
| clone spawn → exec-ready | **132 ms p50** (min 121, p90 133, n=10) |
| — `resolve-fc` | <1 ms (n=10; was 341 ms: per-clone `git ls-remote`) |
| — `listeners+state` | ~3 ms (n=10) |
| — `netns+pasta` | ~38 ms (n=10; was ~96: inotify replaced a 50 ms poll) |
| — `snapshot-load` | ~6 ms (n=10) — the actual Firecracker restore primitive |
| — `resume→ready` | ~79 ms (n=10; was 220: a 201 ms sleep that guarded nothing) |
| routed first-egress after restore | ~1 ms (n=10; was ~1000 — IPv6 DAD) |
| reflink, 12.8 GB disk | ~9 ms (tracks extent count; the published 1.5 ms does not generalize) |
| host-native warm Chromium floor | ~245 ms (order of magnitude only; n and spread not recorded) |

**One number for the restore primitive.** The `snapshot-load` row above and the
"hypervisor restore" figure in the request-path section below were previously
quoted as 5.8 ms and 4 ms — a 45% disagreement between two point estimates of the
same primitive, in one file, neither carrying a band that would make it a
non-disagreement. They are the same quantity measured in two different runs and
are now quoted once, as **~6 ms**, from the row above. If you need it tighter,
re-measure it and quote an interval.

`--exec` runs in-container via podman entry (~94–100 ms, n and basis NOT recorded
— treat as an order of magnitude, not a measurement) vs `crun exec` (~4 ms, same
caveat). Known, unfixed, and a real component of any end-to-end number.

### CDP handshake is now a PER-REQUEST cost — measure it, don't drop it

Under the chosen design (Chromium's DevTools port exposed and driven from the host)
the CDP connect happens on every request. Under the superseded resident-server
design it would have happened once, before the snapshot. It is therefore the one
cost that design would have removed, and it must be reported rather than quietly
dropped. `render.py`'s `connect` stage — `/json/list` + TCP + the RFC 6455 upgrade —
is what to look at.

**The only auditable figure is the primary cell of the record run.** It is
`corrected.json` -> `primary_cell.stages.r_connect_ms`:

| Source | n | median | 95% CI |
|---|---|---|---|
| restored clone, primary cell, one request at a time (`corrected.json`) | 12 | 16.7 ms | 16.4–16.9 ms |

That is the *in-guest* connect to `127.0.0.1:9222`, so it excludes the host↔guest
hop the chosen design adds; a host-driven connect over fcvm port forwarding will
be **larger**.

**Three scavenged rows were deleted from this table on 2026-08-08.** They quoted
a host-container p50 of 3.5 ms (n=1332), a quiet-box clone p50 of 10.7 ms
(n=104), and a loaded-box p50 of 19.1 ms (n=1138), and concluded "p50 nearly
doubles from quiet box to loaded box". None of the three is auditable from this
repo: the first two cite `scratchpad/`, which `git ls-tree` shows is not in the
tree at all, and the third cites a `requests/*.log` directory under a `results/`
path that `.gitignore` excludes — so all three violate this file's own
Deliverables rule 3 ("every figure traceable to a raw record"), and the only
comparative claim in the block rested entirely on the two uncommitted ones. The
19.1 ms figure ALSO sat unreconciled next to REVIEW.md's 16.7 ms (16.4–16.9) for
the same stage in the same PR — two point estimates of one quantity, the exact
defect the `snapshot-load` entry below was written about. They are different
populations (all cells and concurrencies under load, vs the primary cell one at a
time), which is a fine explanation that neither document was making. If the
load-sensitivity claim matters, re-measure it and commit the record.

`results/…` figures exclude the `url=noop` drift-control probe, which prints a
hardcoded `connect_ms=0.0`. Any per-request CDP handshake number is only
meaningful with the concurrency stated next to it.

## The request-optimized path (CDP direct + fast teardown)

Every figure below is `corrected.json` -> `primary_cell.stages`, n=12, cell
`rootless-proxy`/`uffd-4k`/`medium`/JPEG q80, with its 95% CI:

- **request-independent startup ~305 ms** — restore 52.9 (52.0–54.0) + fcvm exec
  handshake 28.0 (27.0–29.5) + guest command start 224.5 (217.0–232.0).
- **render ~356 ms** — CDP handshake 16.7 (16.4–16.9) + page load 204.0
  (196.6–207.3) + screenshot 133.8 (120.8–144.2) + DOM 1.8 (1.6–2.1).
- **teardown 175.1 ms (150.4–194.9)**, entirely AFTER the artifact exists.
- **total 890.6 ms (869.6–928.9)**; artifact 730.1 (708.4–741.1).

So startup is ~34% and teardown ~20% of wall clock — both attackable, which is
the design argument. Two changes attack the two non-render parts; they are
separate arms in `reqbench.py` so they can be attributed separately.

*This paragraph used to read "the measured 573 ms request breaks into ~145 ms of
request-INDEPENDENT process startup, ~226 ms of actual render, and ~154 ms of
teardown". Three defects: `git grep 573` found exactly two hits — this line and a
comment in `reqbench.py` citing this line, a circular citation and not evidence;
145 + 226 + 154 = 525, forty-eight milliseconds short of 573, unattributed and
unmentioned; and REVIEW.md L3 says "Quote only from `20260808-corrected`", whose
primary cell is 890.6 ms total, not 573. No committed artifact on this branch
produces 573 ms.*

The hypervisor restore is **~6 ms** — the single `snapshot-load` figure from the
reference table above; this line used to say 4 ms and the table 5.8 ms, which was
two point estimates of one quantity disagreeing by 45% with no interval between
them.

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
condition. **Build with `--format docker`.** Verify with a command that FAILS —
"must print the Test array" is an instruction to a human, inside a block designed
to be pasted, and it exits 0 either way. This is the check `reqbench.sh` already
gates on, so there is one gate to keep correct rather than two:

```bash
podman image inspect <img> --format '{{json .HealthCheck}}' \
  | grep -q health_state || { echo 'FATAL: image has no HEALTHCHECK (OCI format drop?)'; exit 1; }
```

**2. `--remote-debugging-address=0.0.0.0` IS IGNORED by chromium 151.0.7922.71
(Debian bookworm arm64).** The flag is present in `/proc/<pid>/cmdline` and
`/proc/net/tcp` still shows `127.0.0.1:9222` and nothing else. Reproduced with a
minimal `chromium --headless=new --no-sandbox --disable-gpu
--remote-debugging-address=0.0.0.0 --remote-debugging-port=9222`, so it is the
build, not our flag set. **The failure mode is a TCP connect that succeeds and is
then RESET** — which reads exactly like the Host-header rejection everyone warns
about, and is not it. Do not spend an hour on Host headers: check
`/proc/net/tcp` inside the container FIRST.

**3. Eligible `--publish` TCP ports reach guest loopback directly.** Since
Chromium binds guest loopback and fcvm runs guest containers `--network=host`
(`fc-agent/src/container.rs`), fc-agent DNATs the published guest port to
`127.0.0.1:<port>` after installing a first-position INPUT containment rule and
enabling `route_localnet` on `eth0`. See
`fc-agent/src/network.rs::publish_to_loopback`. Port 12345 is excluded, a port
also given to `--forward-localhost` is rejected, and failed safety setup leaves
loopback-only listeners unavailable rather than widening guest ingress.

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

The current request path has no benchmark-owned relay:

| Hop | Mechanism | Cost |
|---|---|---|
| host -> published guest port | `--publish 9222:9222`; clones inherit `port_mappings` from snapshot metadata | measured by `cdpdrive` as `tcp_ms` |
| guest external interface -> guest loopback:9222 | fc-agent PREROUTING DNAT plus `route_localnet` | included in the successful TCP connection above |

The deleted `socat TCP-LISTEN:9223,fork` relay was one process and one byte-path
hop per clone. Do not reuse the old 0.12 ms `port_wait_ms` as an ingress cost:
that timer started only after the restored VM's final PID state save. A later
harness change moved its boundary before network setup and restore, so the same
field then clustered near 50 ms. Use one stable spawn-to-first-connect boundary
for readiness and `tcp_ms` for a successful connection.

Verified working on `--network rootless` (no root needed). `reqbench.sh` defaults
to rootless for this reason.

### The health gate is the snapshot trigger

With no `--health-check` URL, fcvm's `Healthy` = container running AND podman's
`HEALTHCHECK` healthy (`src/health.rs`, "AND logic"). So the image's HEALTHCHECK
decides what gets frozen. `cdp_health.py` requires BOTH a warm marker (entry.sh
touches it only after a full navigate+screenshot and a loader-correlated
`about:blank` lifecycle `load`; render.py then verifies `location.href` and
`document.readyState == complete`) AND a live CDP round trip that finds a page
target. The blank transition is fail-closed: its timeout is not best-effort, and
entry.sh's `set -e` exits before publishing the marker. Healthy therefore means
*warm, quiescent, and provably able to screenshot*, not *port is open*. Caveat to
verify: podman healthchecks need systemd timers in the guest; `src/health.rs`
notes they can fail to schedule in some rootless setups.

### Fast teardown: one signal, kernel-enforced — scope the guarantee, don't blanket it

`kill(fcvm, SIGKILL)` is the whole teardown. fcvm arms `PR_SET_PDEATHSIG=SIGKILL`
on all three long-lived children — Firecracker
(`src/utils.rs::install_namespace_pre_exec`), the namespace holder
(`src/commands/common.rs::spawn_namespace_holder`) and pasta
(`src/network/pasta.rs`) — so the kernel's `forget_original_parent()` pass
delivers SIGKILL to all of them, concurrently, in one pass: no ordering to get
wrong, and no cleanup code of ours that must survive a SIGKILL. This is the chain
PR #730 restored after ~490 VMs leaked.

**This heading used to read "cannot leak". It does not say that any more, because
the guarantee is not uniform across the three hops.** pasta's arming carries a
precondition the other two do not: `commit_creds()` zeroes `pdeath_signal`
whenever uid/gid change or `cred_cap_issubset(old, new)` fails, and pasta
`setns()`es into the holder's user namespace after its `pre_exec`. Under sudo
that is a capability LOSS, the subset test passes, and the signal survives. Run
fcvm genuinely unprivileged and it becomes a capability GAIN, the kernel clears
the signal, and pasta falls back to passt's own 1-second PID watch of the holder.
So: kernel-enforced for the VMM and the holder unconditionally; for pasta while
fcvm runs as root. A harness must not assume any of it — `reqbench.py`'s
`teardown_fast` waits on a pidfd per child and REFUSES to reap on-disk state if
any child is still alive.

Regression proof: `test_sigkill_reaps_rootless_vm_tree` (podman-run flavour) and
`test_bench_fast_teardown_leaks_nothing_clone` (the clone flavour the bench
actually uses, via the snapshot-restore spawn path). Both REQUIRE firecracker,
holder and pasta to be discovered before they assert anything. That is not
pedantry: until 2026-08-08 the clone test held the holder as a bare `Option` and
asserted `!holder.is_some_and(running)`, which is `!false` when discovery returns
`None` — an assertion that cannot fail when its subject is absent, and discovery
is coupled to production by the literal string `sleep infinity`. Verified by
fault injection (holder argv → `sleep 2147483647`): the old test still passed
while a real orphan sat on the box.

Because SIGKILL cannot be caught, `cleanup_vm` never runs and the state file, its
`.json.lock`, and the data dir all survive. `reqbench.py` reaps them
**synchronously** after the clock stops — no janitor. Both clone-path tests assert
the state file AND the data dir are still there after the kill, precisely so
nobody deletes that reap step. The data dir matters on its own: it holds a reflink
of the golden rootfs, so a leaked one pins the golden snapshot's extents on btrfs
and `snapshots delete` frees nothing.

### Teardown is NOT free — measured, not asserted

Kernel address-space reclaim, measured VM-free on this box (64 cores, 125 GB) with
the same parent+pdeathsig topology, median of 3 per size, `/proc/<pid>/stat` read
in the `Z` state so the figure is complete (`exit_mm()` runs before
`exit_notify()`, so a zombie-state read already includes all reclaim).

**Read the CPU column as quantized, not as measured to 10 ms.** `/proc/<pid>/stat`
counts in jiffies and `CLK_TCK` is 100 on this box, so its resolution is 10 ms —
which is why every value in that column is an exact multiple of 10. Each figure is
`value ± 10 ms` from quantization ALONE, before any sampling variance from n=3.

| Address space | reap wall (n=3) | reclaim CPU (n=3, ±10 ms quantization) |
|---|---|---|
| 256 MiB | ~15 ms | 30 ± 10 ms |
| 512 MiB | ~32 ms | 50 ± 10 ms |
| 1024 MiB | ~64 ms | 110 ± 10 ms |
| 2048 MiB | ~121 ms | 180 ± 10 ms |

A 4-point fit over medians of 3, with the CPU column quantized, supports roughly
**60 ms wall/GiB and 80–130 ms CPU/GiB** — a fitted RANGE, not a measurement, and
stated as one. Over most of that range CPU exceeds wall, so reclaim runs on more
than one core: moving it off the response path does not make it free, it makes it
concurrent. **The claim "early response converts teardown from LATENCY into
THROUGHPUT cost" is supported — "converts", never "removes".** At saturation that
CPU competes with new requests. Do not report the latency win as a capacity win.

**What this table does NOT support:** "CPU exceeds wall" as stated for the 256 MiB
row. 15 ms wall against 30 ± 10 ms CPU is 15 vs [20, 40] — the direction holds but
the margin is not resolved by n=3 at this quantization.

Caveat: synthetic dirty-anon pages, not Firecracker's MAP_PRIVATE file-backed
guest memory; clean file-backed pages may unmap cheaper. Corroboration: the
observed ~78 ms VM teardown for a ~1 GB VM sits on the ~60 ms/GiB line.

## Failure-time evidence capture: what the probe takes, and what it cannot say

An 808-clone run produced 3 CDP failures. Every one came from a clone whose
ARP-triggering readiness ping got no reply (5 clones had no reply, 3 of them
failed; of the 803 whose ping replied, none failed), the guest stayed alive for
the whole 100+ seconds, and the `exec` and `noop` arms, which reach the guest
over vsock, never failed. **Why the guest stopped answering on the IP path, and
why it never recovered, is UNSOLVED.** The leading hypothesis, which nothing has
yet proved, is that the guest's post-restore network re-initialisation never
ran or never finished for that clone.

`reqbench.py`'s `FailureProbe` exists because vsock exec keeps working while the
IP path does not, so the broken guest is interrogable at exactly the moment it
is broken, and the harness used to delete it. On a failed CDP request, and once
per run on a healthy one as the control, it writes `<request-name>.probe.json`
next to that request's clone log and references it from the JSONL record.

- Guest, over `fcvm exec --vm`: addresses, routes, the neighbour table,
  interface counters, listening sockets, the Chromium process table, `dmesg`,
  the guest clock, an in-guest CDP connect, an MMDS read of the current
  restore-epoch, and an arping of the gateway.
- Host, for the same clone: the state file, the fcvm process tree, pasta's pid
  and process state, the holder's namespace via `nsenter` and via its procfs, a
  fresh TCP connect to the published port, and the restore and readiness lines
  from that clone's own fcvm log.

Read the failure dump against the control; a dump with nothing to compare
against proves little, which is why the control is captured whether or not the
run ends up failing, and why a control whose dump fails to WRITE does not count
as taken: the next healthy clone gets another go, bounded at three attempts so a
systematically broken probe cannot tax every healthy rep in the run. Read
`role`, `budget_exhausted`, `interrupted_by_signal` and each command's
`budget_limited` before reading anything else: a `timed_out` step that was
`budget_limited` is the probe running out of budget, not a wedged guest, and a
short dump carrying `interrupted_by_signal` is one that stood aside for a
shutdown.

**It stands aside for INT/TERM, and it is bounded by a process-group kill.** The
harness's signal handler only records the signal, so nothing in the probe is
interrupted asynchronously; without a check of its own a capture would spend its
whole budget with the clone still up and the teardown queued behind it, which is
long enough for a job runner to escalate to SIGKILL and leave behind exactly the
clone the probe came to explain. A pending signal therefore skips the capture,
and one arriving mid-capture stops it at the next step. Each probe command runs
in its own session, so the timeout kills the whole process GROUP rather than the
`fcvm exec` wrapper alone, and it names that group by the spawned pid rather
than by asking the leader for it: a wrapper that exits while its child holds the
pipe open makes `os.getpgid` raise ESRCH exactly when the kill is needed. The
kill verifies itself, and `group_kill.survivors` on the command's record is what
it found, with zombies excluded because a corpse keeps a group present without
being alive in it.

**What it cannot settle.** The passive capture is taken ~100 s after the request
gave up, so it describes the STEADY state of the failure, not the moment it
began; ordering questions still need the clone's log. It cannot see anything
Firecracker's MMDS server did or did not serve, only what the guest can now read
back. Its host-side namespace view is one `nsenter` snapshot, so it cannot show
a neighbour entry that expired earlier. And the active section mutates what the
passive section just recorded, deliberately and last.

## Claims currently REFUTED — status per `REVIEW.md`

This list and `REVIEW.md` disagreed for a while: everything here said "do not
re-publish" while the ledger had already marked the same claims SUPPORTED /
SUPERSEDED / REPLACED / RESOLVED on the corrected run. Whichever a reader hit
first decided whether the number could be published. `REVIEW.md` is the ledger;
this list now points at it rather than contradicting it.

- "fcvm beats a warm container pool on marginal memory (129 vs 151 MiB)" —
  **SUPPORTED but much smaller, and backend-dependent** (REVIEW.md row 1). Quote
  the corrected figures, never the original 129 vs 151.
- any egress-mode *ordering* — **STILL NOT SUPPORTED** (REVIEW.md row 2). The
  confound is gone but run-to-run variance dominates; do not publish an ordering.
- "16 clones sustain 5.5–6.3 req/s" — **SUPERSEDED** (REVIEW.md row 3). And do not
  quote a burst figure as throughput.
- "7.9 vs 5.3 req/GB" (slope without intercept) — **REPLACED** (REVIEW.md row 4).
- ~70 ms of the 310 ms file-vs-UFFD gap — **RESOLVED** (REVIEW.md row 5).
- "JPEG q80 −21% per request" — **REFUTED AS STATED** (REVIEW.md, SURVIVED table).
  The screenshot-STAGE win is real (−28.8%); the whole-request win is −8.3%. This
  list omitted the claim entirely while the Chromium-findings section above kept
  publishing the refuted number, which is precisely the disagreement the preamble
  above says this list exists to end.
- every figure from the `reqbench` CDP A/B — **WITHDRAWN IN FULL** (REVIEW.md,
  "The CDP-path A/B"). `exec 565 ms`, `cdp 384 ms`, `cdp-fast 372 ms`,
  `PART 1 −180.5 ms`, `reclaim CPU 0.00 ms`, the `+610.4 ms` machine cost and the
  `pasta 704 ms` straggler are all withdrawn; do not quote any of them.

`REVIEW.md` is the ledger of what holds and what doesn't. **Update it every run.**

## The fault harness refuses to report a number it cannot stand behind

`faultbench.py` measures per-request page faults and `faultanalyze.py` reduces them.
Both instruments attribute costs to a specific request, so each one has a rule about
when it must report NOTHING rather than something plausible. A wrong number here is
worse than a missing one, because it reads exactly like a real one and `agg` would
average it in. `make test-chromium-fault` guards all four.

- **An ambiguous UFFD trace attributes to neither request.** A trace is written when
  its handler exits, so it trails `t1` by the clone's teardown, and its filename
  carries the serve process's own connection counter, which nothing in the request
  record maps to. With two candidates inside one window there is no identity to break
  the tie, so `match_trace` returns None and stamps `trace_ambiguous`. Taking the
  newest would hand a request the NEXT one's faults and corrupt counts, locality and
  service time together. `agg` then reports a smaller `n`, which is the honest signal.
- **A clone that outlives its request stops the run.** Serial isolation is what makes
  attribution possible; a surviving clone keeps faulting and burning CPU while the next
  request is measured. `require_clones_gone` raises rather than letting the run
  continue, and the records already written stay valid.
- **An output directory is used once.** `requests.jsonl` is appended to and traces are
  matched by mtime, so reusing `--out` blends two runs, possibly taken with different
  arguments, into one analysis. `require_fresh_out_dir` refuses anything non-empty.
- **A parked CONTINUE is timed to its retry.** When `UFFDIO_CONTINUE` returns EAGAIN
  the vCPU stays blocked, so `src/uffd/server.rs` keeps the trace interval open across
  the park and closes it at the retry that actually resolved the fault. Closing it
  around the failed ioctl would report the EAGAIN as the resolution cost, and these
  intervals are read as exact ioctl service time.

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
