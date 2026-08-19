#!/usr/bin/env python3
"""The join layer: three vocabularies for naming the same function.

Every oracle names things differently, and every comparison below is only as
honest as this file. Getting it subtly wrong does not produce an error, it
produces a plausible number — which is the failure mode that matters here,
because a plausible number is one you publish.

    arbor       Flask.send_file          dotted, from Contains edges
    CodeGraph   Flask::send_file         double colon
    CPython     Flask.send_file          co_qualname, but nested functions
                                         become  outer.<locals>.inner

Nothing else in the benchmarks may parse a qualified name.
"""
import os
import re

# CPython marks a nested function's enclosing scope this way. Neither static
# tool models it, so it is folded out rather than pretended to be a scope.
LOCALS = re.compile(r"\.<locals>\.")

# A comprehension, a generator expression or a lambda is its own code object at
# runtime and is not a definition either tool extracts. Calls made from inside
# one belong to the function that contains it.
SYNTHETIC = ("<listcomp>", "<dictcomp>", "<setcomp>", "<genexpr>", "<lambda>", "<module>")


def norm_qual(q):
    """One spelling of a qualified name."""
    if not q:
        return ""
    q = q.replace("::", ".")
    q = LOCALS.sub(".", q)
    parts = [p for p in q.split(".") if p and p not in SYNTHETIC]
    return ".".join(parts)


def norm_path(p, root):
    """Repository-relative, forward slashes, no `./`."""
    if not p:
        return ""
    p = os.path.normpath(p)
    if os.path.isabs(p):
        try:
            p = os.path.relpath(p, root)
        except ValueError:
            return ""
    return p.replace("\\", "/").lstrip("./")


def in_repo(path, root):
    """Is this file part of the repository under test?

    Runtime tracing sees the standard library, site-packages and the test
    runner itself. Those are not edges any of these tools claims to have, and
    counting them as misses would make the recall number meaningless.
    """
    if not path:
        return False
    ap = os.path.abspath(os.path.join(root, path)) if not os.path.isabs(path) else path
    root = os.path.abspath(root)
    if not ap.startswith(root + os.sep):
        return False
    rel = os.path.relpath(ap, root).replace("\\", "/")
    # Any component, not just the first: a vendored environment can sit at any
    # depth, and a prefix check silently lets `src/.venv/...` count as ours.
    excluded = {"site-packages", ".venv", "venv", ".tox", "build", ".git",
                "node_modules", "__pycache__", "dist-packages", ".eggs"}
    return not (excluded & set(rel.split("/")))


class Lines:
    """Byte offset to 1-based line number.

    arbor stores spans as byte offsets into the file; CodeGraph and CPython both
    speak in line numbers. Cached per file because a large repository is asked
    this tens of thousands of times.
    """

    def __init__(self, root):
        self.root = root
        self._cache = {}

    def _table(self, rel):
        if rel not in self._cache:
            try:
                with open(os.path.join(self.root, rel), "rb") as f:
                    data = f.read()
            except OSError:
                self._cache[rel] = None
                return None
            starts = [0]
            for i, b in enumerate(data):
                if b == 0x0A:
                    starts.append(i + 1)
            self._cache[rel] = starts
        return self._cache[rel]

    def of(self, rel, byte):
        starts = self._table(rel)
        if not starts:
            return None
        lo, hi = 0, len(starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if starts[mid] <= byte:
                lo = mid
            else:
                hi = mid - 1
        return lo + 1


def key(path, qual):
    """The join key. File plus qualified name — not the name alone, which
    collides across a large repository, and not the line, which the two static
    tools disagree about by a decorator's worth."""
    return (path, norm_qual(qual))


if __name__ == "__main__":
    # Round-trip checks, so a change here fails loudly rather than shifting
    # every benchmark number by a few percent.
    assert norm_qual("Flask::send_file") == "Flask.send_file"
    assert norm_qual("Flask.send_file") == "Flask.send_file"
    assert norm_qual("outer.<locals>.inner") == "outer.inner"
    assert norm_qual("outer.<locals>.<listcomp>") == "outer"
    assert norm_qual("a::b::c") == "a.b.c"
    assert norm_qual("<module>") == ""
    assert norm_qual("") == ""
    assert norm_path("/tmp/r/src/a.py", "/tmp/r") == "src/a.py"
    assert norm_path("./src/a.py", "/tmp/r") == "src/a.py"
    assert in_repo("src/a.py", "/tmp")
    assert not in_repo("/usr/lib/python3.12/os.py", "/tmp")
    assert not in_repo("src/.venv/lib/x.py", "/tmp")
    assert not in_repo("a/b/site-packages/c.py", "/tmp")
    assert not in_repo("x/__pycache__/y.py", "/tmp")
    assert in_repo("src/venvironment/a.py", "/tmp")  # not a venv
    print("graphjoin: all assertions hold")
