# Security

## Reporting

Use GitHub's **private vulnerability reporting** on this repository — the
*Security* tab, *Report a vulnerability*. It opens a private thread; nothing is
public until an advisory is published.

Please do not open a public issue for a suspected vulnerability. This is a
self-hosted service, so every operator running it is exposed for as long as the
report is visible and unfixed.

A useful report needs the version or commit, what an attacker can reach, and the
smallest thing that demonstrates it. A proof of concept is welcome and a working
exploit is not required.

There is no bounty. This is one person's project.

## What is in scope

The parts that take input from somebody who is not the operator:

* **The webhook endpoint.** Anything that gets past the signature check, the
  replay window, the authorship gate or the label gate.
* **The issue body.** It reaches a model as data inside a delimiter and the
  agent is given no tools. A path that turns it into an instruction, an action,
  or an outbound request is in scope.
* **Clone URLs.** They are checked against an SSRF policy and an optional host
  allowlist. A URL that reaches something the policy meant to exclude is in
  scope.
* **Stored secrets.** Tokens and keys are encrypted at rest with a master key.
  Anything that reads them back without it — including through a log line, an
  error message, or the environment handed to a test command — is in scope.
* **Fix mode.** It is off unless three independent switches are on, and it opens
  draft pull requests only. A patch that reaches the default branch, edits CI
  configuration or dependency manifests, or escapes its worktree is in scope.
* **The graph engine and the CLI**, for anything a hostile *repository* can do
  to whoever indexes it — a crash is a bug, memory unsafety or code execution
  from parsing a file is a vulnerability.

## What is not

* **The test command is not sandboxed, and this is documented rather than
  accidental.** A repository names its own command and it runs with a scrubbed
  environment, its own process group and a timeout — none of which is isolation.
  The `sandbox` setting is where an operator puts that, and leaving it empty
  means the command runs with the server's privileges. Configuring fix mode on
  a repository whose contributors you do not trust, without a sandbox, is a
  decision the operator makes and not a vulnerability in this code.
* **Model output is not trusted and should not be treated as if it were.** A
  patch is a proposal on a draft pull request, for a human to read.
* **Denial of service by cost.** An operator who enables analysis on a
  repository that receives thousands of issues will be billed for them.
* Findings that require the operator's own master key, admin token, or shell.

The threat model these follow from is in [SERVER.md](./SERVER.md); where this
file and that one disagree, this one is the summary and that one is the detail.

## Supported versions

`0.x` — only the latest tag. There are no backports.
