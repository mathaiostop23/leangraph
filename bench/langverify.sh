#!/usr/bin/env bash
# Verify every language against a real repository, not a fixture.
#
# A fixture proves a spec compiles and finds the two things the fixture
# contains. It does not prove the spec covers how the language is actually
# written, and seven times now the difference has been large: Kotlin at 73.5%,
# Ruby at 62.2%, C++ at 89.3% — all of them passing `langsmoke.sh`.
#
# Clones one corpus per language, builds a CodeGraph index as the oracle, and
# reports presence recall. Needs network and `npx`; everything lands in
# .bench-repos/ and nothing is deleted, so a second run only re-measures.
set -uo pipefail
cd "$(dirname "$0")/.."

REPOS=${LEANGRAPH_BENCH_REPOS:-../.bench-repos}
CG="npx -y @colbymchenry/codegraph@1.5.0"

# name|language|url — one representative repository each, small enough that a
# full run is minutes rather than hours.
CORPORA=(
  "flask|python|https://github.com/pallets/flask"
  "django|python|https://github.com/django/django"
  "excalidraw|typescript|https://github.com/excalidraw/excalidraw"
  "leveldb|c++|https://github.com/google/leveldb"
  "libuv|c|https://github.com/libuv/libuv"
  "newtonsoft|c#|https://github.com/JamesNK/Newtonsoft.Json"
  "monolog|php|https://github.com/Seldaek/monolog"
  "okhttp|kotlin|https://github.com/square/okhttp"
  "alamofire|swift|https://github.com/Alamofire/Alamofire"
  "upickle|scala|https://github.com/com-lihaoyi/upickle"
)

[ -x target/release/leangraph ] || { echo "build first: cargo build --release"; exit 1; }
mkdir -p "$REPOS"

names=()
for entry in "${CORPORA[@]}"; do
  IFS='|' read -r name lang url <<< "$entry"
  names+=("$name")
  if [ ! -d "$REPOS/$name" ]; then
    printf 'cloning %-12s %s\n' "$name" "$lang"
    # Blobless and shallow: we index the working tree, not the history.
    git clone -q --filter=blob:none --depth 1 "$url" "$REPOS/$name" || {
      echo "  clone failed, skipping"; continue; }
  fi
  if [ ! -f "$REPOS/$name/.codegraph/codegraph.db" ]; then
    printf 'oracle   %-12s ' "$name"
    (cd "$REPOS/$name" && $CG init . </dev/null 2>&1 | grep -oE "[0-9.]+ nodes" | tail -1)
  fi
  ./target/release/leangraph index "$REPOS/$name" --force >/dev/null 2>&1
done

exec python3 bench/verify.py "${names[@]}"
