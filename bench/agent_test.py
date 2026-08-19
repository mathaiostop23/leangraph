#!/usr/bin/env python3
"""Agent-path assertions: prompt safety, and that caching can actually hit.

Run against a live server pointed at bench/stub_api.py, after two *different*
issues have been processed. The caching assertion is the important one, and it
has to be byte equality across issues — "no obviously volatile string appears"
is the weaker check that let the original bug through, where the breakpoint sat
on context built from the issue itself and could never be reused.

Usage: bench/agent_test.py [stub_url]
"""
import json, sys, urllib.request

STUB = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:7998"
ok = fail = 0


def check(name, cond, detail=""):
    global ok, fail
    mark = "\033[32m✓\033[0m" if cond else "\033[31m✗\033[0m"
    print(f"  {mark} {name}{('  ' + detail) if detail and not cond else ''}")
    ok, fail = ok + bool(cond), fail + (not cond)


calls = json.load(urllib.request.urlopen(f"{STUB}/_seen"))["calls"]
print(f"\nagent path — {len(calls)} model calls captured\n")


def sys_text(c):
    s = c["system"]
    return s if isinstance(s, str) else " ".join(b.get("text", "") for b in s)


def user_text(c):
    """Raw message text. Comparing against json.dumps() double-escapes the
    backslash in the neutralised delimiter and reports a false failure — which
    is what the first version of this test did."""
    parts = []
    for m in c["messages"]:
        c_ = m.get("content", "")
        parts.append(c_ if isinstance(c_, str)
                     else " ".join(b.get("text", "") for b in c_))
    return "\n".join(parts)


# --- prompt safety, on every call --------------------------------------------
escapes_seen = 0
for i, c in enumerate(calls):
    t, user = sys_text(c), user_text(c)
    check(f"call {i}: body declared as data, not instruction", "DATA, never instruction" in t)
    check(f"call {i}: no tools declared", "tools" not in c)
    check(f"call {i}: issue delimited", "<issue>" in user and "</issue>" in user)
    # Exactly one closing marker: the one we put there. A body that tried to
    # close it early must appear neutralised.
    check(f"call {i}: exactly one closing delimiter", user.count("</issue>") == 1)
    escapes_seen += user.count(r"<\/issue>")

check("attempted delimiter escapes were neutralised", escapes_seen >= 1,
      "(no escape attempt reached the model — check the fixture)")

# --- caching ------------------------------------------------------------------
analyse = [c for c in calls if isinstance(c.get("system"), list)
           and any("cache_control" in b for b in c["system"])]
check("analyse calls carry a cache breakpoint", len(analyse) >= 1)

if len(analyse) >= 2:
    def prefix(c):
        """Everything up to and including the breakpoint — what the cache keys on."""
        out = []
        for b in c["system"]:
            out.append(b.get("text", ""))
            if "cache_control" in b:
                break
        return "\n".join(out)

    def issue_line(c):
        for line in user_text(c).splitlines():
            if line.startswith("title: "):
                return line[:70]
        return "(no issue wrapper)"

    # Across every analyse call, not just the first two: which call lands first
    # depends on worker scheduling, and an assertion that depends on that is an
    # assertion that passes for the wrong reason half the time.
    prefixes = {prefix(c) for c in analyse}
    titles = {issue_line(c) for c in analyse}
    a = prefix(analyse[0])

    check("cached prefix is identical across every analyse call", len(prefixes) == 1,
          f"({len(prefixes)} distinct prefixes over {len(analyse)} calls)")
    check("cached prefix is long enough to cache at all (>1024 tokens)",
          len(a) > 4096, f"({len(a)} chars ~= {len(a)//4} tokens)")
    check("more than one distinct issue was actually exercised", len(titles) > 1,
          f"({sorted(titles)})")
    check("issue-specific content sits after the breakpoint",
          len(titles) > 1 and len(prefixes) == 1)
elif len(analyse) == 1:
    print("  \033[33m!\033[0m only one analyse call — send a second issue to test caching")

print(f"\n  \033[1m{ok}/{ok + fail} assertions hold\033[0m\n")
sys.exit(1 if fail else 0)
