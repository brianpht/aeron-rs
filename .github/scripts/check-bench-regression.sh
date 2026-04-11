#!/usr/bin/env bash
# check-bench-regression.sh - Parse critcmp JSON export and fail if any
# benchmark regresses beyond a given threshold.
#
# Usage: check-bench-regression.sh <comparison.json> <threshold_percent>
#
# Exit 0: all benchmarks within threshold.
# Exit 1: at least one benchmark regressed beyond threshold.
#
# critcmp --export JSON structure (per benchmark):
#   { "<bench_name>": { "<baseline>": { "Mean": { "point_estimate": <ns>, ... } } } }
#
# We compare "master" vs "current" baselines. A regression means the "current"
# mean is more than threshold% higher than the "master" mean (for latency
# benchmarks, higher is worse).

set -euo pipefail

COMPARISON_FILE="${1:?Usage: $0 <comparison.json> <threshold_percent>}"
THRESHOLD="${2:?Usage: $0 <comparison.json> <threshold_percent>}"

if [ ! -f "$COMPARISON_FILE" ]; then
  echo "ERROR: comparison file not found: $COMPARISON_FILE"
  exit 1
fi

# Check if jq is available (installed by default on GitHub Actions ubuntu-latest)
if ! command -v jq &>/dev/null; then
  echo "ERROR: jq is required but not installed"
  exit 1
fi

echo "Regression threshold: ${THRESHOLD}%"
echo "---"

FAILED=0
CHECKED=0
REGRESSED_LIST=""

# Iterate over each benchmark in the JSON
for bench_name in $(jq -r 'keys[]' "$COMPARISON_FILE"); do
  master_mean=$(jq -r --arg b "$bench_name" '.[$b].master.Mean.point_estimate // empty' "$COMPARISON_FILE" 2>/dev/null)
  current_mean=$(jq -r --arg b "$bench_name" '.[$b].current.Mean.point_estimate // empty' "$COMPARISON_FILE" 2>/dev/null)

  # Skip if either baseline is missing
  if [ -z "$master_mean" ] || [ -z "$current_mean" ]; then
    echo "SKIP: $bench_name (missing baseline data)"
    continue
  fi

  CHECKED=$((CHECKED + 1))

  # Calculate percentage change using awk (avoids bash floating point issues)
  result=$(awk -v master="$master_mean" -v current="$current_mean" -v thresh="$THRESHOLD" '
    BEGIN {
      if (master <= 0) {
        print "SKIP 0 0"
        exit
      }
      pct_change = ((current - master) / master) * 100
      regressed = (pct_change > thresh) ? 1 : 0
      printf "%s %.2f %.2f\n", (regressed ? "FAIL" : "PASS"), pct_change, current/master
    }
  ')

  status=$(echo "$result" | awk '{print $1}')
  pct=$(echo "$result" | awk '{print $2}')
  ratio=$(echo "$result" | awk '{print $3}')

  if [ "$status" = "SKIP" ]; then
    echo "SKIP: $bench_name (master mean <= 0)"
    continue
  fi

  if [ "$status" = "FAIL" ]; then
    echo "FAIL: $bench_name  +${pct}% (${ratio}x)  master=$(printf '%.0f' "$master_mean")ns  current=$(printf '%.0f' "$current_mean")ns"
    FAILED=$((FAILED + 1))
    REGRESSED_LIST="${REGRESSED_LIST}\n  - ${bench_name}: +${pct}%"
  else
    echo "PASS: $bench_name  ${pct}% (${ratio}x)"
  fi
done

echo "---"
echo "Checked: $CHECKED benchmarks"

if [ "$FAILED" -gt 0 ]; then
  echo ""
  echo "ERROR: $FAILED benchmark(s) regressed beyond ${THRESHOLD}% threshold:"
  echo -e "$REGRESSED_LIST"
  echo ""
  echo "To investigate locally:"
  echo "  cargo bench --bench <name> -- --save-baseline current"
  echo "  critcmp master current"
  exit 1
fi

echo "All benchmarks within ${THRESHOLD}% threshold."
exit 0

