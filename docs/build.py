#!/usr/bin/env python3
"""Wrap the page source in the document shell that GitHub Pages needs.

The same file is published as an artifact, where the host supplies the
doctype, the head and a CSS reset. Served from Pages there is no host, so the
shell is added here rather than kept in two hand-maintained copies that drift.

    docs/build.py <source.html>
"""
import re
import sys

HEAD = '''<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>leangraph — find where the code has to change</title>
<meta name="description" content="A code graph that points a coding agent at the code that has to change. 81.8% of the files an accepted fix touched, over 500 real issues, at one eighteenth of the tokens keyword search reads.">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#FFFFFF" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#080D19" media="(prefers-color-scheme: dark)">
<link rel="canonical" href="https://mathaiostop23.github.io/leangraph/">
<meta property="og:type" content="website">
<meta property="og:title" content="leangraph">
<meta property="og:description" content="Coding agents don't struggle to write code. They struggle to find where to write it.">
<meta property="og:url" content="https://mathaiostop23.github.io/leangraph/">
<meta name="twitter:card" content="summary_large_image">
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns=&#39;http://www.w3.org/2000/svg&#39; viewBox=&#39;0 0 32 32&#39;%3E%3Ccircle cx=&#39;16&#39; cy=&#39;9&#39; r=&#39;3.4&#39; fill=&#39;%23E8A24A&#39;/%3E%3Ccircle cx=&#39;7&#39; cy=&#39;23&#39; r=&#39;2.6&#39; fill=&#39;%2373777F&#39;/%3E%3Ccircle cx=&#39;25&#39; cy=&#39;23&#39; r=&#39;2.6&#39; fill=&#39;%2373777F&#39;/%3E%3Cpath d=&#39;M16 9 7 23M16 9l9 14M7 23h18&#39; stroke=&#39;%2373777F&#39; stroke-width=&#39;1.6&#39; fill=&#39;none&#39;/%3E%3C/svg%3E">
<style>
/* The artifact host ships a reset; a standalone page does not. */
*, *::before, *::after { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body { margin: 0; }
img, svg, canvas { display: block; max-width: 100%; }
button { font: inherit; }
</style>
'''


def main():
    src = open(sys.argv[1]).read()
    # Head material is everything before the first rendered element. Splitting
    # on the wrong tag once left a wrapper in the head and an orphaned closing
    # tag in the body, so the boundary is found rather than guessed.
    m = re.search(r'^<(?:div|header|main|section|nav|button|a)\b', src, re.M)
    if not m:
        sys.exit("no body content found")
    head = src[:m.start()].replace("<title>leangraph</title>\n", "", 1)
    out = HEAD + head + "</head>\n<body>\n" + src[m.start():] + "\n</body>\n</html>\n"

    depth = 0
    for tag in re.finditer(r"<div\b|</div>", out):
        depth += 1 if tag.group().startswith("<div") else -1
        if depth < 0:
            sys.exit("unbalanced <div> in output")
    if depth:
        sys.exit(f"{depth} unclosed <div> in output")

    open("docs/index.html", "w").write(out)
    print(f"  docs/index.html — {len(out):,} bytes, tags balance")


if __name__ == "__main__":
    main()
