#!/usr/bin/env bash
set -euo pipefail

# Run all E2E Docker test suites.
# Usage: ./run-all.sh [suite-name]
#   Run a single suite: ./run-all.sh rproxy
#   Run all suites:     ./run-all.sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PASSED=0
FAILED=0
FAILURES=()

run_suite() {
    local suite="$1"
    local dir="${SCRIPT_DIR}/${suite}"

    if [[ ! -f "${dir}/docker-compose.yml" ]]; then
        return
    fi

    printf '\033[1;34m[TEST]\033[0m Running suite: %s\n' "$suite"

    cd "$dir"
    if docker compose up --build --abort-on-container-exit --exit-code-from test-runner 2>&1; then
        printf '\033[1;32m[PASS]\033[0m %s\n' "$suite"
        PASSED=$((PASSED + 1))
    else
        printf '\033[1;31m[FAIL]\033[0m %s\n' "$suite"
        FAILED=$((FAILED + 1))
        FAILURES+=("$suite")
    fi
    docker compose down -v --remove-orphans 2>/dev/null || true
    cd "$SCRIPT_DIR"
}

if [[ $# -gt 0 ]]; then
    run_suite "$1"
else
    for dir in "$SCRIPT_DIR"/*/; do
        suite="$(basename "$dir")"
        run_suite "$suite"
    done
fi

echo ""
echo "==============================="
echo "Results: ${PASSED} passed, ${FAILED} failed"
if [[ ${FAILED} -gt 0 ]]; then
    echo "Failed suites: ${FAILURES[*]}"
    exit 1
fi
