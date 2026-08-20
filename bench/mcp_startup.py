#!/usr/bin/env python3
"""Measure MCP time-to-first-response: process spawn -> initialize reply.

This is the number that decides whether an agent uses the tool at all.
CodeGraph's own CLAUDE.md: "the agent dives into Read/grep before codegraph
finishes its ~2-3s startup, so it runs with no codegraph."
"""
import json, os, subprocess, sys, time

INIT = json.dumps({
    "jsonrpc": "2.0", "id": 1, "method": "initialize",
    "params": {"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "bench", "version": "0"}},
}) + "\n"


def measure(cmd, runs=5, timeout=30):
    """Wall time from spawn until the initialize response arrives."""
    times = []
    for _ in range(runs):
        t0 = time.perf_counter()
        p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True, bufsize=1)
        try:
            p.stdin.write(INIT)
            p.stdin.flush()
            deadline = t0 + timeout
            while time.perf_counter() < deadline:
                line = p.stdout.readline()
                if not line:
                    break
                try:
                    msg = json.loads(line)
                except json.JSONDecodeError:
                    continue  # some servers log to stdout before speaking JSON-RPC
                if msg.get("id") == 1:
                    times.append(time.perf_counter() - t0)
                    break
            else:
                times.append(float("inf"))
        finally:
            p.kill()
            p.wait()
    times.sort()
    return times[len(times) // 2] if times else float("inf")


def main():
    repo = os.path.abspath(sys.argv[1] if len(sys.argv) > 1
                           else "../.bench-repos/django")
    here = os.path.dirname(os.path.abspath(__file__))
    leangraph = os.path.join(here, "..", "target", "release", "leangraph")

    targets = [
        ("leangraph", [leangraph, "serve", "--mcp", "-p", repo]),
        ("codegraph", ["npx", "-y", "@colbymchenry/codegraph@1.5.0",
                       "serve", "--mcp", "--path", repo]),
    ]

    print(f"\nMCP startup — time from spawn to initialize response\nrepo: {repo}\n")
    results = {}
    for name, cmd in targets:
        t = measure(cmd)
        results[name] = t
        print(f"  {name:<12} {t*1000:>9.1f} ms" if t != float("inf")
              else f"  {name:<12}   no response within timeout")

    a, c = results.get("leangraph"), results.get("codegraph")
    if a and c and a > 0 and c != float("inf"):
        print(f"\n  leangraph is {c/a:.0f}x faster to first response\n")


if __name__ == "__main__":
    main()
