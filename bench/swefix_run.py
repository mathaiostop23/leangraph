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
    out = sh([LEANGRAPH, "context", text, "-p", repo, "--max-nodes", str(max_nodes),
              "--max-bytes", str(max_bytes)], repo)
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


# Input/output dollars per million tokens, for the estimate only. The run
# reports what was actually billed from each response's usage.
PRICE = {
    "claude-opus-5":   (5.0, 25.0),
    "claude-sonnet-5": (2.0, 10.0),
    "gpt-4.1":         (2.0,  8.0),
    "gpt-4.1-mini":    (0.4,  1.6),
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
    r = requests.post(url, headers=headers, json=body, timeout=600)
    if not r.ok:
        # The message matters more than the status: "no credits" and "bad key"
        # are both 4xx and only one of them is worth retrying tomorrow.
        raise RuntimeError(f"{r.status_code}: {r.text[:200]}")
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
    return (t[i:] if i > 0 else t).strip() + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--repos", required=True)
    ap.add_argument("--n", type=int, default=25)
    ap.add_argument("--arm", choices=["leangraph", "keyword"], default="leangraph")
    ap.add_argument("--model", default="")
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

    pin, pout = PRICE.get(model, (3.0, 15.0))
    cost = est_in / 1e6 * pin + est_out / 1e6 * pout
    print(f"\n  {len(prompts)} prompts · {est_in:,.0f} input tokens · "
          f"~{est_out:,.0f} output")
    print(f"  \033[1mestimated cost: ${cost:.2f}\033[0m at {model} rates")

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
    billed = spent_in / 1e6 * pin + spent_out / 1e6 * pout
    print(f"\n  {sum(1 for v in out.values() if v.strip()):,}/{len(out)} produced a diff")
    print(f"  billed: {spent_in:,} in + {spent_out:,} out = \033[1m${billed:.2f}\033[0m")
    print(f"  patches -> {dest}")
    print(f"\n  score them:\n    bench/swefix.py --data <records.json> --patches {dest}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
