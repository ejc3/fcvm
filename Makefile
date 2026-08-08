SHELL := /bin/bash

# Guard: never run make as root on the host (except clean). Running cargo
# as root leaves root-owned files in target/ that break subsequent user builds
# with BrokenPipe errors from nextest finding stale binaries.
# Skip this guard inside containers (where root is normal).
ifeq ($(shell id -u),0)
ifeq ($(filter clean,$(MAKECMDGOALS)),)
ifeq ($(wildcard /.dockerenv /run/.containerenv),)
$(error Do not run make as root. Use 'make test-root' as your normal user — it uses sudo only for the test runner)
endif
endif
endif

# Find Rust toolchain bin directory and set PATH
# Prefer stable (has musl target), fall back to any toolchain
RUST_BIN := $(shell command -v cargo >/dev/null 2>&1 && dirname $$(command -v cargo) || \
	(test -x $(HOME)/.cargo/bin/cargo && echo $(HOME)/.cargo/bin) || \
	(ls -d $(HOME)/.rustup/toolchains/stable-*/bin 2>/dev/null | head -1) || \
	(ls -d $(HOME)/.rustup/toolchains/*/bin 2>/dev/null | head -1))
export PATH := $(RUST_BIN):$(PATH)
CARGO := cargo

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


# Base test command
export CARGO_TARGET_DIR := target
NEXTEST := $(CARGO) nextest $(NEXTEST_CMD) --release

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
	test test-unit test-fast test-all test-root test-packaging fuzz \
	_test-unit _test-fast _test-all _test-root _setup-fcvm _bench \
	container-build container-test container-test-unit container-test-fast container-test-all container-test-fc-mock \
	container-setup-fcvm container-shell container-clean container-bench \
	setup-btrfs setup-default setup-fcvm setup-pjdfstest setup-hugepages bench bench-vm bench-hugepages bench-hugepages-test \
	bench-container-import bench-chromium bench-clone-latency \
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
	@echo "  test-fast          + quick VM tests (rootless, no sudo)"
	@echo "  test-all           + slow VM tests (rootless, no sudo)"
	@echo "  test-root, test    + privileged tests (bridged, pjdfstest, sudo)"
	@echo "  test-fc-mock       Run tests with fc-mock (no KVM required)"
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

# Disk space check - fails if either root or btrfs is too full
# Requires 10GB free on root (for cargo target) and 15GB on btrfs (for VMs)
check-disk:
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
	@if ! PATH="$$HOME/.cargo/bin:$$PATH" cargo nextest --version >/dev/null 2>&1; then \
		echo "==> Installing cargo-nextest..."; \
		case "$$(uname -m)" in aarch64|arm64) NURL=https://get.nexte.st/latest/linux-arm ;; *) NURL=https://get.nexte.st/latest/linux ;; esac; \
		mkdir -p "$$HOME/.cargo/bin"; \
		curl -LsSf "$$NURL" | tar zxf - -C "$$HOME/.cargo/bin" \
			|| PATH="$$HOME/.cargo/bin:$$PATH" cargo install cargo-nextest --locked; \
	fi
	@if [ -d /mnt/fcvm-btrfs ] && ! [ -L target ]; then \
		if [ -d target ]; then \
			echo "==> Moving existing target/ to /mnt/fcvm-btrfs/cargo-target..."; \
			sudo rm -rf /mnt/fcvm-btrfs/cargo-target; \
			mv target /mnt/fcvm-btrfs/cargo-target; \
		else \
			mkdir -p /mnt/fcvm-btrfs/cargo-target; \
		fi; \
		ln -s /mnt/fcvm-btrfs/cargo-target target; \
		echo "==> Symlinked target/ → /mnt/fcvm-btrfs/cargo-target"; \
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

build:
	@echo "==> Building..."
	CARGO_TARGET_DIR=target $(CARGO) build --release -p fcvm
	CARGO_TARGET_DIR=target $(CARGO) build --release -p fc-agent --target $(MUSL_TARGET)
	@mkdir -p target/release && cp target/$(MUSL_TARGET)/release/fc-agent target/release/fc-agent
	@# Sync embedded config to user config dir (config is embedded at compile time)
	@./target/release/fcvm setup --generate-config --force 2>/dev/null || true

build-fc-mock:
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
	sudo rm -rf target/*

# Run-only targets (no setup deps, used by container)
_test-unit:
	$(NEXTEST) --no-default-features

_test-fast:
	RUST_LOG="$(TEST_LOG)" \
	./scripts/no-sudo.sh $(NEXTEST) $(NEXTEST_CAPTURE) --no-default-features --features integration-fast $(FILTER)

_test-all:
	RUST_LOG="$(TEST_LOG)" \
	./scripts/no-sudo.sh $(NEXTEST) $(NEXTEST_CAPTURE) $(FILTER)

_test-root:
	@if find target/ -user root -print -quit 2>/dev/null | grep -q .; then \
		echo "==> WARNING: root-owned files in target/ (from sudo cargo?). Fixing ownership..."; \
		sudo chown -R $$(id -u):$$(id -g) target/; \
	fi
	@RUST_LOG="$(TEST_LOG)" \
	FCVM_DATA_DIR=$(ROOT_DATA_DIR) \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(NEXTEST) $(NEXTEST_CAPTURE) $(NEXTEST_IGNORED) --features privileged-tests $(IPV6_FILTER) $(CI_NESTED_FILTER) $(FILTER) || \
	{ echo ""; \
	  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"; \
	  echo "TEST FAILED - Check debug logs for root cause:"; \
	  echo "  📋 Debug logs: /tmp/fcvm-test-logs/*.log"; \
	  echo "  💡 Re-run with STREAM=1 to see tracing output in real-time"; \
	  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"; \
	  exit 1; }

# Host targets (with setup, check-disk first to fail fast if disk is full)
test-unit: show-notes check-disk build _test-unit
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
_test-fc-mock:
	@FCVM_FIRECRACKER_BIN=/usr/local/bin/fc-mock \
	RUST_LOG="$(TEST_LOG)" \
	FCVM_DATA_DIR=$${FCVM_DATA_DIR:-$(ROOT_DATA_DIR)} \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(NEXTEST) $(NEXTEST_CAPTURE) --profile fc-mock --features privileged-tests -E '$(FC_MOCK_FILTER)' $(FILTER) || \
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
setup-hugepages:
	@current=$$(cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages 2>/dev/null || echo 0); \
	if [ "$$current" -ge "$(HUGEPAGE_POOL_TESTS)" ]; then \
		echo "==> Hugepages already allocated: $$current (need $(HUGEPAGE_POOL_TESTS))"; \
	else \
		echo "==> Allocating hugepage pool ($(HUGEPAGE_POOL_TESTS) pages = $$(( $(HUGEPAGE_POOL_TESTS) * 2 ))MB)..."; \
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
	@echo "==> Running fcvm setup (default kernel)..."
	@# Tests run fcvm via sudo, which reads /root/.config/fcvm — sync BOTH the
	@# user and root configs or a rootfs-config.toml change silently boots the
	@# previous rootfs under sudo (root keeps the stale SHA).
	sudo ./target/release/fcvm setup --generate-config --force
	./target/release/fcvm setup

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
	sudo ./target/release/fcvm setup
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

bench: build
	@echo "==> Running benchmarks..."
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(CARGO) bench -p fuse-pipe --bench throughput
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='$(ROOT_TEST_RUNNER)' \
	$(CARGO) bench -p fuse-pipe --bench operations
	$(CARGO) bench -p fuse-pipe --bench protocol

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
	sudo sh -c 'echo $(HUGEPAGE_POOL_FULL) > /proc/sys/vm/nr_hugepages'
	@echo "==> Running hugepages benchmark (full)..."
	TMPDIR=/mnt/fcvm-btrfs/tmp $(CARGO) bench --bench hugepages; \
	RC=$$?; \
	echo "==> Releasing hugepage pool..."; \
	sudo sh -c 'echo 0 > /proc/sys/vm/nr_hugepages'; \
	exit $$RC

bench-hugepages-test: build setup-default
	@echo "==> Allocating hugepage pool ($(HUGEPAGE_POOL_TEST) pages = $$(( $(HUGEPAGE_POOL_TEST) * 2 ))MB)..."
	sudo sh -c 'echo $(HUGEPAGE_POOL_TEST) > /proc/sys/vm/nr_hugepages'
	@echo "==> Running hugepages benchmark (test)..."
	TMPDIR=/mnt/fcvm-btrfs/tmp $(CARGO) bench --bench hugepages -- --test; \
	RC=$$?; \
	echo "==> Releasing hugepage pool..."; \
	sudo sh -c 'echo 0 > /proc/sys/vm/nr_hugepages'; \
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

_bench:
	@echo "==> Running benchmarks..."
	$(CARGO) bench -p fuse-pipe --bench throughput
	$(CARGO) bench -p fuse-pipe --bench operations
	$(CARGO) bench -p fuse-pipe --bench protocol

# Lint tools versions (keep in sync with CI)
CARGO_AUDIT_VERSION := 0.22.0
CARGO_DENY_VERSION := 0.18.9

setup-lint-tools:
	@which cargo-audit > /dev/null || (echo "Installing cargo-audit..." && cargo install cargo-audit@$(CARGO_AUDIT_VERSION) --locked)
	@which cargo-deny > /dev/null || (echo "Installing cargo-deny..." && cargo install cargo-deny@$(CARGO_DENY_VERSION) --locked)

lint: setup-lint-tools
	$(CARGO) fmt -p fcvm -p fuse-pipe -p fc-agent -p failpoint --check
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) audit
	$(CARGO) deny check

update-dependency:
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

fmt:
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
