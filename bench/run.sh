#!/usr/bin/env bash
# Differential benchmark: arbor vs CodeGraph, same repos, same machine.
#
# Compares both wall clock AND graph size, because a speed number without a
# graph-size number next to it is meaningless — anyone can be fast by
# extracting less.
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

strip_ansi() { sed 's/\x1b\[[0-9;]*m//g'; }

median_wall() {
  local n=$1; shift
  local -a t=()
  for ((i = 0; i < n; i++)); do
    t+=("$( { /usr/bin/time -p bash -c "$*" >/dev/null; } 2>&1 \
            | awk '/^real/{gsub(",",".",$2); print $2}' )")
  done
  printf '%s\n' "${t[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'
}

echo "measuring codegraph process startup ..."
STARTUP=$(median_wall 3 "$CG --version")
echo "  startup overhead: ${STARTUP}s (subtracted from codegraph totals below)"
echo

printf '%-12s %7s %19s %19s %8s\n' "" "" "--------- arbor ---------" "------- codegraph -------" ""
printf '%-12s %7s %9s %9s %9s %9s %8s\n' REPO FILES TIME NODES TIME NODES SPEEDUP
printf '%.0s-' {1..70}; echo

for r in "${TARGETS[@]}"; do
  path="$REPOS_DIR/$r"
  if [[ ! -d "$path" ]]; then
    url=$(clone_url "$r")
    [[ -n "$url" ]] || { echo "$r: unknown repo, clone it into $REPOS_DIR" >&2; continue; }
    git clone --depth 1 --quiet "$url" "$path"
  fi

  a=$(median_wall "$RUNS" "$ARBOR index '$path'")
  out=$("$ARBOR" index "$path" 2>/dev/null | strip_ansi)
  files=$(awk '/^  files/{print $2}' <<<"$out")
  a_nodes=$(awk '/^  graph/{print $2}' <<<"$out")
  a_edges=$(awk '/^  graph/{print $5}' <<<"$out")
  a_res=$(awk '/^  resolution/{print $2}' <<<"$out")

  [[ -d "$path/.codegraph" ]] || $CG init "$path" </dev/null >/dev/null 2>&1
  c=$(median_wall "$RUNS" "$CG index '$path' --force </dev/null")
  c=$(awk -v c="$c" -v s="$STARTUP" 'BEGIN{printf "%.2f", (c-s > 0 ? c-s : c)}')
  cg_out=$($CG index "$path" --force </dev/null 2>&1 | strip_ansi | tr -d '.')
  c_nodes=$(awk '/nodes,/{for(i=1;i<=NF;i++) if($(i+1)=="nodes,") print $i}' <<<"$cg_out" | head -1)
  c_edges=$(awk '/edges/{for(i=1;i<=NF;i++) if($(i+1)=="edges") print $i}' <<<"$cg_out" | head -1)

  awk -v r="$r" -v f="$files" -v a="$a" -v an="$a_nodes" -v ae="$a_edges" -v ar="$a_res" \
      -v c="$c" -v cn="$c_nodes" -v ce="$c_edges" 'BEGIN{
    printf "%-12s %7s %7.0f ms %9s %7.2f s %9s %7.0fx\n", r, f, a*1000, an, c, cn, c/a
    printf "%-12s %7s %19s %19s\n", "", "", ae " edges", ce " edges"
    printf "%-12s %7s %19s\n", "", "", ar " resolved"
  }'
done

cat <<'EOF'

arbor      discover + extract + resolve.  NOT YET PERSISTED — Phase 2 adds CSR
           write, so this number will grow. Treat it as a floor, not a product
           number.
codegraph  full index: extract + resolve + persist, minus process startup.
resolved   share of references whose target exists in the repo that we linked.
           Excludes language builtins and third-party deps, which have no
           in-repo target and cannot be resolved by anyone.
EOF
