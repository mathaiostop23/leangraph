#!/usr/bin/env bash
# Edge correctness, end to end.
#
# Three labellers, run in order of how much they can be argued with:
#   edgefacts   what is falsifiable from the graph alone — no oracle, all corpora
#   edgetrace   what the code actually does — ground truth, but Python and only
#               where the suite runs offline
#   edges       the calibration table both feed
#
# Every optional stage degrades to a printed SKIP rather than a failure, because
# a benchmark that only runs on one machine is a benchmark nobody runs.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
CORPUS=${ARBOR_BENCH_REPOS:-$ROOT/../.bench-repos}

[ -x target/release/arbor ] || { echo "build first: cargo build --release"; exit 1; }

echo
echo "  arbor    $(git rev-parse --short HEAD 2>/dev/null || echo '?')$([ -n "$(git status --porcelain 2>/dev/null)" ] && echo ' +dirty')"
echo "  python   $(python3 -c 'import sys;print(".".join(map(str,sys.version_info[:3])))')"

echo
echo "──────────  falsifiable without an oracle  ──────────"
python3 bench/edgefacts.py "$@"

TRACE=""
FLASK="$CORPUS/flask"
if [ -d "$FLASK/src/flask" ] && python3 -c 'import pytest' 2>/dev/null; then
  TRACE=$(mktemp -t arbor-trace).jsonl
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
  rm -f "$TRACE"
else
  python3 bench/edges.py flask
fi
