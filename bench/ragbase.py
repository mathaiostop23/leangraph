#!/usr/bin/env python3
"""Embedding RAG, as a baseline that is actually the competitor.

Keyword search is what an agent without an index falls back to, and beating it
is table stakes. What people who build context for agents actually deploy is
embedding retrieval over chunked source, and until this existed the comparison
avoided the opponent it needed to face.

Deliberately the strongest version of it:

  * chunks, not whole files, so it is charged only for what it reads —
    keyword top-10 pays for ten entire files and this does not;
  * cosine over a normalised space, top chunks until the budget is spent;
  * the same token budget leangraph used on the same query, so the comparison
    is at equal cost rather than at equal k.

Embeddings are cached by file *content*, which is what makes 500 instances
feasible at all: SWE-bench pins each instance to its own commit, and django's
231 commits share almost every file. Without the cache this is five hours for
one repository.

`all-MiniLM-L6-v2` because it runs offline on this machine and needs no key. A
code-specific model would do better and this understates what a funded RAG
pipeline achieves — said here rather than left for someone to point out.
"""
import hashlib, os, pickle

import numpy as np

CHUNK_LINES = 40
OVERLAP = 10
BYTES_PER_TOKEN = 3.5
MODEL = "all-MiniLM-L6-v2"

_model = None
_cache = {}
_cache_path = None


def _load():
    global _model
    if _model is None:
        os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")
        from sentence_transformers import SentenceTransformer
        import torch
        dev = "mps" if torch.backends.mps.is_available() else "cpu"
        _model = SentenceTransformer(MODEL, device=dev)
    return _model


def open_cache(path):
    """Persist across runs; the first run pays for everything."""
    global _cache, _cache_path
    _cache_path = path
    if path and os.path.exists(path):
        with open(path, "rb") as f:
            _cache = pickle.load(f)
    return len(_cache)


def save_cache():
    if _cache_path:
        tmp = _cache_path + ".tmp"
        with open(tmp, "wb") as f:
            pickle.dump(_cache, f, protocol=4)
        os.replace(tmp, _cache_path)


def _chunks(text):
    lines = text.splitlines()
    step = CHUNK_LINES - OVERLAP
    for i in range(0, max(len(lines), 1), step):
        window = lines[i:i + CHUNK_LINES]
        if not window:
            break
        yield "\n".join(window)
        if i + CHUNK_LINES >= len(lines):
            break


def _embed_file(path):
    """Chunk embeddings for one file, keyed by its content."""
    try:
        raw = open(path, "rb").read()
    except OSError:
        return None
    key = hashlib.sha1(raw).hexdigest()
    if key in _cache:
        return _cache[key]
    try:
        text = raw.decode("utf-8", "replace")
    except Exception:
        return None
    cs = list(_chunks(text))
    if not cs:
        return None
    vecs = _load().encode(cs, batch_size=128, show_progress_bar=False,
                          normalize_embeddings=True).astype(np.float32)
    sizes = np.array([len(c) / BYTES_PER_TOKEN for c in cs], dtype=np.float32)
    _cache[key] = (vecs, sizes)
    return _cache[key]


def index(repo, files):
    """Embed every file, reusing whatever the cache already holds."""
    vecs, sizes, owner = [], [], []
    for rel in files:
        got = _embed_file(os.path.join(repo, rel))
        if got is None:
            continue
        v, s = got
        vecs.append(v)
        sizes.append(s)
        owner.extend([rel] * len(s))
    if not vecs:
        return None
    return np.vstack(vecs), np.concatenate(sizes), owner


def retrieve(idx, query, budget_tokens):
    """Top chunks by cosine, until the budget is spent."""
    if idx is None:
        return [], 0.0
    vecs, sizes, owner = idx
    q = _load().encode([query], normalize_embeddings=True).astype(np.float32)[0]
    scores = vecs @ q
    order = np.argsort(-scores)
    files, spent = [], 0.0
    seen = set()
    for i in order:
        t = float(sizes[i])
        if spent + t > budget_tokens and files:
            break
        spent += t
        f = owner[i]
        if f not in seen:
            seen.add(f)
            files.append(f)
    return files, spent
