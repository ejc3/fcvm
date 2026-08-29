SHELL := /bin/bash

# Guard: never run make as root on the host. Running build plumbing
# as root leaves root-owned files in target/ that break subsequent user builds
# with BrokenPipe errors from nextest finding stale binaries.
# Skip this guard inside containers (where root is normal).
ifeq ($(shell id -u),0)
ifeq ($(wildcard /.dockerenv /run/.containerenv),)
$(error Do not run make as root. Use 'make test-root' as your normal user — it uses sudo only for the test runner)
endif
endif

# Find Rust toolchain bin directory and set PATH
# Prefer stable (has musl target), fall back to any toolchain
RUST_BIN := $(shell command -v cargo >/dev/null 2>&1 && dirname $$(command -v cargo) || \
	(test -x $(HOME)/.cargo/bin/cargo && echo $(HOME)/.cargo/bin) || \
	(ls -d $(HOME)/.rustup/toolchains/stable-*/bin 2>/dev/null | head -1) || \
	(ls -d $(HOME)/.rustup/toolchains/*/bin 2>/dev/null | head -1))
export PATH := $(RUST_BIN):$(PATH)
CARGO_BIN ?= cargo
# Every Cargo command issued by this Makefile holds a shared lease on this
# worktree's target directory.  runner-disk-preflight.sh takes the same lease
# exclusively before pruning idle artifacts, so age-check -> delete cannot race
# a build.  `override` prevents `make CARGO=cargo ...` from silently bypassing
# the safety protocol; CARGO_BIN remains available for toolchain selection.
override CARGO = "$(MAKEFILE_DIR)scripts/cargo-target-run.sh" $(CARGO_BIN)

# Custom dependencies bin directory
CUSTOM_DEPS_BIN := /mnt/fcvm-btrfs/deps/bin
ifneq ($(wildcard $(CUSTOM_DEPS_BIN)),)
export PATH := $(CUSTOM_DEPS_BIN):$(PATH)
endif

# Brief notes (see .claude/CLAUDE.md for details):
#   FILTER=x STREAM=1 - filter tests, stream output
#   Assets are content-addressed (kernel by URL SHA, rootfs by script SHA, initrd by binary SHA)
#   Logs: /tmp/fcvm-test-logs/
.PHONY: show-notes
show-notes:
	@echo "━━━ fcvm ━━━  FILTER=$(FILTER) STREAM=$(STREAM)  Assets=SHA-cached  (see .claude/CLAUDE.md)"

# Paths (can be overridden via environment)
FUSE_BACKEND_RS ?= /home/ubuntu/fuse-backend-rs
FUSER ?= /home/ubuntu/fuser

# Container settings
CONTAINER_TAG := fcvm-test:latest
CONTAINER_ARCH ?= $(shell uname -m)

# Per-mode data directories (prevents UID conflicts between test modes)
ROOT_DATA_DIR := /mnt/fcvm-btrfs/root
CONTAINER_DATA_DIR := /mnt/fcvm-btrfs/container

# Test options: FILTER=pattern STREAM=1 LIST=1 IGNORED=1
FILTER ?=
ifeq ($(IGNORED),1)
NEXTEST_IGNORED := --run-ignored=all
else
NEXTEST_IGNORED :=
endif

# On IPv6-only hosts, auto-exclude bridged tests (they require IPv4 iptables)
# User can still explicitly run bridged tests with FILTER=bridged
ifeq ($(IPV6_ONLY),1)
ifndef FILTER
# No filter set: exclude bridged tests
NEXTEST_PARTITION := --partition hash:1/2
IPV6_FILTER := -E 'not test(/bridged/)'
else
# User set a filter: respect it (they know what they're doing)
IPV6_FILTER :=
endif
else
IPV6_FILTER :=
endif

# CI runs a representative SUBSET of the active nested (NV2) tests, not all of them.
# Nested tests are the arm64 long pole and do NOT parallelize — they degrade ~3.7x under
# mutual contention (system-wide dsb(sy) on every nested guest-exit + NV2 overhead; see the
# [test-groups.nested-tests] comment in .config/nextest.toml), so concurrency can't shrink
# them. Instead CI runs three that cover the distinct nested risks and skips the rest
# (which still run locally with a normal `make test-root`):
#   - test_nested_run_fcvm_inside_vm  : nesting works at all + /dev/kvm usable in the guest
#   - test_nested_l2_with_large_files : FUSE-over-FUSE cache coherency (the #630 DSB regression guard)
#   - test_nested_l2_nfs              : the NFS-share data path under nesting
# Skipped in CI (still run locally): test_nested_l2_fuse (FUSE path is covered by large_files),
# test_nested_l2_network_fuse / _network_nfs (iperf throughput — the slowest AND flakiest under
# any contention), and test_utimensat_pjdfstest_nested_kernel. The L3/L4, benchmark, and
# podman-load nested tests are already #[ignore]'d (run manually, tracked in #630-B).
# Gated on CI=true (set by GitHub Actions). Override anytime with an explicit FILTER=...
# Scoped to package(fcvm) so fuse-pipe's fast test_nested_file unit test is NOT excluded.
# Mutually exclusive with IPV6_FILTER in practice — CI runners have an IPv4 route, so
# IPV6_ONLY is never set there (two -E filtersets would be UNIONED, not intersected).
CI ?=
ifndef FILTER
ifeq ($(CI),true)
# Base test-root CI filter: the nested subset (exclude all nested except the 3 kept).
CI_ROOT_BASE := (not (package(fcvm) & (test(/nested/) | test(/podman_load_over_fuse/))) | test(=test_nested_run_fcvm_inside_vm) | test(=test_nested_l2_with_large_files) | test(=test_nested_l2_nfs))
ifeq ($(FCVM_NO_SNAPSHOT),)
# SnapshotEnabled mode (FCVM_NO_SNAPSHOT unset): ALSO drop test_nested_l2_with_large_files.
# It guards FUSE-over-FUSE DATA INTEGRITY (the #630 DSB cache-coherency fix) — a live-traffic
# property of the always-on kernel patch, independent of the snapshot feature — and it already
# runs in the SnapshotDisabled job (~225s). Under SnapshotEnabled it runs TWICE (cold + restore)
# and the warm run is a 4GB *nested* VM UFFD restore whose page faults go through double Stage-2
# translation (~842s), ~18 min total for zero extra integrity coverage. Snapshot/restore
# correctness is covered by test_startup_snapshot_* and the CH/clone roundtrips. This is the
# single biggest lever on the arm64 SnapshotEnabled long pole (the file copy itself is only ~20s).
CI_NESTED_FILTER := -E '$(CI_ROOT_BASE) & not test(=test_nested_l2_with_large_files)'
else
# SnapshotDisabled mode: keep test_nested_l2_with_large_files (the FUSE-integrity guard runs here).
CI_NESTED_FILTER := -E '$(CI_ROOT_BASE)'
endif
endif
endif

# Default log level: fcvm debug, suppress FUSE spam
# Override with: RUST_LOG=debug make test-root
TEST_LOG ?= fcvm=debug,health-monitor=info,fuser=warn,fuse_backend_rs=warn,passthrough=warn
ifeq ($(STREAM),1)
NEXTEST_CAPTURE := --no-capture
endif
ifeq ($(LIST),1)
NEXTEST_CMD := list
else
NEXTEST_CMD := run
endif

# Architecture detection
ARCH := $(shell uname -m)
ifeq ($(ARCH),aarch64)
MUSL_TARGET := aarch64-unknown-linux-musl
else
MUSL_TARGET := x86_64-unknown-linux-musl
endif

# IPv6-only detection: If no IPv4 default route exists, bridged networking won't work
# (bridged uses IPv4 iptables DNAT). Auto-exclude bridged tests on IPv6-only hosts.
HAS_IPV4 := $(shell ip route show default 2>/dev/null | grep -q . && echo 1 || echo 0)
ifeq ($(HAS_IPV4),0)
IPV6_ONLY := 1
$(info Note: IPv6-only host detected - bridged tests will be skipped)
endif


# Build artifacts go in a per-worktree directory on btrfs; scripts/cargo-target-link.sh
# owns the derivation and the symlink, and is the ONLY place that computes the path.
# Overridable so tests/test_cargo_target_link.rs can point it at a temp dir.
BTRFS_ROOT ?= /mnt/fcvm-btrfs
export BTRFS_ROOT
MAKEFILE_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Base test command
# `target` is a symlink into BTRFS_ROOT, created by the cargo-target-link target
# (a prerequisite of every build/test target).
export CARGO_TARGET_DIR := target
NEXTEST := $(CARGO) nextest $(NEXTEST_CMD) --release
TEST_CONFIG_WRAPPER := ./scripts/with-test-config.sh
# Extra flags forwarded to every criterion bench recipe (see bench-quick).
#
# Criterion's flags belong to the BENCH BINARY, so they have to follow `--`.
# Given to cargo directly they are rejected by its own argument parser before
# any benchmark starts:
#
#     $ cargo bench -p fuse-pipe --bench throughput --sample-size 10
#     error: unexpected argument '--sample-size' found
#       tip: to pass '--sample-size' as a value, use '-- --sample-size'
#     Usage: cargo bench --package [<SPEC>] --bench [<NAME>] [-- [ARGS]...]
#
# BENCH_SEPARATED is what the recipes interpolate, and it stays empty when
# BENCH_ARGS is, so plain `make bench` carries no trailing separator.
BENCH_ARGS ?=
BENCH_SEPARATED = $(if $(strip $(BENCH_ARGS)),-- $(BENCH_ARGS))

# Where criterion keeps baselines and reports. Pinned to an absolute path
# because criterion's default is not the directory it looks like.
#
# Its second choice after CRITERION_HOME is `$CARGO_TARGET_DIR/criterion`, and
# CARGO_TARGET_DIR here is the relative string `target`, which criterion
# resolves inside the bench binary, whose working directory cargo sets to the
# package root. Every suite was therefore writing to `fuse-pipe/target/criterion`
# while the ownership repair scanned `target/` at the repo root and found
# nothing to do. Overridable so tests can point it at a scratch directory.
CRITERION_HOME ?= $(CURDIR)/target/criterion

# cargo/nextest target runner for the privileged suites. cargo-nextest stays unprivileged
# and only the test binary is elevated, so the `sudo` hop sits in the middle of the process
# tree — and a privilege boundary breaks parent->child teardown in BOTH directions: nextest
# (uid 1000) cannot signal the uid-0 test binary at all, and `sudo` cannot forward the
# SIGKILL that killed it. The elevated test binary is then orphaned-but-alive and its whole
# microVM subtree with it. root-test-runner.sh restores the link with a kernel-enforced
# PR_SET_PDEATHSIG; see that script's header for the measurements.
ROOT_TEST_RUNNER := sudo -E env PATH=$(PATH) $(CURDIR)/scripts/root-test-runner.sh

# Optional cargo cache directory (for CI caching)
CARGO_CACHE_DIR ?=
ifneq ($(CARGO_CACHE_DIR),)
# CI mode: use cache directory for both registry and target
CARGO_CACHE_MOUNT := -v $(CARGO_CACHE_DIR)/registry:/usr/local/cargo/registry
TARGET_MOUNT := -v $(CARGO_CACHE_DIR)/target:/workspace/fcvm/target
else
# Local mode: use temp directory for target (avoids permission conflicts)
CARGO_CACHE_MOUNT :=
TARGET_MOUNT := -v /tmp/fcvm-container-target:/workspace/fcvm/target
endif

# Test log directory (mounted into container)
TEST_LOG_DIR := /tmp/fcvm-test-logs

# Container run command (base)
# Note: Use -v instead of --device for /dev/kvm to preserve group permissions in rootless mode
# See: https://github.com/containers/podman/issues/16701
CONTAINER_RUN_BASE := podman run --rm --privileged \
	--security-opt label=disable --group-add keep-groups \
	-v .:/workspace/fcvm \
	$(TARGET_MOUNT) \
	-v $(FUSE_BACKEND_RS):/workspace/fuse-backend-rs -v $(FUSER):/workspace/fuser \
	--device /dev/fuse -v /dev/kvm:/dev/kvm -v /dev/userfaultfd:/dev/userfaultfd \
	--ulimit nofile=65536:65536 --ulimit nproc=-1:-1 \
	-v /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
	-v $(TEST_LOG_DIR):$(TEST_LOG_DIR) $(CARGO_CACHE_MOUNT) \
	-e FCVM_DATA_DIR=$(CONTAINER_DATA_DIR)

# Container run with high process limits (for parallel test suites)
CONTAINER_RUN := $(CONTAINER_RUN_BASE) --ulimit nproc=65536:65536 --pids-limit=65536

.PHONY: all help build clean clean-test-data check-disk \
	test test-unit test-agent-unit test-fast test-all test-root test-packaging test-ci-infrastructure test-clone-floor-overlap fuzz \
	_test-unit _test-agent-unit _test-fast _test-all _test-root _setup-fcvm _bench \
	container-build container-test container-test-unit container-test-fast container-test-all container-test-fc-mock \
	container-setup-fcvm container-shell container-clean container-bench \
	cargo-target-link build-host-tools setup-btrfs setup-default release-default-kernel setup-fcvm setup-pjdfstest setup-hugepages bench bench-vm bench-hugepages bench-hugepages-test \
	bench-container-import bench-chromium analyze-chromium-request bench-clone-latency test-chromium-request \
	bench-chromium-request-build bench-webkit-request-build bench-webkit-request-golden bench-webkit-request-verify bench-webkit-request-run test-chromium bench-chromium-request-golden bench-chromium-request-verify \
	bench-chromium-corpus bench-stop \
	bench-chromium-request-run bench-chromium-request-all bench-chromium-hostcdp bench-chromium-fault \
	bench-chromium-request-diag bench-webkit-request-diag \
	bench-chromium-scale analyze-chromium-scale report-chromium-scale test-chromium-scale \
	test-chromium-fault \
	bench-quick bench-throughput bench-operations bench-protocol \
	lint fmt update-dependency ssh test-serve-sdk

all: build

help:
	@echo "fcvm Makefile"
	@echo ""
	@echo "Build:"
	@echo "  build              Build fcvm + fc-agent"
	@echo "  build-fc-mock      Build fc-mock (Firecracker mock for container mode)"
	@echo "  update-dependency  Update one Cargo.lock package (PACKAGE=..., optional VERSION=...)"
	@echo "  clean              Remove target directory"
	@echo ""
	@echo "Test (host):"
	@echo "  test-unit          Unit tests only (no VMs, no sudo)"
	@echo "  test-agent-unit    fc-agent unit tests only (no VMs, no sudo)"
	@echo "  test-fast          + quick VM tests (rootless, no sudo)"
	@echo "  test-all           + slow VM tests (rootless, no sudo)"
	@echo "  test-root, test    + privileged tests (bridged, pjdfstest, sudo)"
	@echo "  test-fc-mock       Run tests with fc-mock (no KVM required)"
	@echo "  test-clone-floor-overlap  Reproduce the clone/CH/hugepage lifecycle overlap"
	@echo "  test-ci-infrastructure  Deterministic runner-failure classifier fixtures"
	@echo "  fuzz               Seeded lifecycle chaos fuzz (SEEDS=N|list OPS=M, defaults 1/10)"
	@echo ""
	@echo "Test (container):"
	@echo "  container-test-unit    Unit tests in container"
	@echo "  container-test-fast    + quick VM tests in container"
	@echo "  container-test-all, container-test  + slow VM tests in container"
	@echo "  container-test-fc-mock  fc-mock tests in container (no KVM)"
	@echo ""
	@echo "Container:"
	@echo "  container-build    Build test container"
	@echo "  container-shell    Interactive shell in container"
	@echo "  container-bench    Run benchmarks in container"
	@echo ""
	@echo "Setup:"
	@echo "  setup-btrfs        Create btrfs loopback at /mnt/fcvm-btrfs"
	@echo "  setup-fcvm         Download kernel and create rootfs"
	@echo "  setup-pjdfstest    Build pjdfstest"
	@echo "  setup-lint-tools   Install cargo-audit and cargo-deny"
	@echo "  install-host-kernel  Build and install host kernel with patches (requires reboot)"
	@echo ""
	@echo "Kernel patches:"
	@echo "  kernel-patch-create PROFILE=nested NAME=0004-fix FILE=fs/fuse/dir.c"
	@echo "  kernel-patch-edit PROFILE=nested PATCH=0002"
	@echo "  kernel-patch-validate PROFILE=nested"
	@echo ""
	@echo "SDK:"
	@echo "  test-serve-sdk     Run ComputeSDK E2E test (requires computesdk sibling repo)"
	@echo ""
	@echo "Benchmarks:"
	@echo "  bench              Run fuse-pipe benchmarks"
	@echo "  bench-vm           Run VM benchmarks (exec, clone)"
	@echo "  bench-hugepages    Run hugepages benchmark (32GB VM, 16GB dirty)"
	@echo "  bench-hugepages-test  Run hugepages benchmark (2GB VM, 256MB dirty)"
	@echo "  bench-container-import  Compare podman load vs direct image mount"
	@echo "  bench-chromium     Chromium shared-nothing clone bench (egress x memory matrix)"
	@echo "  bench-chromium-request-build   Build the request-bench container image"
	@echo "  bench-webkit-request-build     Build the WebKit request-bench container image"
	@echo "  bench-chromium-request-golden  Create golden snapshot (TAG=, HUGEPAGES=1, NETMODE=, GUEST_ENV=)"
	@echo "  bench-chromium-request-verify  Prove CDP hops on a restored clone (TAG=)"
	@echo "  bench-chromium-request-run     Measured run (TAG=, BACKEND=, UFFD_MODE=, UFFD_PREFETCH=, REPS=, WARMUP=, ARMS=, RESULTS=)"
	@echo "  bench-chromium-request-all     Full chain: image, golden, verify, run"
	@echo "  bench-chromium-request-diag    In-guest load diagnostics, one traced render per clone, serve always --uffd-prefetch off (TAG=, BACKEND=, UFFD_MODE=, DIAG_URLS=, DIAG_REPS=, DIAG_EXPECT_IPS=, DIAG_MAX_LOAD_MS=, RESULTS=)"
	@echo "  bench-webkit-request-diag      WebKit twin of bench-chromium-request-diag (TAG=, BACKEND=, DIAG_URLS=, DIAG_REPS=, DIAG_EXPECT_IPS=, DIAG_MAX_LOAD_MS=, RESULTS=)"
	@echo "  bench-chromium-corpus         Corpus campaign, orchestrator frozen per run (TAG=, CPU=, PHASE=)"
	@echo "  bench-stop                    Stop all bench processes, reap stray VMs, restore dnsmasq"
	@echo "  bench-chromium-hostcdp         Host-container direct-CDP baseline (no VM; BENCH_RESOLVE_ALL_TO=)"
	@echo "  bench-chromium-fault           Page-fault bench (FAULT_OUT= required; needs bench.sh goldens)"
	@echo "  analyze-chromium-request  Re-run publication gates for RESULTS=/path/to/run"
	@echo "  test-chromium          Run ALL bench unit tests (what CI runs)"
	@echo "  test-chromium-request  Run the request benchmark's deterministic unit tests"
	@echo "  bench-chromium-scale  Open-loop FILE/UFFD Chromium request scalability run"
	@echo "  analyze-chromium-scale  Validate a scale run and write deterministic JSON"
	@echo "  report-chromium-scale   Validate a scale run and write plain Markdown"
	@echo "  test-chromium-scale  Run the scale harness's deterministic unit tests"
	@echo "  test-chromium-fault  Run the fault harness's deterministic unit tests"
	@echo "  bench-clone-latency  Clone spawn->exec-ready latency (LABEL=, N=)"
	@echo ""
	@echo "CI merge train (pooled CI for a batch of PRs, see docs/ci-train.md):"
	@echo "  train-create PRS=\"689 690\"  Build the ci-train branch from a batch of PRs"
	@echo "  train-dispatch     Push the train and dispatch ONE full CI matrix for the batch"
	@echo "  train-status       Show the train run's state"
	@echo "  train-land         After a green run, merge every batch PR in order"
	@echo "  train-bisect       Split a red train into two half-trains (recurse with TRAIN=ci-train-a)"
	@echo ""
	@echo "Other:"
	@echo "  lint               Run linting (auto-installs tools if needed)"
	@echo "  fmt                Format code"
	@echo "  clean-test-data    Remove VM disks, snapshots, state (keeps cached assets)"
	@echo "  check-disk         Check disk space requirements"
	@echo ""
	@echo "Options: FILTER=pattern STREAM=1 LIST=1"
# Point this worktree's target/ at its own directory on btrfs.
#
# Every entry point that invokes cargo must depend on this. `make build` had no
# such prerequisite, so a fresh worktree created a REAL target/ on the root
# filesystem — which is nearly full — silently defeating the whole arrangement.
.PHONY: cargo-target-link
cargo-target-link:
	@BTRFS_ROOT="$(BTRFS_ROOT)" "$(MAKEFILE_DIR)scripts/cargo-target-link.sh"

# Disk space check - fails if either root or btrfs is too full
# Requires 10GB free on root (for cargo target) and 15GB on btrfs (for VMs)
check-disk: cargo-target-link
	@# Ensure test log directory exists for container mounts
	@mkdir -p $(TEST_LOG_DIR)
	@# Fix advisory-db ownership (sudo/non-sudo mixing corrupts it)
	@sudo chown -R $$(id -u):$$(id -g) "$$HOME/.cargo/advisory-db" 2>/dev/null || true
	@sudo chown -R $$(id -u):$$(id -g) "$$HOME/.cargo/advisory-dbs" 2>/dev/null || true
	@# Symlink ~/.cargo and target/ to btrfs so cargo builds don't fill root filesystem
	@if [ -d /mnt/fcvm-btrfs ] && ! [ -L "$$HOME/.cargo" ]; then \
		if [ -d "$$HOME/.cargo" ]; then \
			echo "==> Moving existing ~/.cargo to /mnt/fcvm-btrfs/cargo..."; \
			sudo rm -rf /mnt/fcvm-btrfs/cargo; \
			mv "$$HOME/.cargo" /mnt/fcvm-btrfs/cargo; \
		elif [ ! -e "$$HOME/.cargo" ]; then \
			mkdir -p /mnt/fcvm-btrfs/cargo; \
		fi; \
		ln -sf /mnt/fcvm-btrfs/cargo "$$HOME/.cargo"; \
		echo "==> Symlinked ~/.cargo → /mnt/fcvm-btrfs/cargo"; \
	fi
	@# Self-heal a wiped cargo home. The btrfs above is ephemeral and can be reset
	@# out from under us, leaving ~/.cargo a dangling symlink. cargo can't restore
	@# itself, so recreate the symlink's TARGET dir (mkdir through a dangling symlink
	@# fails, so operate on the readlink'd path) and relink the rustup toolchain
	@# shims; the registry re-downloads on the next build.
	@if [ -L "$$HOME/.cargo" ] && ! [ -e "$$HOME/.cargo/bin/cargo" ]; then \
		echo "==> cargo home wiped; restoring toolchain..."; \
		TGT=$$(readlink "$$HOME/.cargo"); \
		mkdir -p "$$TGT/bin"; \
		TCBIN=$$(ls -d "$$HOME"/.rustup/toolchains/*/bin 2>/dev/null | head -1); \
		if [ -n "$$TCBIN" ]; then \
			for b in cargo rustc cargo-clippy clippy-driver rustfmt cargo-fmt rustdoc; do \
				[ -e "$$TCBIN/$$b" ] && ln -sf "$$TCBIN/$$b" "$$TGT/bin/$$b"; \
			done; \
			echo "==> Restored cargo shims from $$TCBIN into $$TGT/bin"; \
		else echo "==> WARNING: no rustup toolchain found to restore cargo from"; fi; \
	fi
	@# Ensure cargo-nextest (the test runner; a separate install from rustup).
	@if ! PATH="$$HOME/.cargo/bin:$$PATH" $(CARGO) nextest --version >/dev/null 2>&1; then \
		echo "==> Installing cargo-nextest..."; \
		case "$$(uname -m)" in aarch64|arm64) NURL=https://get.nexte.st/latest/linux-arm ;; *) NURL=https://get.nexte.st/latest/linux ;; esac; \
		mkdir -p "$$HOME/.cargo/bin"; \
		curl -LsSf "$$NURL" | tar zxf - -C "$$HOME/.cargo/bin" \
			|| PATH="$$HOME/.cargo/bin:$$PATH" $(CARGO) install cargo-nextest --locked; \
	fi
	@BTRFS_FREE=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$BTRFS_FREE" ] && [ "$$BTRFS_FREE" -lt 15 ]; then \
		echo "ERROR: Need 15GB free on /mnt/fcvm-btrfs (have $${BTRFS_FREE}GB)"; \
		echo "Try: make clean-test-data"; \
		exit 1; \
	fi; \
	echo "Disk check passed: /mnt/fcvm-btrfs has $${BTRFS_FREE}GB free"

# Clean leftover test data (VM disks, snapshots, state files)
# Preserves cached assets (kernels, rootfs, initrd, image-cache)
# CRITICAL: Uses fcvm's proper cleanup commands to handle btrfs CoW correctly
clean-test-data: build
	@echo "==> Killing stale VM processes from previous runs..."
	@sudo pkill -9 firecracker 2>/dev/null; sudo pkill -9 pasta 2>/dev/null; sudo kill -9 $$(pgrep -x sleep -P 1) 2>/dev/null; sleep 1; true
	@echo "==> Cleaning stale network namespaces..."
	@for ns in $$(sudo ip netns list 2>/dev/null | grep -E '^(fcvm-|test-lf-|test-pf-|test-proxy-)' | awk '{print $$1}'); do sudo ip netns del "$$ns" 2>/dev/null && echo "  deleted $$ns"; done; true
	@echo "==> Cleaning stale iptables rules from fcvm VMs..."
	@sudo iptables-save -t nat 2>/dev/null | grep -E 'MASQUERADE.*(172\.30\.|10\.0\.)' | sed 's/^-A//' | while read rule; do sudo iptables -t nat -D $$rule 2>/dev/null; done; true
	@sudo ip6tables-save -t nat 2>/dev/null | grep 'MASQUERADE' | grep -v NETAVARK | sed 's/^-A//' | while read rule; do sudo ip6tables -t nat -D $$rule 2>/dev/null; done; true
	@echo "==> Force unmounting stale FUSE mounts..."
	@# Find and force unmount any FUSE mounts from previous test runs
	@mount | grep fuse | grep -E '/tmp|/var/tmp' | cut -d' ' -f3 | xargs -r -I{} fusermount3 -u -z {} 2>/dev/null || true
	@echo "==> Cleaning snapshots via fcvm (handles btrfs CoW properly)..."
	@# Use fcvm's snapshot prune for proper cleanup - handles reflinks correctly
	sudo ./target/release/fcvm snapshots prune --all --force 2>/dev/null || true
	@# Also clean per-mode directories
	sudo FCVM_DATA_DIR=$(ROOT_DATA_DIR) ./target/release/fcvm snapshots prune --all --force 2>/dev/null || true
	sudo FCVM_DATA_DIR=$(CONTAINER_DATA_DIR) ./target/release/fcvm snapshots prune --all --force 2>/dev/null || true
	@# Fallback: remove any snapshots that prune couldn't parse (stale/incompatible configs)
	sudo rm -rf /mnt/fcvm-btrfs/snapshots/*
	sudo rm -rf $(ROOT_DATA_DIR)/snapshots/* $(CONTAINER_DATA_DIR)/snapshots/*
	@echo "==> Cleaning leftover VM disks..."
	sudo rm -rf /mnt/fcvm-btrfs/vm-disks/*
	sudo rm -rf $(ROOT_DATA_DIR)/vm-disks/* $(CONTAINER_DATA_DIR)/vm-disks/*
	@echo "==> Cleaning state files..."
	sudo rm -rf /mnt/fcvm-btrfs/state/*.json /mnt/fcvm-btrfs/state/*.lock
	sudo rm -rf $(ROOT_DATA_DIR)/state/*.json $(ROOT_DATA_DIR)/state/*.lock
	sudo rm -rf $(CONTAINER_DATA_DIR)/state/*.json $(CONTAINER_DATA_DIR)/state/*.lock
	@echo "==> Cleaning UFFD sockets..."
	sudo rm -f /mnt/fcvm-btrfs/uffd-*.sock
	sudo rm -f $(ROOT_DATA_DIR)/uffd-*.sock $(CONTAINER_DATA_DIR)/uffd-*.sock
	@echo "==> Cleaning test logs..."
	sudo rm -rf /tmp/fcvm-test-logs
	mkdir -p /tmp/fcvm-test-logs
	@echo "==> Cleaned test data (preserved cached assets)"

# Record which FUSE dependency code this build compiles against. Two
# dependencies, two mechanisms:
#   fuse-backend-rs is a sibling path dependency (fuse-pipe/Cargo.toml points
#   at ../../fuse-backend-rs), so the compiled code is whatever that directory
#   holds, not what any lockfile pins. Reported as git describe of the
#   checkout, or MISSING when there is no checkout. When the tree is dirty
#   the line appends +<first 12 hex of sha256 over `git diff HEAD`>, because
#   every possible local edit against one commit otherwise prints the same
#   -dirty value. Untracked files are excluded from the digest: describe
#   --dirty does not flag them, and an untracked file cannot reach the build
#   unless a tracked file references it, which dirties the tree.
#   fuser is a git dependency (fuse-pipe/Cargo.toml declares the URL,
#   Cargo.lock pins the revision). Cargo compiles the locked revision from
#   its git cache, never the sibling /workspace/fuser mount, so the line
#   reports the lock's resolved source, which carries the exact commit
#   after '#'.
# Issue #807: two local fuse-backend-rs checkouts drifted 19 commits apart
# and no build log recorded which one a given binary used. Pinned by
# tests/test_dep_provenance.rs.
.PHONY: dep-provenance
dep-provenance:
	@dir="$(MAKEFILE_DIR)../fuse-backend-rs"; \
	if desc=$$(git -C "$$dir" describe --always --dirty 2>/dev/null); then \
		case "$$desc" in \
			*-dirty) desc="$$desc+$$(git -C "$$dir" diff --no-ext-diff HEAD | sha256sum | cut -c1-12)" ;; \
		esac; \
	else \
		desc=MISSING; \
	fi; \
	echo "fuse-backend-rs: $$desc"
	@src=$$(awk -F'"' '/^name = "fuser"$$/ {f=1; next} f && /^\[\[package\]\]/ {exit} f && /^source = / {print $$2; exit}' "$(MAKEFILE_DIR)Cargo.lock" 2>/dev/null); \
	echo "fuser: $${src:-MISSING}"

# The provenance the dep-provenance prerequisite prints only describes the
# sources cargo read if nothing changed the sibling tree mid-build. Both
# build targets therefore snapshot the provenance before their cargo
# commands, re-derive it after, and fail on any difference instead of
# logging a line about a tree cargo never saw.
build: cargo-target-link dep-provenance
	@echo "==> Building..."
	@set -e; \
	before="$$($(MAKE) --no-print-directory dep-provenance)"; \
	CARGO_TARGET_DIR=target $(CARGO) build --release -p fcvm; \
	CARGO_TARGET_DIR=target $(CARGO) build --release -p fc-agent --target $(MUSL_TARGET); \
	mkdir -p target/release; \
	cp target/$(MUSL_TARGET)/release/fc-agent target/release/fc-agent; \
	after="$$($(MAKE) --no-print-directory dep-provenance)"; \
	if [ "$$before" != "$$after" ]; then \
		printf 'ERROR: dependency provenance changed during the build\nbefore:\n%s\nafter:\n%s\n' "$$before" "$$after" >&2; \
		exit 1; \
	fi
	@# Sync embedded config to user config dir (config is embedded at compile time)
	@./target/release/fcvm setup --generate-config --force 2>/dev/null || true

# Host-native tools used by the kernel-builder AMI/workflow. Unlike `build`,
# this does not require a musl Rust target for the guest agent.
build-host-tools: cargo-target-link dep-provenance
	@echo "==> Building host-native fcvm and fc-agent..."
	@set -e; \
	before="$$($(MAKE) --no-print-directory dep-provenance)"; \
	CARGO_TARGET_DIR=target $(CARGO) build --release -p fcvm -p fc-agent; \
	after="$$($(MAKE) --no-print-directory dep-provenance)"; \
	if [ "$$before" != "$$after" ]; then \
		printf 'ERROR: dependency provenance changed during the build\nbefore:\n%s\nafter:\n%s\n' "$$before" "$$after" >&2; \
		exit 1; \
	fi

build-fc-mock: cargo-target-link
	@echo "==> Building fc-mock..."
	CARGO_TARGET_DIR=target $(CARGO) build --release -p fc-mock
	@# Install to /usr/local/bin so it's accessible from within user namespaces
	@# (rootless networking uses nsenter which can't traverse /home/ubuntu/ with 750 perms)
	sudo install -m 755 target/release/fc-mock /usr/local/bin/fc-mock

# Test that the release binary works without source tree (simulates cargo install)
test-packaging: build
	@echo "==> Testing packaging (simulates cargo install)..."
	./scripts/test-packaging.sh target/release/fcvm

clean:
	@# Publish an empty namespace; the disk guard safely reclaims the retired
	@# generation without unlinking dentries that may be foreign mountpoints.
	@BTRFS_ROOT="$(BTRFS_ROOT)" "$(MAKEFILE_DIR)scripts/cargo-target-link.sh" --rotate

# Run-only targets (no setup deps, used by container)
_test-unit: cargo-target-link
	$(TEST_CONFIG_WRAPPER) $(NEXTEST) --no-default-features $(FILTER)

_test-agent-unit: cargo-target-link
	$(TEST_CONFIG_WRAPPER) $(NEXTEST) -p fc-agent $(FILTER)

_test-fast: cargo-target-link
	RUST_LOG="$(TEST_LOG)" \
	$(TEST_CONFIG_WRAPPER) ./scripts/no-sudo.sh $(NEXTEST) $(NEXTEST_CAPTURE) --no-default-features --features integration-fast $(FILTER)

_test-all: cargo-target-link
	RUST_LOG="$(TEST_LOG)" \
	$(TEST_CONFIG_WRAPPER) ./scripts/no-sudo.sh $(NEXTEST) $(NEXTEST_CAPTURE) $(FILTER)

_test-root: cargo-target-link
	@if find target/ -user root -print -quit 2>/dev/null | grep -q .; then \
		echo "==> WARNING: root-owned files in target/ (from sudo cargo?). Fixing ownership..."; \
		sudo chown -R $$(id -u):$$(id -g) target/; \
	fi
	@RUST_LOG="$(TEST_LOG)" \
	FCVM_DATA_DIR=$(ROOT_DATA_DIR) \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(TEST_CONFIG_WRAPPER) $(NEXTEST) $(NEXTEST_CAPTURE) $(NEXTEST_IGNORED) --features privileged-tests $(IPV6_FILTER) $(CI_NESTED_FILTER) $(FILTER) || \
	{ echo ""; \
	  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"; \
	  echo "TEST FAILED - Check debug logs for root cause:"; \
	  echo "  📋 Debug logs: /tmp/fcvm-test-logs/*.log"; \
	  echo "  💡 Re-run with STREAM=1 to see tracing output in real-time"; \
	  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"; \
	  exit 1; }

# Host targets (with setup, check-disk first to fail fast if disk is full)
test-unit: show-notes check-disk build _test-unit
test-agent-unit: show-notes check-disk cargo-target-link _test-agent-unit
test-fast: show-notes check-disk setup-fcvm _test-fast
test-all: show-notes check-disk setup-fcvm _test-all
test-root: show-notes check-disk setup-fcvm setup-pjdfstest setup-hugepages _test-root
test: test-root

# Seeded lifecycle chaos fuzz (tests/test_fuzz_chaos.rs): one rootless VM per
# seed, randomized-but-seeded op schedule, end-state oracles. Runs through the
# same _test-root machinery as the rest of the suite.
# Usage: make fuzz SEEDS=25 OPS=30   (defaults: SEEDS=1 OPS=10)
# SEEDS is a count N (runs seeds 1..=N) or a comma list of literal seeds
# ("3,17,42"; a trailing comma like "7," replays exactly seed 7).
SEEDS ?= 1
OPS ?= 10
fuzz: show-notes check-disk setup-fcvm
	FCVM_FUZZ_SEEDS=$(SEEDS) FCVM_FUZZ_OPS=$(OPS) $(MAKE) _test-root FILTER=fuzz_lifecycle_chaos

# fc-mock: container-mode tests (no KVM required)
# Uses fc-mock binary instead of Firecracker.
# Only runs tests known to work with fc-mock (no KVM, no real Firecracker).
FC_MOCK_FILTER := package(fcvm) & (test(=test_sanity_bridged) | test(=test_sanity_rootless) | test(/fc_mock/) | test(/state_manager/) | test(/health_monitor/) | test(/no_sudo/))
_test-fc-mock: cargo-target-link
	@FCVM_FIRECRACKER_BIN=/usr/local/bin/fc-mock \
	RUST_LOG="$(TEST_LOG)" \
	FCVM_DATA_DIR=$${FCVM_DATA_DIR:-$(ROOT_DATA_DIR)} \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(TEST_CONFIG_WRAPPER) $(NEXTEST) $(NEXTEST_CAPTURE) --profile fc-mock --features privileged-tests -E '$(FC_MOCK_FILTER)' $(FILTER) || \
	{ echo ""; \
	  echo "TEST FAILED (fc-mock mode)"; \
	  exit 1; }
test-fc-mock: show-notes check-disk build build-fc-mock setup-fcvm _test-fc-mock

# Container targets (setup on host where needed, run-only in container)
# Container uses shadowed target/ mount to avoid permission conflicts
# check-disk runs on host before container tests start

# fc-mock in container (subset — networking tests excluded)
# Rootless podman containers can't create nested user namespaces or TAP devices,
# so only fc-mock unit tests run here. Full fc-mock tests run on bare metal (test-fc-mock).
FC_MOCK_CONTAINER_FILTER := package(fcvm) & (test(/fc_mock/) | test(/state_manager/) | test(/health_monitor/) | test(/no_sudo/)) & not test(=test_fc_mock_sanity) & not test(=test_fc_mock_container_launch)
container-test-fc-mock: check-disk container-build setup-btrfs
	@echo "==> Running fc-mock tests in container (unit tests only)..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) bash -c '\
		make build build-fc-mock && \
		FCVM_FIRECRACKER_BIN=/usr/local/bin/fc-mock \
		RUST_LOG="$(TEST_LOG)" \
		$(NEXTEST) $(NEXTEST_CAPTURE) --profile fc-mock --features privileged-tests -E "$(FC_MOCK_CONTAINER_FILTER)" $(FILTER) || \
		{ echo "TEST FAILED (fc-mock container mode)"; exit 1; }'

container-test-unit: check-disk container-build
	@echo "==> Running unit tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make build _test-unit

container-test-fast: check-disk container-setup-fcvm
	@echo "==> Running fast tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make _test-fast

container-test-all: check-disk container-setup-fcvm
	@echo "==> Running all tests in container..."
	$(CONTAINER_RUN) $(CONTAINER_TAG) make _test-all

container-test: container-test-all

CONTAINER_CACHE_REPO ?= ghcr.io/ejc3/fcvm-cache
# Only push cache if authenticated to ghcr.io (CI has login, local dev doesn't)
CACHE_TO_FLAG := $(shell podman login --get-login ghcr.io >/dev/null 2>&1 && echo "--cache-to $(CONTAINER_CACHE_REPO)" || echo "")

container-build:
	@sudo mkdir -p /mnt/fcvm-btrfs 2>/dev/null || true
	@mkdir -p /tmp/fcvm-container-target
	podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) \
		--layers --cache-from $(CONTAINER_CACHE_REPO) $(CACHE_TO_FLAG) . \
	|| podman build -t $(CONTAINER_TAG) -f Containerfile --build-arg ARCH=$(CONTAINER_ARCH) \
		--layers --cache-from $(CONTAINER_CACHE_REPO) .

container-shell: container-build
	$(CONTAINER_RUN) -it $(CONTAINER_TAG) bash

container-clean:
	podman rmi $(CONTAINER_TAG) 2>/dev/null || true

# Setup targets
setup-pjdfstest:
	@if [ ! -x /tmp/pjdfstest-check/pjdfstest ]; then \
		echo '==> Building pjdfstest...'; \
		rm -rf /tmp/pjdfstest-check && \
		git clone --depth 1 https://github.com/pjd/pjdfstest /tmp/pjdfstest-check && \
		cd /tmp/pjdfstest-check && autoreconf -ifs && ./configure && make; \
	fi

# Hugepage pool for privileged tests (512 pages = 1GB, enough for 512MB test VMs)
HUGEPAGE_POOL_TESTS := 512
# The pool lock (/mnt/fcvm-btrfs/hugepage-pool.lock) is shared with
# bench-chromium-fault and bench/chromium/reqbench.sh, and taken through
# scripts/hugepage-pool-lock.sh, which opens it read-only (never O_CREAT, so
# fs.protected_regular cannot refuse it whoever owns it) and creates it
# atomically when absent. Do not touch, chmod, or chown the lock here: a
# root-created file with a `flock <path>` open behind it is how every
# unprivileged run after the first died with "Permission denied". (Keep this
# comment HERE: a `#` line inside the continued recipe below is shell text
# and eats the trailing backslash -- that is how #868's first revision
# produced `syntax error: unexpected end of file`.)
setup-hugepages:
	@current=$$(cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages 2>/dev/null || echo 0); \
	if [ "$$current" -ge "$(HUGEPAGE_POOL_TESTS)" ]; then \
		echo "==> Hugepages already allocated: $$current (need $(HUGEPAGE_POOL_TESTS))"; \
	else \
		echo "==> Allocating hugepage pool ($(HUGEPAGE_POOL_TESTS) pages = $$(( $(HUGEPAGE_POOL_TESTS) * 2 ))MB)..."; \
		scripts/hugepage-pool-lock.sh \
			sudo sh -c 'echo $(HUGEPAGE_POOL_TESTS) > /proc/sys/vm/nr_hugepages'; \
	fi

setup-btrfs:
	@if [ -d /mnt/fcvm-btrfs ] && stat -f -c '%T' /mnt/fcvm-btrfs 2>/dev/null | grep -q btrfs; then \
		echo '==> /mnt/fcvm-btrfs already on btrfs'; \
	elif stat -f -c '%T' /mnt 2>/dev/null | grep -q btrfs; then \
		echo '==> /mnt is btrfs, creating /mnt/fcvm-btrfs as directory (no loopback needed)'; \
		sudo mkdir -p /mnt/fcvm-btrfs && \
		sudo chown $$(id -un):$$(id -gn) /mnt/fcvm-btrfs && \
		mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,initrd,cache,image-cache}; \
	elif ! mountpoint -q /mnt/fcvm-btrfs 2>/dev/null; then \
		echo '==> Creating btrfs loopback (host is not btrfs)...'; \
		if [ ! -f /var/fcvm-btrfs.img ]; then \
			sudo truncate -s 60G /var/fcvm-btrfs.img && sudo mkfs.btrfs /var/fcvm-btrfs.img; \
		fi && \
		sudo mkdir -p /mnt/fcvm-btrfs && \
		sudo mount -o loop /var/fcvm-btrfs.img /mnt/fcvm-btrfs && \
		sudo mkdir -p /mnt/fcvm-btrfs/{kernels,rootfs,initrd,cache,image-cache} && \
		sudo chown -R $$(id -un):$$(id -gn) /mnt/fcvm-btrfs && \
		echo '==> btrfs ready at /mnt/fcvm-btrfs'; \
	fi
	@# Ensure these dirs exist with correct permissions (may be missing after reboot/corruption)
	@sudo mkdir -p /mnt/fcvm-btrfs/image-cache /mnt/fcvm-btrfs/containers
	@sudo chown $$(id -un):$$(id -gn) /mnt/fcvm-btrfs/image-cache /mnt/fcvm-btrfs/containers
	@# Heal root-owned store entries (see the script header for why)
	@./scripts/normalize-store-ownership.sh /mnt/fcvm-btrfs
	@# Enable IP forwarding (required for bridged networking)
	@sudo sysctl -q -w net.ipv4.ip_forward=1
	@# Create per-mode data directories (state, snapshots, vm-disks)
	@# Default: owned by current user (test-fast runs as ubuntu)
	@mkdir -p /mnt/fcvm-btrfs/{state,snapshots,vm-disks,tmp}
	@# ROOT_DATA_DIR: owned by root (test-root runs with sudo)
	@sudo mkdir -p $(ROOT_DATA_DIR)/{state,snapshots,vm-disks}
	@# CONTAINER_DATA_DIR: owned by current user (podman rootless maps to subordinate UIDs)
	@sudo mkdir -p $(CONTAINER_DATA_DIR)/{state,snapshots,vm-disks}
	@sudo chown -R $$(id -un):$$(id -gn) $(CONTAINER_DATA_DIR)

setup-default: build setup-btrfs
	@FREE_GB=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$FREE_GB" ] && [ "$$FREE_GB" -lt 15 ]; then \
		echo "ERROR: Need 15GB on /mnt/fcvm-btrfs (have $${FREE_GB}GB)"; \
		exit 1; \
	fi
	@echo "==> Running fcvm setup (default kernel; builds only if release is absent)..."
	@# Tests run fcvm via sudo, which reads /root/.config/fcvm — sync BOTH the
	@# user and root configs or a rootfs-config.toml change silently boots the
	@# previous rootfs under sudo (root keeps the stale SHA).
	sudo ./target/release/fcvm setup --generate-config --force
	./target/release/fcvm setup --kernel-profile default --build-kernels

# The default kernel as a publishable release artifact.
#
# The release workflow uses this instead of calling `fcvm setup` directly, so that
# storage setup, config synchronisation and any future build routing stay in one
# place rather than being duplicated in YAML.
#
# FORCE=1 makes a forced rebuild actually rebuild, via --force-build-kernels.
# Deleting the cached file is NOT enough: the release being replaced is still
# published at build time, so `--build-kernels` (which only builds after a FAILED
# download) downloads it again and the job republishes the exact artifact the
# operator asked to replace. A post-run existence check cannot catch that, since
# a download leaves the same file. KERNEL_FILE names the artifact to assert on.
release-default-kernel: build setup-btrfs
	@if [ "$(FORCE)" = "1" ] && [ -z "$(KERNEL_FILE)" ]; then 		echo "ERROR: FORCE=1 needs KERNEL_FILE=<vmlinux-...bin> to assert the rebuild produced it"; 		exit 1; 	fi
	sudo ./target/release/fcvm setup --generate-config --force
	@if [ "$(FORCE)" = "1" ]; then 		echo "==> FORCE: rebuilding from source, bypassing the published release"; 		sudo ./target/release/fcvm setup --kernel-profile default --force-build-kernels 			--config "$(CURDIR)/rootfs-config.toml"; 	else 		sudo ./target/release/fcvm setup --kernel-profile default --build-kernels 			--config "$(CURDIR)/rootfs-config.toml"; 	fi
	@if [ -n "$(KERNEL_FILE)" ] && [ ! -f "/mnt/fcvm-btrfs/kernels/$(KERNEL_FILE)" ]; then 		echo "ERROR: setup finished without producing /mnt/fcvm-btrfs/kernels/$(KERNEL_FILE)"; 		ls -la /mnt/fcvm-btrfs/kernels/; 		exit 1; 	fi

setup-fcvm: setup-default
	@echo "==> Running fcvm setup --kernel-profile nested..."
	./target/release/fcvm setup --kernel-profile nested --build-kernels
	@echo "==> Running fcvm setup --kernel-profile btrfs..."
	./target/release/fcvm setup --kernel-profile btrfs --build-kernels

# Build and install host kernel with all patches from kernel/patches/
# Requires reboot to activate the new kernel
install-host-kernel: build setup-btrfs
	sudo ./target/release/fcvm setup --kernel-profile nested --build-kernels --install-host-kernel

# Run setup inside container (for CI - container has Firecracker)
container-setup-fcvm: container-build setup-btrfs
	@echo "==> Running fcvm setup in container..."
	@# Fix ownership for rootless podman: container UID 0 maps to host user,
	@# so /mnt/fcvm-btrfs/firecracker must be writable by current user (not root)
	@sudo chown -R $$(id -un):$$(id -gn) /mnt/fcvm-btrfs/firecracker 2>/dev/null || true
	$(CONTAINER_RUN) $(CONTAINER_TAG) make build _setup-fcvm

_setup-fcvm:
	@FREE_GB=$$(df -BG /mnt/fcvm-btrfs 2>/dev/null | awk 'NR==2 {gsub("G",""); print $$4}'); \
	if [ -n "$$FREE_GB" ] && [ "$$FREE_GB" -lt 15 ]; then \
		echo "ERROR: Need 15GB on /mnt/fcvm-btrfs (have $${FREE_GB}GB)"; \
		exit 1; \
	fi
	sudo ./target/release/fcvm setup --generate-config --force
	sudo ./target/release/fcvm setup --kernel-profile default --build-kernels
	sudo ./target/release/fcvm setup --kernel-profile nested --build-kernels
	sudo ./target/release/fcvm setup --kernel-profile btrfs --build-kernels

# SDK E2E test — requires computesdk package as sibling repo and Node.js
test-serve-sdk: build
	@echo "==> Running ComputeSDK E2E test..."
	@if [ ! -d "$(CURDIR)/../computesdk/packages/computesdk" ]; then \
		echo "ERROR: computesdk not found at ../computesdk"; \
		echo "Clone it: git clone <computesdk-repo> ../computesdk && cd ../computesdk && pnpm install && pnpm build"; \
		exit 1; \
	fi
	cd tests && npm install --silent
	npx tsx tests/test_serve_sdk.ts

# Chromium shared-nothing benchmark: per-request clone latency + memory density
# across every egress path, vs host-native baselines. Results land in
# bench/chromium/results/<timestamp>/report.md. Knobs: R, REBUILD, PHASES —
# see bench/chromium/bench.sh header.
bench-chromium: build
	@echo "==> Running Chromium shared-nothing benchmark..."
	@bash bench/chromium/bench.sh run

# Request-optimized Chromium benchmark: one make target per reqbench.sh phase,
# so the dependency chain is explicit instead of discovered at runtime. The
# golden needs the fcvm binary AND the default-profile assets (kernel, rootfs,
# initrd, firecracker — that is `setup-default`, whose absence is the
# "Custom firecracker not found" golden failure of 2026-08-13). The measured
# phases (verify/run) deliberately do NOT depend on `build`: reqbench.sh
# stages fcvm+fc-agent+its own sources into a hash-sealed runtime bundle, and
# the run refuses a golden whose provenance records a different bundle hash —
# a rebuild between golden and run would swap the binary under test, so the
# binary must come from the golden-time build and a missing/stale one fails
# closed with a clear error. Structural pin: MakefileBenchGraph in
# bench/chromium/test_reqbench.py (make test-chromium-request).
#
# Knobs reach reqbench.sh through the environment — make exports command-line
# variables — so e.g.:
#   make bench-chromium-request-golden TAG=cb-req-golden-huge HUGEPAGES=1
#   make bench-chromium-request-run TAG=cb-req-golden-huge UFFD_MODE=minor \
#        UFFD_PREFETCH=on REPS=202 ARMS=exec,noop,cdp-fast,cdp
# The run driver invokes reqanalyze.py afterwards and propagates its
# publication-gate status, so a Make success means both the producer and the
# analyzer accepted the result.
BACKEND ?= uffd
REPS ?= 200
WARMUP ?= 2
ifndef RESULTS
RESULTS := $(CURDIR)/bench/chromium/results/reqbench-$(shell date +%Y%m%d-%H%M%S)-$(BACKEND)
endif

bench-chromium-request-build: build
	@echo "==> Building Chromium request-bench container image..."
	@bash bench/chromium/reqbench.sh build

# The WebKit arm's image. Separate from the Chromium one on purpose: they are
# different engines behind the same BENCH_READY contract, and rebuilding the
# Chromium image changes its digest, which invalidates goldens already recorded
# against it.
bench-webkit-request-build: build
	@echo "==> Building WebKit request-bench container image..."
	@ENGINE=webkit bash bench/chromium/reqbench.sh build

# The WebKit twins of the chromium request targets: same reqbench.sh, same
# seal, same gates -- ENGINE=webkit swaps the render driver (wddrive over W3C
# WebDriver classic on 9515), the image default, and the arm list (cdp,noop:
# cdp-fast is CDP WebSocket prewiring and exec's guest driver is CDP-only).
bench-webkit-request-golden: bench-webkit-request-build setup-default
	@echo "==> Creating WebKit golden snapshot (TAG=$(if $(TAG),$(TAG),cb-req-webkit))..."
	@ENGINE=webkit TAG=$(if $(TAG),$(TAG),cb-req-webkit) bash bench/chromium/reqbench.sh golden

bench-webkit-request-verify:
	@echo "==> Verifying WD hops on a restored WebKit clone..."
	@ENGINE=webkit TAG=$(if $(TAG),$(TAG),cb-req-webkit) bash bench/chromium/reqbench.sh verify

bench-webkit-request-run:
	@echo "==> Running WebKit request benchmark ($(BACKEND), $(REPS) measured attempts per arm)..."
	@ENGINE=webkit TAG=$(if $(TAG),$(TAG),cb-req-webkit) \
		BACKEND="$(BACKEND)" REPS="$(REPS)" WARMUP="$(WARMUP)" RESULTS="$(RESULTS)" \
		bash bench/chromium/reqbench.sh run

# TAG is quoted: unquoted, `TAG='x #'` left bash an assignment followed by a
# comment and the target exited 0 having run nothing; reqbench.sh is what
# refuses a bad tag and has to be reached.
bench-webkit-request-diag:
	@echo "==> Diagnosing page loads on restored WebKit clones ($(BACKEND), $(if $(DIAG_REPS),$(DIAG_REPS),3) clone(s) per URL)..."
	@ENGINE=webkit TAG="$(if $(TAG),$(TAG),cb-req-webkit)" \
		BACKEND="$(BACKEND)" DIAG_URLS="$(DIAG_URLS)" DIAG_REPS="$(DIAG_REPS)" \
		DIAG_EXPECT_IPS="$(DIAG_EXPECT_IPS)" DIAG_MAX_LOAD_MS="$(DIAG_MAX_LOAD_MS)" \
		RESULTS="$(RESULTS)" bash bench/chromium/reqbench.sh diag

bench-chromium-request-golden: bench-chromium-request-build setup-default
	@echo "==> Creating golden snapshot (TAG=$(if $(TAG),$(TAG),cb-req-golden), HUGEPAGES=$(if $(HUGEPAGES),$(HUGEPAGES),0))..."
	@bash bench/chromium/reqbench.sh golden

bench-chromium-request-verify:
	@echo "==> Verifying CDP hops on a restored clone..."
	@bash bench/chromium/reqbench.sh verify

bench-chromium-request-run:
	@echo "==> Running gated Chromium request benchmark ($(BACKEND), $(REPS) measured attempts per arm)..."
	@BACKEND="$(BACKEND)" REPS="$(REPS)" WARMUP="$(WARMUP)" RESULTS="$(RESULTS)" \
		bash bench/chromium/reqbench.sh run

# In-guest load diagnostics on the golden the run uses: one clone per (URL,
# rep), one render each with cdpdrive's --net-trace (Network.* rows the
# measured arms never ask for), and a summary naming every remote IP, name
# that did not resolve, stall and failed render. Same seal rule as verify and
# run: no build dependency. The DIAG_* knobs are forwarded the way run
# forwards BACKEND and RESULTS; reqbench.sh holds their defaults.
bench-chromium-request-diag:
	@echo "==> Diagnosing page loads on restored clones ($(BACKEND), $(if $(DIAG_REPS),$(DIAG_REPS),3) clone(s) per URL)..."
	@BACKEND="$(BACKEND)" DIAG_URLS="$(DIAG_URLS)" DIAG_REPS="$(DIAG_REPS)" \
		DIAG_EXPECT_IPS="$(DIAG_EXPECT_IPS)" DIAG_MAX_LOAD_MS="$(DIAG_MAX_LOAD_MS)" \
		RESULTS="$(RESULTS)" bash bench/chromium/reqbench.sh diag

bench-chromium-request-all: build setup-default
	@echo "==> Full request-bench chain (image, golden, verify, measured run) under one seal..."
	@BACKEND="$(BACKEND)" REPS="$(REPS)" WARMUP="$(WARMUP)" RESULTS="$(RESULTS)" \
		bash bench/chromium/reqbench.sh all

# Corpus campaign, PACKAGED PER RUN.
#
# corpus_campaign.sh is the orchestrator, and bash reads a script incrementally
# by byte offset: editing it while it runs resumes mid-line and executes
# whatever the shifted bytes happen to spell. Observed 2026-08-16, inserting a
# block into a running campaign:
#     corpus_campaign.sh: line 201: not: command not found
# which killed a three-cell sweep after its first cell had already gated clean.
#
# reqbench.sh already solves this FOR ITSELF: it stages its five sources plus
# the two binaries into a content-addressed bundle, chmod 0555, and execs the
# copy. This applies the same discipline one layer up, from make, so a campaign
# is immune to edits in the working tree for its whole lifetime. Everything for
# one run -- the frozen orchestrator, its manifest, the source revision, and
# every reqbench record -- lands under a single directory.
.PHONY: bench-stop
# `:=` NOT `?=`/`=`. A recursively expanded $(shell date) re-runs on every
# reference, so a run that crosses a second boundary creates one directory and
# writes later artifacts into another -- the exact contamination this per-run
# packaging exists to prevent. checkmake flags the recursive form for this
# reason (timestampexpanded). An explicit CORPUS_STAMP= on the command line
# still wins, because make gives command-line variables precedence.
CORPUS_STAMP := $(shell date +%Y%m%d-%H%M%S)
CORPUS_RUN_DIR := $(CURDIR)/bench/chromium/results/corpus-$(CORPUS_STAMP)
# Stop every benchmark process cleanly, and put the host back.
#
# A killed campaign leaves three kinds of debris: its own orchestrator and
# samplers, the microVMs it had in flight, and the host services it borrowed.
# The campaign script stops dnsmasq to take 127.0.0.1:53 for the replay server
# and restores it from an EXIT trap -- which does not run when the script is
# SIGKILLed, so a killed campaign leaves the box with no dnsmasq.
#
# It NEVER kills its own process tree. `pkill -f` matches whole command lines,
# so it will happily kill the shell that invoked make whenever that shell's
# command line merely MENTIONS a pattern -- which happens constantly: an editor
# session, a grep, a heredoc containing these very comments. Bracketing the
# pattern is not enough, because the mention may be unbracketed in the caller.
# Observed here: make died with 144 before restoring dnsmasq, because the
# invoking shell's command line contained the campaign script's name in prose.
# So matches are filtered against this process's own ancestor chain first.
#
# Stray microVM groups are delegated to ci-stray-vm-guard.sh rather than
# reimplemented: it captures per-TID kernel stacks, wchan and status BEFORE
# killing, which is exactly the state SIGKILL destroys and exactly what you
# need when a firecracker is stuck non-zombie in D state.
.PHONY: bench-stop
bench-stop:
	@echo "==> stopping benchmark orchestrators and samplers"
	@# PPid from /proc/<pid>/status, NOT field 4 of /proc/<pid>/stat: comm sits in
	@# field 2 wrapped in parens and may contain spaces, which shifts every later
	@# field. That misparse read the state field and failed with
	@#   [: S: integer expression expected
	@ancestors=" $$$$ "; pid=$$$$; \
	while [ "$$pid" -gt 1 ]; do \
		pid=$$(awk '/^PPid:/{print $$2}' /proc/$$pid/status 2>/dev/null); \
		case "$$pid" in ''|*[!0-9]*) break ;; esac; \
		ancestors="$$ancestors$$pid "; \
	done; \
	for pat in 'corpus_campaign\.sh' 'cpuprobe\.py' 'reqbench\.sh' 'reqbench\.py' \
	           'reqscale\.py' 'corpus_serve\.py'; do \
		for victim in $$(pgrep -f "$$pat" 2>/dev/null || true); do \
			case "$$ancestors" in *" $$victim "*) continue ;; esac; \
			echo "  kill $$victim ($$pat)"; \
			kill "$$victim" 2>/dev/null || sudo kill "$$victim" 2>/dev/null || true; \
		done; \
	done
	@sleep 2
	@echo "==> reaping stray microVM process groups (evidence captured first)"
	@-sudo bash scripts/ci-stray-vm-guard.sh bench-stop || true
	@echo "==> restoring host services the campaign borrows"
	@# The campaign takes 127.0.0.1:53 from dnsmasq. corpus_serve.py can still be
	@# holding that port while it shuts down, so `systemctl start` loses the race
	@# and, suppressed, this target printed "clean" over a box left with NO DNS
	@# resolution -- the failure a teardown target exists to prevent. Retry until
	@# the port is free, then REPORT the outcome rather than asserting it.
	@for i in 1 2 3 4 5 6 7 8 9 10; do \
		sudo systemctl start dnsmasq >/dev/null 2>&1 && break; \
		sleep 1; \
	done; \
	state=$$(systemctl is-active dnsmasq 2>/dev/null || echo unknown); \
	echo "dnsmasq=$$state"; \
	if [ "$$state" != active ]; then \
		echo "FAILED: dnsmasq is $$state after teardown; this box has no DNS. \
Check for a process still holding :53 (sudo ss -lnup 'sport = :53')." >&2; \
		exit 1; \
	fi
	@echo "==> clean"

# A campaign against a DIRTY tree records a source_revision that does not
# contain the code that ran. The seal binds the revision and the runtime bundle
# hash, but an uncommitted edit leaves the revision identical while changing the
# behaviour, so the record looks sealed and is unreproducible. Untracked files
# are fine (results/, scratch); modifications to tracked files are not.
.PHONY: require-clean-tree
require-clean-tree:
	@dirty="$$(git -C "$(CURDIR)" status --porcelain --untracked-files=no)"; \
	if [ -n "$$dirty" ]; then \
		echo "REFUSING: uncommitted changes to tracked files. A measured run would record a"; \
		echo "source_revision that does not describe what ran. Commit or stash first:"; \
		echo "$$dirty"; \
		exit 2; \
	fi

bench-chromium-corpus: require-clean-tree build setup-default
	@mkdir -p "$(dir $(CORPUS_RUN_DIR))"
	@# Reserve, do not reuse: plain mkdir FAILS on collision. `mkdir -p` would
	@# accept an existing directory and interleave a second campaign's records
	@# with the first's under one stamp, which is unrecoverable after the fact
	@# because nothing in a record says which run wrote it.
	@mkdir "$(CORPUS_RUN_DIR)" || { \
		echo "REFUSING: $(CORPUS_RUN_DIR) already exists; pass CORPUS_STAMP=<new> for a second run" >&2; \
		exit 1; }
	@mkdir -p "$(CORPUS_RUN_DIR)/orchestrator"
	@cp bench/chromium/corpus_campaign.sh "$(CORPUS_RUN_DIR)/orchestrator/corpus_campaign.sh"
	@chmod 0555 "$(CORPUS_RUN_DIR)/orchestrator/corpus_campaign.sh"
	@cd "$(CORPUS_RUN_DIR)/orchestrator" && sha256sum corpus_campaign.sh > MANIFEST.sha256
	@sha256sum "$(CURDIR)/target/release/fcvm" >> "$(CORPUS_RUN_DIR)/orchestrator/MANIFEST.sha256"
	@git -C "$(CURDIR)" rev-parse HEAD > "$(CORPUS_RUN_DIR)/orchestrator/SOURCE_REVISION"
	@# Pin the revision behind a ref of its own. Every record cites
	@# source_revision, and reqbench REFUSES a golden whose revision differs from
	@# the running tree -- so a record is only reproducible while its commit is
	@# still reachable. A squash-merge or a deleted branch makes that SHA
	@# unreachable and the record becomes an orphan citation. Branches are cheap;
	@# an unreproducible measurement is not.
	@rev="$$(git -C "$(CURDIR)" rev-parse HEAD)"; \
	ref="bench-run/$(CORPUS_STAMP)-$${rev%$${rev#??????????}}"; \
	if existing="$$(git -C "$(CURDIR)" rev-parse --verify -q "refs/heads/$$ref")"; then \
		[ "$$existing" = "$$rev" ] || { \
			echo "REFUSING: $$ref already pins $$existing, not $$rev; an earlier record cites it" >&2; \
			exit 1; }; \
	else \
		git -C "$(CURDIR)" branch "$$ref" "$$rev" >/dev/null || { \
			echo "REFUSING: could not pin $$rev behind $$ref; the record would cite an unreachable commit" >&2; \
			exit 1; }; \
	fi; \
	echo "$$ref" > "$(CORPUS_RUN_DIR)/orchestrator/SOURCE_REF"; \
	if git -C "$(CURDIR)" push -q origin "refs/heads/$$ref:refs/heads/$$ref" 2>/dev/null; then \
		echo "pushed" >> "$(CORPUS_RUN_DIR)/orchestrator/SOURCE_REF"; \
		echo "==> revision pinned: $$ref (pushed)"; \
	else \
		echo "LOCAL-ONLY" >> "$(CORPUS_RUN_DIR)/orchestrator/SOURCE_REF"; \
		echo "==> WARNING: $$ref exists only locally; push it or this run's revision"; \
		echo "    can be lost to a squash-merge and the records become unreproducible"; \
	fi
	@echo "==> campaign frozen at $(CORPUS_RUN_DIR)/orchestrator (read-only; edits to the tree cannot reach this run)"
	@REPO="$(CURDIR)" RESULTS="$(CORPUS_RUN_DIR)/reqbench" \
		bash "$(CORPUS_RUN_DIR)/orchestrator/corpus_campaign.sh"

# Host-container direct-CDP baseline (no VM): same image, same driver, warm
# pool on the host. Needs only the container image, not VM assets.
bench-chromium-hostcdp: bench-chromium-request-build
	@echo "==> Running host-container direct-CDP baseline..."
	@bash bench/chromium/hostcdp.sh

# Per-request guest page-fault count/cost per memory backend. Requires the
# bench.sh goldens (cb-golden-*) to exist; cells without one are skipped.
# faultbench selects uffd-huge-minor whenever the huge golden exists, and
# bench.sh restores the pool (commonly to zero) after creating that golden —
# so the pool must be provisioned HERE or the entry point fails immediately
# after the workflow that created its own prerequisite. Default sized for
# --guest-mib 2048: backing memfd (1024 pages) + concurrent clones.
FAULT_POOL ?= 4096
bench-chromium-fault: build setup-default
	@test -n "$(FAULT_OUT)" || (echo "ERROR: FAULT_OUT required (results directory)"; exit 1)
	@if ls -d /mnt/fcvm-btrfs/snapshots/cb-golden-huge* >/dev/null 2>&1; then \
		scripts/hugepage-pool-lock.sh sh -c ' \
			current=$$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || echo 0); \
			if [ "$$current" -lt "$(FAULT_POOL)" ]; then \
				echo "==> Growing hugepage pool $$current -> $(FAULT_POOL) for the huge cells..."; \
				sudo sh -c "echo $(FAULT_POOL) > /proc/sys/vm/nr_hugepages"; \
				after=$$(cat /proc/sys/vm/nr_hugepages 2>/dev/null || echo 0); \
				if [ "$$after" -lt "$(FAULT_POOL)" ]; then \
					echo "ERROR: hugepage pool only $$after/$(FAULT_POOL) pages (fragmentation)"; \
					exit 1; \
				fi; \
			fi'; \
	fi
	@echo "==> Running per-request page-fault benchmark..."
	@python3 bench/chromium/faultbench.py --out "$(FAULT_OUT)" $(FAULT_ARGS)

analyze-chromium-request:
	@test -f "$(RESULTS)/reqbench.jsonl" || { echo "ERROR: no $(RESULTS)/reqbench.jsonl" >&2; exit 2; }
	@python3 bench/chromium/reqanalyze.py --json-out "$(RESULTS)/analysis.json" \
		$(if $(STALL_MAX_MS),--stall-max-ms $(STALL_MAX_MS),) "$(RESULTS)/reqbench.jsonl"

# What CI runs (ci.yml globs test_*.py). The per-harness targets below are
# narrower dev conveniences; run THIS before pushing, because a new test file is
# invisible to every one of them until somebody remembers to add a target, and
# the failure mode of forgetting is a green local run.
test-chromium:
	@python3 -m unittest discover -s bench/chromium -p 'test_*.py' \
		$(if $(FILTER),-k '$(FILTER)',)

test-chromium-request:
	@python3 -m unittest discover -s bench/chromium -p 'test_reqbench.py' \
		$(if $(FILTER),-k '$(FILTER)',)

test-chromium-scale:
	@python3 -m unittest discover -s bench/chromium -p 'test_reqscale.py' \
		$(if $(FILTER),-k '$(FILTER)',)

test-chromium-fault:
	@python3 -m unittest discover -s bench/chromium -p 'test_faultbench.py' \
		$(if $(FILTER),-k '$(FILTER)',)

# Open-loop CDP-fast scalability and page-fault measurement. Unlike
# `bench-chromium-request`, whose driver loops `for rep in range(...)` and so can
# only ever measure a closed loop, this one launches on absolute deadlines and can
# therefore hold an arrival RATE. It also interleaves FILE and UFFD inside each
# rate interval rather than across separate runs, so the backend comparison is not
# confounded with drift.
#
# Every cell and every publication gate is required: an accidental benchmark is
# worse than no benchmark, so there are no defaults to fall back on.
bench-chromium-scale: build
	@test -n "$(SCALE_RATES)" || (echo "ERROR: SCALE_RATES required (for example 2,4,8)"; exit 1)
	@test -n "$(SCALE_BURSTS)" || (echo "ERROR: SCALE_BURSTS required (must be at least 5)"; exit 1)
	@test -n "$(SCALE_SEED)" || (echo "ERROR: SCALE_SEED required"; exit 1)
	@test -n "$(SCALE_OUT)" || (echo "ERROR: SCALE_OUT required"; exit 1)
	@test -n "$(SCALE_URL)" || (echo "ERROR: SCALE_URL required"; exit 1)
	@test -n "$(SCALE_TAG)" || (echo "ERROR: SCALE_TAG required"; exit 1)
	@test -n "$(SCALE_CONTROL_CHROMIUM)" || (echo "ERROR: SCALE_CONTROL_CHROMIUM required"; exit 1)
	@test -n "$(SCALE_MAX_OFFERED_ERROR_PCT)" || (echo "ERROR: SCALE_MAX_OFFERED_ERROR_PCT required"; exit 1)
	@test -n "$(SCALE_MIN_DEPARTURE_RATIO)" || (echo "ERROR: SCALE_MIN_DEPARTURE_RATIO required"; exit 1)
	@test -n "$(SCALE_MAX_BACKLOG)" || (echo "ERROR: SCALE_MAX_BACKLOG required"; exit 1)
	@test -n "$(SCALE_MAX_LAUNCH_LAG_MS)" || (echo "ERROR: SCALE_MAX_LAUNCH_LAG_MS required"; exit 1)
	@test -n "$(SCALE_MAX_CONTROL_DRIFT_PCT)" || (echo "ERROR: SCALE_MAX_CONTROL_DRIFT_PCT required"; exit 1)
	@echo "==> Running open-loop Chromium request scalability benchmark..."
	sudo -E env RUST_LOG=fcvm=debug python3 bench/chromium/reqscale.py \
		--fcvm ./target/release/fcvm --snapshot-tag "$(SCALE_TAG)" \
		--url "$(SCALE_URL)" --rates "$(SCALE_RATES)" \
		--bursts "$(SCALE_BURSTS)" --control-chromium "$(SCALE_CONTROL_CHROMIUM)" \
		--max-offered-rps-error-pct "$(SCALE_MAX_OFFERED_ERROR_PCT)" \
		--min-departure-ratio "$(SCALE_MIN_DEPARTURE_RATIO)" \
		--max-score-end-backlog "$(SCALE_MAX_BACKLOG)" \
		--max-p95-launch-lag-ms "$(SCALE_MAX_LAUNCH_LAG_MS)" \
		--max-control-median-drift-pct "$(SCALE_MAX_CONTROL_DRIFT_PCT)" \
		--seed "$(SCALE_SEED)" \
		--out-dir "$(SCALE_OUT)" $(SCALE_TRACE_ARGS)

analyze-chromium-scale:
	@test -n "$(SCALE_RUN_DIR)" || (echo "ERROR: SCALE_RUN_DIR required"; exit 1)
	@test -n "$(SCALE_ANALYSIS_JSON)" || (echo "ERROR: SCALE_ANALYSIS_JSON required"; exit 1)
	python3 bench/chromium/reqscale_analyze.py \
		--run-dir "$(SCALE_RUN_DIR)" --json-out "$(SCALE_ANALYSIS_JSON)"

report-chromium-scale:
	@test -n "$(SCALE_RUN_DIR)" || (echo "ERROR: SCALE_RUN_DIR required"; exit 1)
	@test -n "$(SCALE_REPORT)" || (echo "ERROR: SCALE_REPORT required"; exit 1)
	python3 bench/chromium/reqscale_analyze.py \
		--run-dir "$(SCALE_RUN_DIR)" --markdown-out "$(SCALE_REPORT)"

# Run three ordinary nextest lanes concurrently. Each lane keeps the repository's
# normal scheduling (in particular, hugepage tests remain serialized by their
# configured test group); separate lanes are needed on one-CPU test hosts where a
# single nextest process cannot overlap the clone stress test with lifecycle tests.
# Standard helpers keep writing their per-fcvm debug logs, while each lane also
# captures its complete nextest and cleanup transcript.
CLONE_FLOOR_CLONE_FILTER := -E 'package(fcvm) & test(=test_snapshot_clone_stress_100_bridged)'
CLONE_FLOOR_CH_FILTER := -E 'package(fcvm) & test(=test_cloud_hypervisor_cold_boot)'
CLONE_FLOOR_HUGEPAGE_FILTER := -E 'package(fcvm) & (test(=test_hugepage_vm_boot) | test(=test_hugepage_cache_restore_uses_uffd) | test(=test_hugepage_snapshot_clone))'
test-clone-floor-overlap: show-notes check-disk setup-fcvm setup-hugepages
	@mkdir -p $(TEST_LOG_DIR)
	@set -uo pipefail; \
		run_lane() { \
			lane="$$1"; filter="$$2"; \
			$(MAKE) --no-print-directory _test-root STREAM=1 FILTER="$$filter" 2>&1 | \
				tee "$(TEST_LOG_DIR)/clone-floor-overlap-$${lane}.log"; \
		}; \
		run_lane clones "$(CLONE_FLOOR_CLONE_FILTER)" & clones_job=$$!; \
		run_lane cloud-hypervisor "$(CLONE_FLOOR_CH_FILTER)" & ch_job=$$!; \
		run_lane hugepages "$(CLONE_FLOOR_HUGEPAGE_FILTER)" & hugepages_job=$$!; \
		clones_rc=0; wait "$$clones_job" || clones_rc=$$?; \
		ch_rc=0; wait "$$ch_job" || ch_rc=$$?; \
		hugepages_rc=0; wait "$$hugepages_job" || hugepages_rc=$$?; \
		echo "clone-floor overlap results: clones=$$clones_rc cloud-hypervisor=$$ch_rc hugepages=$$hugepages_rc"; \
		test "$$clones_rc" -eq 0 -a "$$ch_rc" -eq 0 -a "$$hugepages_rc" -eq 0

test-ci-infrastructure:
	@PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_ci_infrastructure.py'

# One criterion suite under the privileged runner. throughput and operations
# mount a real FUSE filesystem, so they take the runner the root tests use;
# protocol is pure serialization and stays unprivileged.
#
# Each privileged suite hands target/ back afterwards. criterion writes its
# results from inside the bench binary, so under that runner target/criterion
# ends up root-owned, and two things break: bench-protocol, which is
# deliberately unprivileged, fails every write with `Permission denied (os
# error 13)` — it still prints timings but persists nothing, so criterion never
# holds a baseline for it and can never report a change — and `_test-root`
# refuses to start until the ownership is repaired. The repair is inline rather
# than a $(MAKE) call because a sub-make runs even under `make -n`, and this
# one would invoke sudo.
#
# The repair's own status decides the recipe's. Keeping only the benchmark's
# status reports the suite green while target/ is still root-owned, which is
# precisely the state the repair exists to prevent, and the next unprivileged
# run is the one that pays for it. The scan is treated the same way: a find
# that cannot read target/ has not established that no repair is needed.
define run_privileged_bench
	CRITERION_HOME='$(CRITERION_HOME)' \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(CARGO) bench -p fuse-pipe --bench $(1) $(BENCH_SEPARATED); \
	bench_rc=$$?; \
	if [ ! -d '$(CRITERION_HOME)' ]; then \
		if [ $$bench_rc -eq 0 ]; then \
			echo "ERROR: --bench $(1) exited 0 but $(CRITERION_HOME) does not exist. criterion logs persistence failures and still returns 0, so the suite printed timings and saved no sample.json, estimates.json or baseline: nothing can be compared and no regression can ever be reported" >&2; \
			exit 1; \
		fi; \
		exit $$bench_rc; \
	fi; \
	if ! root_owned=$$(find '$(CRITERION_HOME)' -user root -print -quit); then \
		echo "ERROR: cannot scan $(CRITERION_HOME) for root-owned files, so the ownership repair after --bench $(1) never ran" >&2; \
		exit 1; \
	fi; \
	if [ -n "$$root_owned" ] && ! sudo chown -R $$(id -u):$$(id -g) '$(CRITERION_HOME)'; then \
		echo "ERROR: $(CRITERION_HOME) is still root-owned after --bench $(1); bench-protocol cannot persist criterion output and _test-root will refuse to start" >&2; \
		exit 1; \
	fi; \
	exit $$bench_rc
endef

# One criterion suite as the invoking user. Nothing here writes as root, so
# there is no ownership to repair.
define run_unprivileged_bench
	CRITERION_HOME='$(CRITERION_HOME)' $(CARGO) bench -p fuse-pipe --bench $(1) $(BENCH_SEPARATED); \
	bench_rc=$$?; \
	if [ ! -d '$(CRITERION_HOME)' ] && [ $$bench_rc -eq 0 ]; then \
		echo "ERROR: --bench $(1) exited 0 but $(CRITERION_HOME) does not exist. criterion logs persistence failures and still returns 0, so the suite printed timings and saved no sample.json, estimates.json or baseline: nothing can be compared and no regression can ever be reported" >&2; \
		exit 1; \
	fi; \
	exit $$bench_rc
endef

# The three fuse-pipe suites, in order.
#
# One target with three recipe lines, not three prerequisites. Make
# parallelizes prerequisites but never the lines of a single recipe, so this
# cannot start two suites at once under `make -j`, nor under a -j inherited
# through MAKEFLAGS from a parent make. The prerequisite form did: with a stub
# cargo under `make -j8 bench`, all three suites were running together.
# Concurrent suites time each other, and the unprivileged protocol suite races
# the privileged suites for ownership of target/criterion. A comment saying
# "do not use -j" is not a control.
#
# GNU Make 4.3 is what this repo builds on, and neither of make's own
# mechanisms is usable there: `.NOTPARALLEL` ignores its prerequisites before
# 4.4 and serializes the whole makefile rather than this target, and `.WAIT`
# arrived in 4.4.
bench: build
	@echo "==> Running fuse-pipe benchmarks: throughput, operations, protocol"
	$(call run_privileged_bench,throughput)
	$(call run_privileged_bench,operations)
	$(call run_unprivileged_bench,protocol)

bench-throughput: build
	$(call run_privileged_bench,throughput)

bench-operations: build
	$(call run_privileged_bench,operations)

bench-protocol: build
	$(call run_unprivileged_bench,protocol)

# The same three suites, shortened for iteration. A target-specific variable
# reaches the prerequisite, so this needs no recursive make.
#
# Quote `make bench` in any report, never this: the intervals are wide. Note
# also that a group calling `sample_size()` in code keeps its own count, so
# --sample-size only reaches the groups that do not, while --warm-up-time and
# --measurement-time reach all of them.
bench-quick: BENCH_ARGS := --sample-size 10 --warm-up-time 1 --measurement-time 3
bench-quick: bench

# VM benchmarks (exec, clone) - require KVM, Firecracker, setup
bench-vm: build setup-default
	@echo "==> Running VM benchmarks..."
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(CARGO) bench --bench exec -- --test
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(CARGO) bench --bench clone -- --test

# Hugepages benchmark: compare clone speed with 4KB vs 2MB pages
# Full mode: 32GB VM, 16GB dirty memory (~20 min)
# Test mode: 2GB VM, 256MB dirty memory (~5 min)
HUGEPAGE_POOL_FULL := 17000
HUGEPAGE_POOL_TEST := 1200

bench-hugepages: build setup-default
	@echo "==> Allocating hugepage pool ($(HUGEPAGE_POOL_FULL) pages = $$(( $(HUGEPAGE_POOL_FULL) * 2 ))MB)..."
	scripts/hugepage-pool-lock.sh sudo sh -c 'echo $(HUGEPAGE_POOL_FULL) > /proc/sys/vm/nr_hugepages'
	@echo "==> Running hugepages benchmark (full)..."
	TMPDIR=/mnt/fcvm-btrfs/tmp $(CARGO) bench --bench hugepages; \
	RC=$$?; \
	echo "==> Releasing hugepage pool..."; \
	scripts/hugepage-pool-lock.sh sudo sh -c 'echo 0 > /proc/sys/vm/nr_hugepages'; \
	exit $$RC

bench-hugepages-test: build setup-default
	@echo "==> Allocating hugepage pool ($(HUGEPAGE_POOL_TEST) pages = $$(( $(HUGEPAGE_POOL_TEST) * 2 ))MB)..."
	scripts/hugepage-pool-lock.sh sudo sh -c 'echo $(HUGEPAGE_POOL_TEST) > /proc/sys/vm/nr_hugepages'
	@echo "==> Running hugepages benchmark (test)..."
	TMPDIR=/mnt/fcvm-btrfs/tmp $(CARGO) bench --bench hugepages -- --test; \
	RC=$$?; \
	echo "==> Releasing hugepage pool..."; \
	scripts/hugepage-pool-lock.sh sudo sh -c 'echo 0 > /proc/sys/vm/nr_hugepages'; \
	exit $$RC

bench-container-import: build setup-default
	@echo "==> Running container import benchmark..."
	$(CARGO) bench --bench container_import

# Clone hot-path latency: `fcvm snapshot run` spawn -> exec-server-ready, N times.
# Event-driven readiness (waits on the debug log's marker), plus a stage breakdown
# parsed from the RUST_LOG=fcvm=debug timeline.
# Results in /tmp/fcvm-clone-latency-$(LABEL)/ (never committed).
#   make bench-clone-latency LABEL=before N=10
bench-clone-latency: build setup-default
	@echo "==> Running clone latency benchmark..."
	bench/clone-latency.sh $(or $(LABEL),run) $(or $(N),10)

# Container benchmark target (used by nightly CI)
# Uses CONTAINER_RUN_BASE (no --ulimit nproc) to avoid EPERM on GHA ubuntu-latest
container-bench: check-disk container-build
	@echo "==> Running benchmarks in container..."
	$(CONTAINER_RUN_BASE) $(CONTAINER_TAG) make build _bench

_bench: cargo-target-link
	@echo "==> Running benchmarks..."
	$(CARGO) bench -p fuse-pipe --bench throughput
	$(CARGO) bench -p fuse-pipe --bench operations
	$(CARGO) bench -p fuse-pipe --bench protocol

# Lint tools versions (keep in sync with CI)
CARGO_AUDIT_VERSION := 0.22.0
CARGO_DENY_VERSION := 0.18.9

setup-lint-tools: cargo-target-link
	@which cargo-audit > /dev/null || (echo "Installing cargo-audit..." && $(CARGO) install cargo-audit@$(CARGO_AUDIT_VERSION) --locked)
	@which cargo-deny > /dev/null || (echo "Installing cargo-deny..." && $(CARGO) install cargo-deny@$(CARGO_DENY_VERSION) --locked)

lint: setup-lint-tools
	$(CARGO) fmt -p fcvm -p fuse-pipe -p fc-agent -p failpoint --check
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) audit
	$(CARGO) deny check

update-dependency: cargo-target-link
	@test -n "$(PACKAGE)" || (echo "ERROR: PACKAGE required"; exit 1)
	$(CARGO) update -p "$(PACKAGE)" $(if $(VERSION),--precise "$(VERSION)")

# CI merge train - pooled CI for a batch of independent low-risk PRs.
# One full CI matrix validates the whole batch instead of one per PR.
# Protocol, cost math, and when (not) to pool: docs/ci-train.md
# TRAIN selects the branch (default ci-train; bisect halves are ci-train-a/-b).
TRAIN ?= ci-train

.PHONY: train-create train-dispatch train-status train-land train-bisect
train-create:
	@if [ -n "$(CONTINUE)" ]; then \
		./scripts/ci-train.sh --branch $(TRAIN) create --continue; \
	elif [ -n "$(PRS)" ]; then \
		./scripts/ci-train.sh --branch $(TRAIN) create $(PRS); \
	else \
		echo 'ERROR: PRS required (e.g., make train-create PRS="689 690"),'; \
		echo '       or CONTINUE=1 to resume after a manual conflict resolution'; \
		exit 1; \
	fi

train-dispatch:
	./scripts/ci-train.sh --branch $(TRAIN) dispatch

train-status:
	./scripts/ci-train.sh --branch $(TRAIN) status

train-land:
	./scripts/ci-train.sh --branch $(TRAIN) land

train-bisect:
	./scripts/ci-train.sh --branch $(TRAIN) bisect

fmt: cargo-target-link
	$(CARGO) fmt

# SSH to jumpbox (IP from terraform: cd ~/src/aws && terraform output jumpbox_ssh_command)
JUMPBOX_IP := 54.193.62.221
ssh:
	ssh -i ~/.ssh/fcvm-ec2 ubuntu@$(JUMPBOX_IP)

# Kernel patch helpers - generates properly formatted patches
# Usage: make kernel-patch-create PROFILE=nested NAME=0004-my-fix FILE=fs/fuse/dir.c
PROFILE ?= nested
NAME ?=
PATCH ?=
FILE ?=

kernel-patch-create:
	@test -n "$(NAME)" || (echo "ERROR: NAME required (e.g., NAME=0004-my-fix)"; exit 1)
	@test -n "$(FILE)" || (echo "ERROR: FILE required (e.g., FILE=fs/fuse/dir.c)"; exit 1)
	./scripts/kernel-patch.sh create $(PROFILE) $(NAME) $(FILE)

kernel-patch-edit:
	@test -n "$(PATCH)" || (echo "ERROR: PATCH required (e.g., PATCH=0002)"; exit 1)
	./scripts/kernel-patch.sh edit $(PROFILE) $(PATCH)

kernel-patch-validate:
	./scripts/kernel-patch.sh validate $(PROFILE)
