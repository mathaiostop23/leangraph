#!/usr/bin/env bash
# Reproducible Phase 0 harness: arbor parse+extract vs CodeGraph full index.
#
# Usage: bench/run.sh [repo ...]     (default: flask excalidraw django)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPOS_DIR="${ARBOR_BENCH_REPOS:-$ROOT/../.bench-repos}"
ARBOR="$ROOT/target/release/arbor"
CG="npx -y @colbymchenry/codegraph@1.5.0"
RUNS="${ARBOR_BENCH_RUNS:-3}"
export CODEGRAPH_TELEMETRY=0

# Associative arrays need bash 4; macOS ships 3.2. Keep it portable.
clone_url() {
  case "$1" in
    flask)      echo https://github.com/pallets/flask ;;
    excalidraw) echo https://github.com/excalidraw/excalidraw ;;
    django)     echo https://github.com/django/django ;;
    *)          echo "" ;;
  esac
}

if [[ $# -gt 0 ]]; then TARGETS=("$@"); else TARGETS=(flask excalidraw django); fi

[[ -x "$ARBOR" ]] || { echo "build first: cargo build --release" >&2; exit 1; }
mkdir -p "$REPOS_DIR"

# --- median of N wall-clock seconds for a command -----------------------------
median_wall() {
  local n=$1; shift
  local -a t=()
  for ((i = 0; i < n; i++)); do
    t+=("$( { /usr/bin/time -p bash -c "$*" >/dev/null; } 2>&1 \
            | awk '/^real/{gsub(",",".",$2); print $2}' )")
  done
  printf '%s\n' "${t[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'
}

# --- measure the CLI's fixed process-startup cost, so we can subtract it ------
echo "measuring codegraph process startup ..."
STARTUP=$(median_wall 3 "$CG --version")
echo "  startup overhead: ${STARTUP}s (subtracted from codegraph totals below)"
echo

printf '%-12s %8s %9s %12s %14s %8s %9s\n' \
  REPO FILES MB ARBOR CODEGRAPH SHARE HEADROOM
printf '%.0s-' {1..78}; echo

for r in "${TARGETS[@]}"; do
  path="$REPOS_DIR/$r"
  if [[ ! -d "$path" ]]; then
    url=$(clone_url "$r")
    [[ -n "$url" ]] || { echo "$r: unknown repo, clone it into $REPOS_DIR" >&2; continue; }
    git clone --depth 1 --quiet "$url" "$path"
  fi

  # arbor: parse + extract only (no resolution, no persistence)
  a=$(median_wall "$RUNS" "$ARBOR '$path'")
  read -r files mb < <("$ARBOR" "$path" 2>/dev/null \
    | sed 's/\x1b\[[0-9;]*m//g' \
    | awk '/^  files/{gsub(/[(),]/,""); print $2, $3}')

  # codegraph: full index (extract + resolve + persist)
  [[ -d "$path/.codegraph" ]] || $CG init "$path" </dev/null >/dev/null 2>&1
  c=$(median_wall "$RUNS" "$CG index '$path' --force </dev/null")
  c=$(awk -v c="$c" -v s="$STARTUP" 'BEGIN{printf "%.2f", (c-s > 0 ? c-s : c)}')

  awk -v r="$r" -v f="$files" -v mb="$mb" -v a="$a" -v c="$c" 'BEGIN{
    printf "%-12s %8s %9s %10.0f ms %12.2f s %7.1f%% %8.0fx\n",
           r, f, mb, a*1000, c, 100*a/c, c/a
  }'
done

cat <<'EOF'

ARBOR      = parse + extract only (mmap, blake3, tree-sitter, cursor walk)
CODEGRAPH  = full index: extract + resolve + persist, minus process startup
SHARE      = what fraction of CodeGraph's pipeline parse+extract accounts for
HEADROOM   = time budget available to us for resolution + persistence
EOF
