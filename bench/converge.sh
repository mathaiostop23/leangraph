#!/usr/bin/env bash
# Invariant: an incremental sync must produce a graph *semantically identical*
# to a full reindex of the same working tree.
#
# This is the property that makes incremental safe. A stale-but-plausible graph
# is worse than a slow one: it answers confidently and wrongly, and nothing
# downstream can tell.
#
# The comparison is on content-derived node keys, not raw bytes. Node ids are an
# allocation detail — an incremental sync deliberately keeps the ids it had,
# where a full index packs them from zero — so byte equality would report a
# difference that means nothing. Every node identity, its location, and every
# edge between identities must match exactly.
#
# Usage: bench/converge.sh [repo]
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARBOR="$ROOT/target/release/arbor"
REPO="${1:-$ROOT/../.bench-repos/django}"
REPO="$(cd "$REPO" && pwd)"
GRAPH="$REPO/.arbor/graph.bin"

[[ -x "$ARBOR" ]] || { echo "build first: cargo build --release" >&2; exit 1; }

hash_graph() { "$ARBOR" dump -p "$REPO" --what semantic | shasum -a 256 | cut -c1-16; }
full()  { "$ARBOR" index "$REPO" --force >/dev/null 2>&1; }
sync()  { "$ARBOR" index "$REPO"         >/dev/null 2>&1; }

pass=0; fail=0
check() {
  if [[ "$2" == "$3" ]]; then
    printf '  \033[32m✓\033[0m %-34s %s\n' "$1" "$2"; pass=$((pass+1))
  else
    printf '  \033[31m✗\033[0m %-34s %s != %s\n' "$1" "$2" "$3"; fail=$((fail+1))
  fi
}

echo
echo "convergence — $(basename "$REPO")"
echo

full; A=$(hash_graph)
full; B=$(hash_graph)
check "full index is reproducible" "$A" "$B"

# --- modify then revert -------------------------------------------------------
VICTIM=$(cd "$REPO" && git ls-files '*.py' '*.ts' | head -1)
printf '\n# arbor convergence probe\n' >> "$REPO/$VICTIM"
sync
(cd "$REPO" && git checkout -- "$VICTIM")
sync; C=$(hash_graph)
full; D=$(hash_graph)
check "modify + revert converges" "$C" "$D"

# --- add a file ---------------------------------------------------------------
NEW="$REPO/zz_arbor_probe.py"
printf 'class ArborProbe:\n    def ping(self):\n        return 1\n' > "$NEW"
sync; E=$(hash_graph)
full; F=$(hash_graph)
check "added file converges" "$E" "$F"

# --- delete it ----------------------------------------------------------------
rm -f "$NEW"
sync; G=$(hash_graph)
full; H=$(hash_graph)
check "deleted file converges" "$G" "$H"

check "add+delete restores original" "$G" "$A"

echo
if [[ $fail -eq 0 ]]; then
  printf '  \033[1m%d/%d invariants hold\033[0m\n\n' "$pass" "$((pass+fail))"
else
  printf '  \033[31m%d of %d FAILED\033[0m — incremental cannot be trusted\n\n' \
    "$fail" "$((pass+fail))"
  exit 1
fi
