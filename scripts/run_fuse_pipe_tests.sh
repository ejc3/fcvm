#!/usr/bin/env bash
#
# Sequential fuse-pipe test runner with per-step logging and timeouts.
# Intended to run on the EC2 host where libfuse, pjdfstest, etc. are available.
#
# Usage:
#   cd /home/ubuntu/fcvm
#   ./scripts/run_fuse_pipe_tests.sh
#
# Environment variables:
#   LOG_DIR        - directory for logs (default: /tmp/fuse-pipe-tests)
#   STEP_TIMEOUT   - per-step timeout seconds (default: 1800)
#   STRESS_OPS     - overrides default ops per worker for stress test
#   STRESS_WORKERS - overrides stress worker count
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}/fuse-pipe"

LOG_DIR="${LOG_DIR:-/tmp/fuse-pipe-tests}"
mkdir -p "${LOG_DIR}"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
LOG_FILE="${LOG_DIR}/run-${TIMESTAMP}.log"

STEP_TIMEOUT="${STEP_TIMEOUT:-1800}"
TIMEOUT_BIN="$(command -v timeout || true)"

echo "==> Logs: ${LOG_FILE}"
echo "==> Starting fuse-pipe test sweep at ${TIMESTAMP}" | tee -a "${LOG_FILE}"

run_step() {
    local name="$1"
    shift
    echo -e "\n==> [${name}] $*" | tee -a "${LOG_FILE}"
    if [[ -n "${TIMEOUT_BIN}" ]]; then
        "${TIMEOUT_BIN}" --foreground "${STEP_TIMEOUT}" "$@" 2>&1 | tee -a "${LOG_FILE}"
    else
        "$@" 2>&1 | tee -a "${LOG_FILE}"
    fi
}

die() {
    echo "!! $*" | tee -a "${LOG_FILE}"
    exit 1
}

run_step "unit+lib" cargo test --lib -- --nocapture || die "unit/lib tests failed"
run_step "integration" cargo test --test integration -- --nocapture || die "integration tests failed"

if [[ $EUID -ne 0 ]]; then
    echo "==> Re-running remaining tests with sudo for full coverage" | tee -a "${LOG_FILE}"
fi

run_step "stress" sudo env STRESS_WORKERS="${STRESS_WORKERS:-4}" STRESS_OPS="${STRESS_OPS:-1000}" \
    cargo test --test stress -- --nocapture || die "stress test failed"

run_step "pjdfstest-fast" sudo cargo test --test pjdfstest_fast -- --nocapture || die "pjdfstest_fast failed"
run_step "pjdfstest-full" sudo cargo test --test pjdfstest_full -- --nocapture || die "pjdfstest_full failed"

echo -e "\n==> ALL TESTS PASSED" | tee -a "${LOG_FILE}"
