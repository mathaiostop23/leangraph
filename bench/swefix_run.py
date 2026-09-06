#!/usr/bin/env python3
"""Generate the patches `swefix.py` scores, one arm per kind of context.

The question is not "can a model fix SWE-bench" — published agents answer that,
and they iterate: run the tests, read more, try again. This asks the narrower
question this project is actually about: **given one shot and a fixed budget,
does our context produce a correct patch more often than grep's does?**

So both arms are handicapped identically. Same model, same prompt, same single
call, no tools, no retries. The only difference is what code was put in front of
it. Absolute numbers from this will be far below any leaderboard and are not
comparable to one; the difference between the arms is the measurement.

  arm A   leangraph context, ~16k tokens
  arm B   keyword top-10, whole files, ~280k tokens

B is deliberately the generous baseline: it is what an agent without an index
actually reads, at eighteen times the cost. Winning against a starved opponent
would prove nothing.

Nothing is spent without `--go`. The default prints what it would cost.

Usage:
  bench/swefix_run.py --data ~/.cache/swebench/verified.json \
      --repos ~/.cache/swebench --n 25 [--arm leangraph|keyword] [--go]
"""
import argparse, json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
LEANGRAPH = os.path.join(HERE, "..", "target", "release", "leangraph")
BYTES_PER_TOKEN = 3.5

# Only instances the benchmark publishes an arm64 image for: without one the
# patch cannot be scored, so generating it would be spending for nothing.
IMAGE = "swebench/sweb.eval.arm64.{}:latest"

STOP = {"the","and","for","not","but","with","from","this","that","when","then","than",
        "have","has","was","are","you","all","any","can","use","using","used","get","set",
        "add","new","one","two","out","off","its","our","how","why","who","what","which",
        "where","some","only","also","into","over","same","such","each","more","most",
        "other","should","would","could","does","did","done","make","made","see","seen",
        "call","called","run","raise","error","issue","bug","fix","test","code","file",
        "line","python","expected"}

PROMPT = """\
Below is code from a repository, then a bug report against it.

Produce a minimal unified diff that fixes the bug. Output the diff and nothing
else — no prose, no explanation, no markdown fences. Paths in the diff must be
repository-relative and must match the files shown.

The output must be a patch `git apply` accepts, and must begin with a
`diff --git` line followed by `---`/`+++` headers and `@@` hunks carrying real
line numbers. Do NOT use the `*** Begin Patch` / `*** Update File:` /
`*** End Patch` envelope — that format is rejected here.

## Code

{context}

## Issue

{issue}
"""


def sh(args, cwd=None):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True).stdout


def has_image(instance_id):
    tag = IMAGE.format(instance_id.replace("__", "_1776_"))
    return subprocess.run(["docker", "manifest", "inspect", tag],
                          capture_output=True).returncode == 0


def leangraph_context(repo, text, max_nodes, max_bytes):
    # `--source`, not the bare ranking. Without it this arm shipped a table of
    # node names and no code at all, and the model wrote its patch against a
    # half-remembered version of the file — which is not a retrieval result,
    # just a measurement of recall from pretraining.
    out = sh([LEANGRAPH, "context", text, "-p", repo, "--max-nodes", str(max_nodes),
              "--max-bytes", str(max_bytes), "--source"], repo)
    return out


def keyword_context(repo, text, top=10):
    """What an agent without an index reads: grep's top files, whole."""
    terms = [t for t in re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", text)
             if t.lower() not in STOP][:24]
    hits = {}
    for t in set(terms):
        for f in sh(["git", "grep", "-l", "-F", "--", t], cwd=repo).splitlines():
            if f.endswith(".py"):
                hits[f] = hits.get(f, 0) + 1
    ranked = sorted(hits, key=lambda f: (-hits[f], f))[:top]
    parts = []
    for f in ranked:
        try:
            parts.append(f"### {f}\n```\n" + open(os.path.join(repo, f),
                         encoding="utf-8", errors="replace").read() + "\n```")
        except OSError:
            pass
    return "\n\n".join(parts)


# --------------------------------------------------------------------- models

def provider_from_env():
    """Whichever key exists, named the same way the server names them."""
    if os.environ.get("ANTHROPIC_API_KEY"):
        return "anthropic", os.environ["ANTHROPIC_API_KEY"]
    if os.environ.get("OPENAI_API_KEY"):
        return "openai", os.environ["OPENAI_API_KEY"]
    return None, None


# Input/output dollars per million tokens, for the estimate only; the run
# reports what was actually billed from each response's own usage.
#
# A model not in this table gets no dollar estimate rather than a guessed one.
# The first version fell back to a made-up rate, which is the worst behaviour
# available here: the whole point of the dry run is to say what a thing costs
# before buying it, and a confident wrong number is less useful than none. Pass
# `--price IN,OUT` to estimate a model this table has never heard of.
PRICE = {
    "claude-opus-5":   (5.0, 25.0),
    "claude-sonnet-5": (2.0, 10.0),
    "claude-haiku-4-5": (1.0, 5.0),
    "gpt-4.1":         (2.0,  8.0),
    "gpt-4.1-mini":    (0.4,  1.6),
    # Short-context rate. Past the long-context threshold luna bills
    # $0.40/$1.80, so a whole-file arm can be estimated at half what it costs.
    "gpt-5.6-luna":    (0.2,  1.2),
    "gpt-5.6-terra":   (2.0, 12.0),
    "gpt-5.6-sol":     (4.0, 20.0),
    "gpt-6-astra":    (10.0, 50.0),
}


def call(provider, key, model, prompt, max_tokens=8192):
    # `requests` rather than `urllib`: urllib uses the platform trust store and
    # fails with CERTIFICATE_VERIFY_FAILED on a machine whose Python was not
    # installed with one — which is not a rare configuration and is a confusing
    # way for a benchmark to die. requests carries its own roots.
    import requests
    if provider == "anthropic":
        url = "https://api.anthropic.com/v1/messages"
        headers = {"x-api-key": key, "anthropic-version": "2023-06-01",
                   "content-type": "application/json"}
        body = {"model": model, "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": prompt}]}
    else:
        url = "https://api.openai.com/v1/chat/completions"
        headers = {"authorization": f"Bearer {key}", "content-type": "application/json"}
        body = {"model": model, "max_completion_tokens": max_tokens,
                "messages": [{"role": "user", "content": prompt}]}
    # A 429 is not an answer about the context, it is an answer about the
    # minute. The whole-file arm sends 230k-token prompts and walks into the
    # per-minute token limit; without this it silently lost six of twenty
    # instances, all django, and the surviving sample was no longer the one the
    # comparison claimed to be over. Retry on 429 and 5xx, honouring
    # Retry-After when the server sends one. A 4xx that is not 429 is a real
    # refusal — no credits, bad key, bad model — and retrying it just burns
    # time, so it is raised at once.
    for attempt in range(6):
        r = requests.post(url, headers=headers, json=body, timeout=600)
        if r.ok:
            break
        if r.status_code != 429 and r.status_code < 500:
            raise RuntimeError(f"{r.status_code}: {r.text[:200]}")
        if attempt == 5:
            raise RuntimeError(f"{r.status_code} after 6 tries: {r.text[:160]}")
        wait = float(r.headers.get("retry-after") or 0) or min(60.0, 4.0 * 2 ** attempt)
        print(f"    {r.status_code}, waiting {wait:.0f}s", flush=True)
        time.sleep(wait)
    v = r.json()
    if provider == "anthropic":
        text = "".join(b.get("text", "") for b in v.get("content", [])
                       if b.get("type") == "text")
        u = v.get("usage", {})
        return text, u.get("input_tokens", 0), u.get("output_tokens", 0)
    text = v["choices"][0]["message"].get("content") or ""
    u = v.get("usage", {})
    return text, u.get("prompt_tokens", 0), u.get("completion_tokens", 0)


def clean_diff(text):
    """A model told not to use fences sometimes uses them anyway."""
    t = text.strip()
    if t.startswith("```"):
        t = re.sub(r"^```[a-z]*\n", "", t)
        t = re.sub(r"\n```$", "", t)
    i = t.find("diff --git")
    if i < 0:
        i = t.find("--- ")
    return repair_hunks((t[i:] if i > 0 else t).strip() + "\n")


def repair_hunks(diff):
    """Fix hunk bookkeeping without touching a single added or removed line.

    Two things go wrong in generated diffs, and neither is about the edit. A
    blank context line gets emitted empty instead of as a single space, which
    ends the hunk early as far as `git apply` is concerned; and the counts in
    the `@@` header disagree with the body. Both are clerical. Repairing them
    keeps the measurement on whether the fix was right, which is the question
    the benchmark asks.
    """
    lines = diff.splitlines()
    out, i = [], 0
    while i < len(lines):
        line = lines[i]
        m = re.match(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$", line)
        if not m:
            out.append(line)
            i += 1
            continue
        old_start, new_start, tail = m.group(1), m.group(2), m.group(3)
        body, j = [], i + 1
        while j < len(lines):
            nxt = lines[j]
            if nxt.startswith("@@") or nxt.startswith("diff --git") or \
               nxt.startswith("--- ") or nxt.startswith("+++ "):
                break
            # An empty line inside a hunk is a blank context line.
            body.append(" " if nxt == "" else nxt)
            j += 1
        # Trailing blanks belong to the gap between hunks, not the hunk.
        while body and body[-1] == " ":
            body.pop()
        old = sum(1 for b in body if b[:1] in (" ", "-"))
        new = sum(1 for b in body if b[:1] in (" ", "+"))
        out.append(f"@@ -{old_start},{old} +{new_start},{new} @@{tail}")
        out.extend(body)
        i = j
    return "\n".join(out).rstrip("\n") + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--repos", required=True)
    ap.add_argument("--n", type=int, default=25)
    ap.add_argument("--arm", choices=["leangraph", "keyword"], default="leangraph")
    ap.add_argument("--model", default="",
                    help="any model id the provider accepts; the default is the "
                         "cheapest that has been used here")
    ap.add_argument("--price", default="",
                    help="IN,OUT dollars per million tokens, for a model this "
                         "script has no rate for")
    ap.add_argument("--max-nodes", type=int, default=100)
    ap.add_argument("--max-bytes", type=int, default=100_000)
    ap.add_argument("--out", default="")
    ap.add_argument("--go", action="store_true", help="actually spend money")
    args = ap.parse_args()

    provider, key = provider_from_env()
    model = args.model or ("claude-sonnet-5" if provider == "anthropic" else "gpt-4.1")
    rows = json.load(open(os.path.expanduser(args.data)))
    repos = os.path.expanduser(args.repos)

    have = {d for d in os.listdir(repos)
            if os.path.isdir(os.path.join(repos, d, ".git"))}
    rows = [r for r in rows if r["repo"].split("/")[1] in have]

    print(f"\n\033[1mfix mode — {args.arm} arm\033[0m")
    print(f"  provider: {provider or 'NONE — set ANTHROPIC_API_KEY or OPENAI_API_KEY'}")
    print(f"  model:    {model}\n")

    # Only what can be scored. Checked before anything is generated.
    picked, checked = [], 0
    for r in rows:
        if len(picked) >= args.n:
            break
        checked += 1
        if has_image(r["instance_id"]):
            picked.append(r)
    print(f"  {len(picked)} instances with a runnable image (of {checked} checked)\n")

    est_in = est_out = 0
    prompts = {}
    for r in picked:
        repo = os.path.abspath(os.path.join(repos, r["repo"].split("/")[1]))
        sh(["git", "checkout", "-q", "-f", r["base_commit"]], cwd=repo)
        sh(["git", "clean", "-qfd", "-e", ".leangraph"], cwd=repo)
        subprocess.run(["rm", "-rf", os.path.join(repo, ".leangraph")])
        subprocess.run([LEANGRAPH, "index", repo], capture_output=True)
        text = r["problem_statement"]
        ctx = (leangraph_context(repo, text, args.max_nodes, args.max_bytes)
               if args.arm == "leangraph" else keyword_context(repo, text))
        p = PROMPT.format(context=ctx, issue=text)
        prompts[r["instance_id"]] = p
        est_in += len(p) / BYTES_PER_TOKEN
        est_out += 1500
        print(f"  built {r['instance_id']:<34} {len(p)/BYTES_PER_TOKEN:>9,.0f} tokens", flush=True)

    rate = PRICE.get(model)
    if not rate and args.price:
        try:
            a, b = args.price.split(",")
            rate = (float(a), float(b))
        except ValueError:
            print(f"  --price wants IN,OUT — got {args.price!r}")
            return 2
    print(f"\n  {len(prompts)} prompts · {est_in:,.0f} input tokens · "
          f"~{est_out:,.0f} output")
    if rate:
        cost = est_in / 1e6 * rate[0] + est_out / 1e6 * rate[1]
        print(f"  \033[1mestimated cost: ${cost:.2f}\033[0m at {model} rates")
    else:
        print(f"  \033[1mno published rate here for {model}\033[0m — pass "
              f"--price IN,OUT to estimate, or read the billed figure after "
              f"the run, which comes from the response and is always right")

    if not args.go:
        print("\n  nothing spent. add --go to run it.\n")
        return 0
    if not provider:
        print("\n  no key in the environment; refusing to run.\n")
        return 1

    out, spent_in, spent_out = {}, 0, 0
    for i, (iid, p) in enumerate(prompts.items(), 1):
        try:
            text, ti, to = call(provider, key, model, p)
            out[iid] = clean_diff(text)
            spent_in += ti
            spent_out += to
        except Exception as e:
            print(f"  ! {iid}: {type(e).__name__} {str(e)[:80]}")
            out[iid] = ""
        print(f"  {i}/{len(prompts)}  {iid:<34} {len(out[iid]):>6} chars", flush=True)
        time.sleep(0.5)

    dest = args.out or f"/tmp/patches-{args.arm}.json"
    json.dump(out, open(dest, "w"))
    print(f"\n  {sum(1 for v in out.values() if v.strip()):,}/{len(out)} produced a diff")
    if rate:
        billed = spent_in / 1e6 * rate[0] + spent_out / 1e6 * rate[1]
        print(f"  billed: {spent_in:,} in + {spent_out:,} out = \033[1m${billed:.2f}\033[0m")
    else:
        print(f"  used: {spent_in:,} input + {spent_out:,} output tokens "
              f"(no rate here to price them)")
    print(f"  patches -> {dest}")
    print(f"\n  score them:\n    bench/swefix.py --data <records.json> --patches {dest}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
