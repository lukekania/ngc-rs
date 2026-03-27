#!/usr/bin/env bash
set -euo pipefail

PROJECT_PATH="${1:-.}"
TSCONFIG="${PROJECT_PATH}/tsconfig.json"

echo "=== ngc-rs resolve ==="
hyperfine --warmup 3 "cargo run --release -- info --project ${TSCONFIG}"

echo ""
echo "=== tsc --listFiles ==="
hyperfine --warmup 3 "npx tsc --project ${TSCONFIG} --listFiles --noEmit"
