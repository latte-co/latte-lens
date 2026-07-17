#!/usr/bin/env bash
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

cargo_command=${CARGO:-cargo}
python_command=${PYTHON:-python3}
binary=${BINARY:-latte-lens}
minimum=${E2E_COVERAGE_MIN:-85}
ignore_regex=${E2E_COVERAGE_IGNORE_REGEX:-'(/agent/|/bin/agent_observability_harness\.rs$|/(clipboard|content_safety|diff|git|preview|repo_graph|runtime|search|text_layout|tree)\.rs$)'}
target_dir=${E2E_COVERAGE_TARGET_DIR:-target/llvm-cov-e2e}

case "$target_dir" in
  /*) ;;
  *) target_dir="$repo_root/$target_dir" ;;
esac
export CARGO_TARGET_DIR=$target_dir

env_file=$(mktemp "${TMPDIR:-/tmp}/latte-lens-coverage-env.XXXXXX")
trap 'rm -f "$env_file"' EXIT

"$cargo_command" llvm-cov clean --workspace
"$cargo_command" llvm-cov show-env --sh >"$env_file"
# cargo-llvm-cov owns this generated environment and quotes every exported value.
# shellcheck disable=SC1090
source "$env_file"

"$cargo_command" build --locked
"$python_command" scripts/e2e_tui.py \
  "$CARGO_TARGET_DIR/debug/$binary" \
  --scenario all \
  --artifact-dir "$CARGO_TARGET_DIR/e2e-artifacts"
# Exercise every final-binary CLI branch as part of the process-level profile.
# The live Hook case accepts an idempotent metadata fallback when coverage
# instrumentation consumes the fixed 5 ms ACK budget.
"$cargo_command" test --all-features --locked --test cli_e2e
"$cargo_command" build --locked --features agent-observability-harness \
  --bin latte-lens-agent-harness
"$python_command" scripts/agent_e2e_tui.py \
  "$CARGO_TARGET_DIR/debug/latte-lens-agent-harness" \
  --artifact-dir "$CARGO_TARGET_DIR/e2e-artifacts/agent"
"$cargo_command" llvm-cov report \
  --ignore-filename-regex "$ignore_regex" \
  --fail-under-lines "$minimum"
