#!/usr/bin/env bash
# Edge correctness, end to end.
#
# Three labellers, run in order of how much they can be argued with:
#   edgefacts   what is falsifiable from the graph alone — no oracle, all corpora
#   edgetrace   what the code actually does — ground truth, but Python and only
#               where the suite runs offline
#   edges       the calibration table both feed
#   edgeprecision  precision itself, on the call sites the tracer settled
#
# Every optional stage degrades to a printed SKIP rather than a failure, because
# a benchmark that only runs on one machine is a benchmark nobody runs.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
CORPUS=${LEANGRAPH_BENCH_REPOS:-$ROOT/../.bench-repos}

[ -x target/release/leangraph ] || { echo "build first: cargo build --release"; exit 1; }

echo
echo "  leangraph    $(git rev-parse --short HEAD 2>/dev/null || echo '?')$([ -n "$(git status --porcelain 2>/dev/null)" ] && echo ' +dirty')"
echo "  python   $(python3 -c 'import sys;print(".".join(map(str,sys.version_info[:3])))')"

echo
echo "──────────  falsifiable without an oracle  ──────────"
python3 bench/edgefacts.py "$@"

TRACE=""
FLASK="$CORPUS/flask"
if [ -d "$FLASK/src/flask" ] && python3 -c 'import pytest' 2>/dev/null; then
  # Stale bytecode names the checkout it was compiled in, which silently
  # removes most of the trace after a move. Cheap to prevent, invisible to
  # diagnose.
  find "$FLASK" -name __pycache__ -type d -prune -exec rm -rf {} + 2>/dev/null
  find "$FLASK" -name .pytest_cache -type d -prune -exec rm -rf {} + 2>/dev/null
  TRACE=$(mktemp -t leangraph-trace).jsonl
  echo
  echo "──────────  running flask's own test suite under a tracer  ──────────"
  ( cd "$FLASK" && PYTHONPATH=src python3 "$ROOT/bench/edgetrace.py" \
      --root "$FLASK" --out "$TRACE" --require flask -- -m pytest tests -q >/dev/null ) \
    || { echo "  SKIP runtime oracle: the suite did not run"; TRACE=""; }
else
  echo
  echo "  SKIP runtime oracle: need pytest and $CORPUS/flask"
fi

echo
echo "──────────  calibration  ──────────"
if [ -n "$TRACE" ]; then
  python3 bench/edges.py flask --trace "$TRACE"
  # The one place precision is settled rather than bounded: where the tracer
  # saw which target a call actually reached, every other candidate we emitted
  # for it is wrong, and that is a measurement rather than an estimate.
  echo
  echo "──────────  precision, where it can be settled  ──────────"
  python3 bench/edgeprecision.py flask --trace "$TRACE"
  rm -f "$TRACE"
else
  python3 bench/edges.py flask
fi
