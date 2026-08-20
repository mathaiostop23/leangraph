#!/usr/bin/env bash
# Every language must still extract and resolve.
#
# A fixture rather than a corpus, and that limit is real — this proves a spec
# works on ordinary code, not that it works on Django. But it is the difference
# between "the spec compiles" and "the spec finds anything", and the compile
# tells you nothing: `kinds()` drops a node name the grammar does not know, so a
# broken spec produces an indexer that runs happily and extracts nothing.
#
# Each case is one file with a type, two methods, and one method calling the
# other. If a language stops producing that edge, its spec has drifted.
set -uo pipefail
cd "$(dirname "$0")/.."
BIN="$(pwd)/target/release/leangraph"
[ -x "$BIN" ] || { echo "build first: cargo build --release" >&2; exit 1; }

DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT

cat > "$DIR/a.go" <<'EOF'
package main
type Shape struct{ W int }
func (s Shape) Area() int { return s.side() }
func (s Shape) side() int { return s.W }
EOF
cat > "$DIR/B.java" <<'EOF'
package demo;
public class Box {
    public int getSize() { return 1; }
    public int compareTo(Box o) { return getSize(); }
}
EOF
cat > "$DIR/c.rb" <<'EOF'
class Person
  def greet; hello; end
  def hello; "hi"; end
end
EOF
cat > "$DIR/d.php" <<'EOF'
<?php
class User {
    public function name() { return "n"; }
    public function shout() { return $this->name(); }
}
EOF
cat > "$DIR/e.kt" <<'EOF'
class Counter(var n: Int) {
    fun bump(): Int { return step() }
    fun step(): Int = n + 1
}
EOF
cat > "$DIR/f.swift" <<'EOF'
class Engine {
    func start() -> Int { return warm() }
    func warm() -> Int { return 1 }
}
EOF
cat > "$DIR/g.scala" <<'EOF'
class Adder(val base: Int) {
  def add(x: Int): Int = combine(x)
  def combine(x: Int): Int = base + x
}
EOF
cat > "$DIR/h.cs" <<'EOF'
public class Store {
    public int Count() { return Total(); }
    public int Total() { return 1; }
}
EOF
cat > "$DIR/i.c" <<'EOF'
int square(int v) { return v * v; }
int run(void) { return square(4); }
EOF
cat > "$DIR/j.cpp" <<'EOF'
class Widget {
public:
    int draw() { return measure(); }
    int measure() { return 7; }
};
EOF
cat > "$DIR/k.rs" <<'EOF'
pub struct Engine { n: u32 }
impl Engine {
    pub fn start(&self) -> u32 { self.warm() }
    pub fn warm(&self) -> u32 { self.n }
}
EOF
cat > "$DIR/l.py" <<'EOF'
class Loader:
    def load(self): return self.parse()
    def parse(self): return 1
EOF
cat > "$DIR/m.ts" <<'EOF'
export class Client {
    send(): number { return this.encode(); }
    encode(): number { return 1; }
}
EOF

"$BIN" index "$DIR" >/dev/null 2>&1

# caller -> callee that each language must resolve
CASES=$(cat <<'CASES_END'
a.go:Area:side
B.java:compareTo:getSize
c.rb:greet:hello
d.php:shout:name
e.kt:bump:step
f.swift:start:warm
g.scala:add:combine
h.cs:Count:Total
i.c:run:square
j.cpp:draw:measure
k.rs:start:warm
l.py:load:parse
m.ts:send:encode
CASES_END
)

"$BIN" dump -p "$DIR" --what edges > "$DIR/edges.jsonl"

# Parsed rather than grepped: the dump's field order is not a contract, and a
# regex over it fails silently the day it changes — which is the same class of
# bug this script exists to catch.
python3 - "$DIR/edges.jsonl" <<'PYEOF'
import json, sys

CASES = [
    ("a.go",    "go",    "Area",      "side"),
    ("B.java",  "java",  "compareTo", "getSize"),
    ("c.rb",    "ruby",  "greet",     "hello"),
    ("d.php",   "php",   "shout",     "name"),
    ("e.kt",    "kotlin","bump",      "step"),
    ("f.swift", "swift", "start",     "warm"),
    ("g.scala", "scala", "add",       "combine"),
    ("h.cs",    "c#",    "Count",     "Total"),
    ("i.c",     "c",     "run",       "square"),
    ("j.cpp",   "c++",   "draw",      "measure"),
    ("k.rs",    "rust",  "start",     "warm"),
    ("l.py",    "python","load",      "parse"),
    ("m.ts",    "ts",    "send",      "encode"),
]

edges = [json.loads(l) for l in open(sys.argv[1])]
GREEN, RED, OFF = "\033[32m", "\033[31m", "\033[0m"
print(f"\n  {'language':<10} {'edge':<24} {'kind':<11} confidence")
print("  " + "-" * 46)
bad = 0
for f, lang, src, dst in CASES:
    # Any semantic edge counts, and the kind is printed rather than assumed.
    # Ruby's idiomatic call has no parentheses, so `hello` parses as a bare
    # identifier — indistinguishable from a local variable — and lands as a
    # reference. It reaches the right target at confidence 100, which is what
    # the graph needs; calling it a `calls` edge would be a claim the syntax
    # does not support.
    hit = next(
        (e for e in edges
         if e["ek"] in ("calls", "references") and e["sf"] == f
         and e["sq"].split(".")[-1] == src
         and e["dq"].split(".")[-1] == dst),
        None,
    )
    if hit:
        print(f"  {GREEN}OK{OFF} {lang:<8} {src} -> {dst:<14} {hit['ek']:<11} conf={hit['conf']}")
    else:
        bad += 1
        print(f"  {RED}!!{OFF} {lang:<8} {src} -> {dst:<14} NOT RESOLVED")
print(f"\n  {len(CASES) - bad}/{len(CASES)} languages connect the two methods\n")
sys.exit(1 if bad else 0)
PYEOF
