#!/usr/bin/env python3
"""CodeGraph as a column, because it is the opponent this project names.

Speed and size have been measured against CodeGraph since Phase 0. Retrieval
never has — the cost benchmark compares leangraph against *grep*, which leaves
the obvious objection unanswered: being ten times faster to index is worth
nothing if the other engine puts better files in front of the agent.

`codegraph explore` is the counterpart to `leangraph context`. It is CodeGraph's
primary MCP tool, by its own design note that agents under-pick secondary ones,
and it answers the same question: given this text, which code should be read.

Two honest wrinkles, stated rather than smoothed over:

  * **Different budgets.** leangraph is capped in nodes and bytes, `explore` in
    files. Neither can be set to the other's units, so the comparison is each
    tool at its own default, with the token cost reported beside the recall.
  * **What counts as returned.** `explore` prints source for a few files and
    *names* others in a blast-radius list. Source is the like-for-like set;
    the named ones are a pointer, not content. Both are scored, separately.
"""
import os, re, subprocess

CG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "..")
BYTES_PER_TOKEN = 3.5

# Set by the harness once, so a missing install fails at startup rather than
# 500 instances into a run.
BIN = None

_SOURCE_HEADER = re.compile(r"^\*\*`([^`]+)`\*\*", re.M)
_CITED = re.compile(r"\(([\w./-]+\.(?:py|ts|tsx|js|jsx)):\d+\)")
_FENCE = re.compile(r"^```", re.M)


def available(path):
    """Fail loudly and early if the binary is not where it was said to be."""
    if not path or not os.path.exists(path):
        raise SystemExit(f"codegraph not found at {path!r} — install it with\n"
                         f"  npm i --prefix <dir> @colbymchenry/codegraph@1.5.0")
    return path


def index(repo):
    """A fresh index, to match the fresh index leangraph is measured on."""
    subprocess.run([BIN, "init", "."], cwd=repo, stdin=subprocess.DEVNULL,
                   capture_output=True, text=True, timeout=600)


def explore(repo, text, max_files=None):
    """Files whose source came back, files merely named, and what it cost.

    Cost is the whole of what `explore` prints, because that is what an agent
    pays for: the prose framing and the blast-radius list are real tokens. The
    source-only figure is kept beside it so the two engines can also be compared
    on content alone.
    """
    cmd = [BIN, "explore", text, "-p", "."]
    if max_files:
        cmd += ["--max-files", str(max_files)]
    try:
        out = subprocess.run(cmd, cwd=repo, stdin=subprocess.DEVNULL,
                             capture_output=True, text=True, timeout=300).stdout
    except subprocess.TimeoutExpired:
        return [], [], 0.0, 0.0
    if not out.strip():
        return [], [], 0.0, 0.0

    source_files = [m.group(1) for m in _SOURCE_HEADER.finditer(out)]
    cited = sorted({m.group(1) for m in _CITED.finditer(out)} | set(source_files))

    # Bytes inside fenced blocks: the source it actually handed over.
    src_bytes, inside, at = 0, False, 0
    for m in _FENCE.finditer(out):
        if inside:
            src_bytes += m.start() - at
        else:
            at = m.end()
        inside = not inside

    return (source_files, cited,
            len(out) / BYTES_PER_TOKEN, src_bytes / BYTES_PER_TOKEN)
