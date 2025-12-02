# fuse-pipe Makefile
# See TESTING.md for complete documentation

# === Configuration ===
EC2_HOST ?= ubuntu@54.67.60.104
EC2_KEY ?= ~/.ssh/fcvm-ec2
SSH = ssh -i $(EC2_KEY) $(EC2_HOST)

# === Build ===
.PHONY: build build-release build-stress

build:
	cargo build

build-release:
	cargo build --release

build-stress:
	cargo build --release --test stress

# === Testing ===
.PHONY: test test-lib test-integration test-stress test-pjdfs test-pjdfs-full test-all

test:
	cargo test

test-lib:
	cargo test --lib

test-integration:
	cargo test --test integration

test-stress: build-stress
	cargo build --release --test stress
	@echo "Run with: sudo ./target/release/deps/stress-* --workers 4 --ops 1000 --readers 256"

test-pjdfs:
	cargo test --test pjdfstest_fast -- --nocapture

test-pjdfs-full:
	cargo test --test pjdfstest_full --features pjdfstest-full -- --nocapture

# Run all tests in order (simplest to hardest)
test-all: test test-integration
	@echo "All tests passed!"

# === Benchmarks (Local) ===
.PHONY: bench bench-throughput bench-ops bench-trace bench-quick

bench: bench-throughput bench-ops

bench-throughput: build-stress
	cargo bench --bench throughput

bench-ops: build-stress
	cargo bench --bench operations

bench-trace: build-stress
	cargo bench --bench throughput --features trace-benchmarks
	cargo bench --bench operations --features trace-benchmarks

bench-quick: build-stress
	cargo bench --bench throughput -- --quick
	cargo bench --bench operations -- --quick

# === Benchmarks (Remote EC2) ===
.PHONY: bench-remote bench-remote-trace

bench-remote:
	$(SSH) "cd ~/fcvm/fuse-pipe && cargo build --release --test stress && cargo bench --bench throughput -- --quick 2>&1 | tee /tmp/bench.log"

bench-remote-trace:
	$(SSH) "cd ~/fcvm/fuse-pipe && cargo build --release --test stress && cargo bench --bench throughput --features trace-benchmarks -- --quick 2>&1 | tee /tmp/bench-trace.log"

# === Log Viewing ===
.PHONY: bench-logs bench-logs-remote

bench-logs:
	@echo "=== Recent benchmark logs ==="
	@ls -lt /tmp/fuse-bench-*.log 2>/dev/null | head -5 || echo "No local logs found"
	@echo ""
	@echo "=== Latest telemetry ==="
	@cat $$(ls -t /tmp/fuse-bench-telemetry-*.json 2>/dev/null | head -1) 2>/dev/null | jq . || echo "No telemetry found"

bench-logs-remote:
	$(SSH) "tail -100 /tmp/bench.log 2>/dev/null || echo 'No remote logs'"

# === Cleanup ===
.PHONY: clean bench-clean bench-clean-all

clean:
	cargo clean

bench-clean:
	rm -rf target/criterion
	rm -f /tmp/fuse-bench-*.log
	rm -f /tmp/fuse-bench-telemetry-*.json
	rm -f /tmp/fuse-stress*.sock
	rm -f /tmp/fuse-ops-bench-*.sock

bench-clean-all: bench-clean
	$(SSH) "rm -f /tmp/bench*.log /tmp/fuse-bench-*.json" 2>/dev/null || true

# === Sync to EC2 ===
.PHONY: sync

sync:
	rsync -avz --exclude 'target' --exclude '.git' \
		-e "ssh -i $(EC2_KEY)" \
		../ $(EC2_HOST):~/fcvm/

# === Linting ===
.PHONY: lint clippy fmt fmt-check

lint: clippy fmt-check

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

# === Help ===
.PHONY: help

help:
	@echo "fuse-pipe Makefile"
	@echo ""
	@echo "Build:"
	@echo "  make build          Build debug"
	@echo "  make build-release  Build release"
	@echo "  make build-stress   Build stress test binary"
	@echo ""
	@echo "Test:"
	@echo "  make test           Run all tests"
	@echo "  make test-lib       Run library tests only"
	@echo "  make test-integration Run integration tests"
	@echo "  make test-stress    Build stress test (run manually)"
	@echo "  make test-pjdfs     Run pjdfstest (fast)"
	@echo "  make test-all       Run all tests in sequence"
	@echo ""
	@echo "Benchmark:"
	@echo "  make bench          Run all benchmarks"
	@echo "  make bench-quick    Run quick benchmarks"
	@echo "  make bench-trace    Run benchmarks with tracing"
	@echo "  make bench-remote   Run benchmarks on EC2"
	@echo ""
	@echo "Logs:"
	@echo "  make bench-logs     View local benchmark logs"
	@echo "  make bench-logs-remote View EC2 benchmark logs"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean          Clean build artifacts"
	@echo "  make bench-clean    Clean benchmark logs/telemetry"
	@echo ""
	@echo "Other:"
	@echo "  make sync           Sync to EC2"
	@echo "  make lint           Run clippy and fmt check"
	@echo "  make help           Show this help"
