//! Fix mode: propose a patch as a pull request.
//!
//! This is the most dangerous thing in the codebase and it is off by default in
//! three independent ways: the repository must have `fix_mode` set, the issue
//! must carry a second label distinct from the one that triggers analysis, and a
//! write token must exist. Any one of them missing and nothing happens.
//!
//! What it will not do, ever:
//!
//! * push to the default branch, or force-push anything;
//! * merge, or mark a pull request auto-mergeable;
//! * touch CI configuration, dependency manifests or lockfiles — a patch that
//!   edits `.github/workflows` or `package.json` is a privilege escalation
//!   dressed as a bug fix, and it is the first thing a hostile issue would try;
//! * apply a patch it cannot verify, or one that reaches outside the worktree.
//!
//! The output is a pull request for a human to read. It is not an autonomous
//! change to anyone's code, and the design does not leave room for it to become
//! one by accident.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Paths a proposed patch may never touch, whatever the issue says.
///
/// CI config runs with repository credentials; manifests and lockfiles pull
/// executable code from the network. Neither belongs in a machine-proposed fix,
/// and both are what an injection attempt would aim at.
const FORBIDDEN: &[&str] = &[
    ".github/",
    ".gitlab-ci",
    ".circleci/",
    "Jenkinsfile",
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.toml",
    "Cargo.lock",
    "pyproject.toml",
    "setup.py",
    "requirements",
    "poetry.lock",
    "Dockerfile",
    "docker-compose",
    "Makefile",
    ".env",
    ".git/",
];

const MAX_FILES: usize = 12;
const MAX_PATCH_BYTES: usize = 60_000;

fn git(dir: &Path, token: Option<&str>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0").current_dir(dir);
    if let Some(t) = token {
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: Bearer {t}"));
    }
    cmd.args(args);
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = token.map_or_else(|| err.clone().into_owned(), |t| err.replace(t, "***"));
        bail!("git {}: {}", args.first().unwrap_or(&""), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Files a unified diff claims to touch.
///
/// Parsed from `+++ b/...` rather than trusting the model to declare them: the
/// check has to run on what will actually be applied, not on a summary of it.
pub fn touched_files(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| l.strip_prefix("+++ "))
        .map(|p| p.trim())
        .filter(|p| *p != "/dev/null")
        .map(|p| p.strip_prefix("b/").unwrap_or(p).to_string())
        .collect()
}

/// Reject before applying. `git apply` already refuses paths outside the tree,
/// but relying on that alone would mean the policy lives in someone else's code.
pub fn vet(patch: &str) -> Result<Vec<String>> {
    if patch.trim().is_empty() {
        bail!("the model produced no patch");
    }
    if patch.len() > MAX_PATCH_BYTES {
        bail!(
            "patch is {} bytes; refusing anything over {MAX_PATCH_BYTES}",
            patch.len()
        );
    }
    let files = touched_files(patch);
    if files.is_empty() {
        bail!("patch names no files");
    }
    if files.len() > MAX_FILES {
        bail!(
            "patch touches {} files; refusing anything over {MAX_FILES}",
            files.len()
        );
    }
    for f in &files {
        if f.starts_with('/') || f.contains("..") {
            bail!("patch reaches outside the repository: {f}");
        }
        let lower = f.to_ascii_lowercase();
        if let Some(hit) = FORBIDDEN.iter().find(|p| {
            lower.starts_with(&p.to_lowercase())
                || lower.contains(&format!("/{}", p.to_lowercase()))
        }) {
            bail!("patch touches {f}, which fix mode never edits (matched `{hit}`)");
        }
    }
    Ok(files)
}

pub struct Proposal {
    pub branch: String,
    pub files: Vec<String>,
    /// What the repository's own tests said about the patch, if the operator
    /// configured a command to run them with.
    pub tests: Option<TestRun>,
}

/// The outcome of running the repository's tests over an applied patch.
#[derive(Debug, Clone)]
pub struct TestRun {
    pub ok: bool,
    pub command: String,
    /// Tail of the combined output, capped. A failing suite's last lines are
    /// the useful ones, and a pull request body has a size limit.
    pub output: String,
    pub timed_out: bool,
}

/// How long a test command may run before it is killed.
const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// How much of its output reaches the pull request.
const TEST_OUTPUT_CHARS: usize = 4_000;

/// Run the repository's own tests against the applied patch.
///
/// **This executes code from the repository.** Three things make that a
/// decision rather than an accident, and none of them is a sandbox:
///
///   * the command is the operator's, written into the repository's config —
///     never the model's, and never inferred from the tree;
///   * fix mode is already off in three independent ways before a patch is
///     written at all;
///   * the patch cannot have touched CI configuration, dependency manifests,
///     lockfiles, Dockerfiles or Makefiles — those are refused before `git
///     apply` runs, and they are what a hostile patch would reach for to turn
///     "run the tests" into "run anything".
///
/// What this does *not* provide is isolation. The command runs as the server
/// does, with whatever that process can reach. Putting a boundary around it —
/// a container, a user, a network policy — is the operator's to do, and the
/// documentation says so rather than implying this is safe by construction.
fn run_tests(work: &Path, command: &str, files: &[String]) -> TestRun {
    run_with_timeout(work, command, files, TEST_TIMEOUT)
}

fn run_with_timeout(
    work: &Path,
    command: &str,
    files: &[String],
    limit: std::time::Duration,
) -> TestRun {
    use std::process::{Command, Stdio};

    // `{files}` where the command wants the affected paths; otherwise the
    // command runs whole, which is what a `make test` wants.
    let list = files.join(" ");
    let rendered = command.replace("{files}", &list);

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(&rendered)
        .current_dir(work)
        // A test that waits on stdin waits forever.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return TestRun {
                ok: false,
                command: rendered,
                output: format!("could not start the test command: {e}"),
                timed_out: false,
            }
        }
    };

    let deadline = std::time::Instant::now() + limit;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
            Err(_) => break,
        }
    }

    let out = child.wait_with_output().ok();
    let ok = !timed_out && out.as_ref().is_some_and(|o| o.status.success());
    let mut text = out
        .map(|o| {
            let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
            t.push_str(&String::from_utf8_lossy(&o.stderr));
            t
        })
        .unwrap_or_default();
    if text.len() > TEST_OUTPUT_CHARS {
        // The tail: a failing suite says what failed at the end.
        let cut = text.len() - TEST_OUTPUT_CHARS;
        let cut = text
            .char_indices()
            .map(|(i, _)| i)
            .find(|&i| i >= cut)
            .unwrap_or(0);
        text = format!("… (truncated)\n{}", &text[cut..]);
    }
    TestRun {
        ok,
        command: rendered,
        output: text.trim().to_string(),
        timed_out,
    }
}

/// Apply in a throwaway worktree, commit, push a fresh branch, open a PR.
///
/// A separate worktree rather than the indexed checkout: the graph is built
/// from that tree and a half-applied patch would poison every answer until the
/// next sync.
pub fn propose(
    repo_dir: &Path,
    default_branch: &str,
    issue_number: i64,
    patch: &str,
    token: Option<&str>,
    full_name: &str,
    test_command: Option<&str>,
) -> Result<Proposal> {
    let files = vet(patch)?;
    let token = token.context("fix mode needs a token with write access")?;

    let branch = format!("leangraph/issue-{issue_number}");
    if branch == default_branch {
        bail!("refusing to work on the default branch");
    }

    let work: PathBuf = repo_dir
        .parent()
        .unwrap_or(repo_dir)
        .join(format!(".leangraph-fix-{issue_number}"));
    let _ = git(
        repo_dir,
        None,
        &["worktree", "remove", "--force", &work.to_string_lossy()],
    );
    let _ = std::fs::remove_dir_all(&work);

    git(
        repo_dir,
        None,
        &[
            "worktree",
            "add",
            "--force",
            "-B",
            &branch,
            &work.to_string_lossy(),
            &format!("origin/{default_branch}"),
        ],
    )
    .context("creating an isolated worktree")?;

    let result = (|| -> Result<Proposal> {
        let patch_path = work.join(".leangraph-patch");
        std::fs::write(&patch_path, patch)?;

        // Check before apply, so a bad patch leaves the worktree untouched.
        git(
            &work,
            None,
            &[
                "apply",
                "--check",
                "--whitespace=nowarn",
                ".leangraph-patch",
            ],
        )
        .context("the patch does not apply cleanly to the current tree")?;
        git(
            &work,
            None,
            &["apply", "--whitespace=nowarn", ".leangraph-patch"],
        )?;
        std::fs::remove_file(&patch_path).ok();

        // Stage only what was vetted. `git add -A` would sweep up anything the
        // patch created outside the declared file list.
        for f in &files {
            git(&work, None, &["add", "--", f])?;
        }
        let staged = git(&work, None, &["diff", "--cached", "--name-only"])?;
        if staged.trim().is_empty() {
            bail!("the patch applied but changed nothing");
        }

        git(
            &work,
            None,
            &[
                "-c",
                "user.name=leangraph",
                "-c",
                "user.email=leangraph@localhost",
                "commit",
                "-m",
                &format!("Proposed fix for #{issue_number}\n\nGenerated by leangraph. Review before merging."),
            ],
        )?;
        // No --force: if the branch exists and has diverged, that is a human's
        // work and it is not ours to overwrite.
        // Before the push, so a patch that breaks the suite is still reported
        // rather than quietly opened as though nothing was checked.
        let tests = test_command
            .filter(|c| !c.trim().is_empty())
            .map(|c| run_tests(&work, c, &files));

        git(&work, Some(token), &["push", "origin", &branch])?;

        Ok(Proposal {
            branch: branch.clone(),
            files: files.clone(),
            tests,
        })
    })();

    // Always clean up, success or not: a leftover worktree makes the next
    // attempt fail for an unrelated reason.
    let _ = git(
        repo_dir,
        None,
        &["worktree", "remove", "--force", &work.to_string_lossy()],
    );
    let _ = std::fs::remove_dir_all(&work);
    let _ = full_name;
    result
}

/// What the pull request says about itself.
///
/// The claim has to match what happened. "Has not been run or tested" on a
/// patch whose suite passed understates it; the same sentence on one whose
/// suite failed would be the more dangerous error, so both are stated plainly
/// and the failing case says so first.
fn pr_body(issue_number: i64, tests: Option<&TestRun>) -> String {
    let mut b = format!("Proposed by leangraph for #{issue_number}.\n\n");
    match tests {
        None => b.push_str(
            "Generated from a code graph and **not run or tested**. Review it as you \
             would any patch from a stranger.\n",
        ),
        Some(t) if t.timed_out => b.push_str(&format!(
            "⚠️ The repository's tests were started and **timed out**, so this patch is \
             unverified.\n\n```\n$ {}\n{}\n```\n",
            t.command, t.output
        )),
        Some(t) if t.ok => b.push_str(&format!(
            "The repository's own tests **passed** against this patch.\n\n\
             ```\n$ {}\n{}\n```\n\nThat is the suite agreeing, not a review.\n",
            t.command, t.output
        )),
        Some(t) => b.push_str(&format!(
            "⚠️ The repository's own tests **failed** against this patch. It is opened \
             anyway so the failure is visible rather than discarded.\n\n\
             ```\n$ {}\n{}\n```\n",
            t.command, t.output
        )),
    }
    b.push_str("\nCloses nothing automatically.");
    b
}

/// Open the pull request. Separate from `propose` because pushing succeeds
/// through git while this needs the provider API, and a failure here should not
/// discard a branch that is already on the remote.
/// API root. Overridable for GitHub Enterprise, where the host is the
/// customer's own — `https://ghe.example.com/api/v3`.
pub fn api_base() -> String {
    std::env::var("LEANGRAPH_GITHUB_API")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.github.com".into())
}

pub async fn open_pr(
    full_name: &str,
    branch: &str,
    base: &str,
    issue_number: i64,
    token: &str,
    tests: Option<&TestRun>,
) -> Result<String> {
    let res = reqwest::Client::new()
        .post(format!("{}/repos/{full_name}/pulls", api_base()))
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "leangraph")
        .json(&serde_json::json!({
            "title": format!("Proposed fix for #{issue_number}"),
            "head": branch,
            "base": base,
            "body": pr_body(issue_number, tests),
            // Explicitly a draft: the point is a human reads it.
            "draft": true
        }))
        .send()
        .await?;

    let status = res.status();
    let v: serde_json::Value = res.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "github refused the pull request ({status}): {}",
            v.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
        );
    }
    v.get("html_url")
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .context("github returned no pull request url")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testkit::TempTree;

    #[test]
    fn a_passing_suite_is_reported_as_the_suite_agreeing() {
        let t = TempTree::new("fixtests");
        let r = run_tests(t.path(), "echo '3 passed'; exit 0", &[]);
        assert!(r.ok);
        assert!(!r.timed_out);
        assert!(r.output.contains("3 passed"), "{r:?}");

        let body = pr_body(7, Some(&r));
        assert!(body.contains("**passed**"), "{body}");
        assert!(
            body.contains("not a review"),
            "a green suite is not review, and the body must not imply it is"
        );
    }

    #[test]
    fn a_failing_suite_opens_the_pull_request_anyway_and_says_so() {
        // Discarding the branch would hide the failure. The point of the draft
        // is that a human sees what happened.
        let t = TempTree::new("fixtests-fail");
        let r = run_tests(t.path(), "echo 'assert 1 == 2'; exit 1", &[]);
        assert!(!r.ok);
        let body = pr_body(7, Some(&r));
        assert!(body.contains("**failed**"), "{body}");
        assert!(body.contains("assert 1 == 2"), "{body}");
    }

    #[test]
    fn a_command_that_never_finishes_is_killed_and_the_patch_called_unverified() {
        let t = TempTree::new("fixtests-hang");
        // Borrow the real path but with a deadline this test can wait out.
        let started = std::time::Instant::now();
        let r = run_with_timeout(
            t.path(),
            "sleep 30",
            &[],
            std::time::Duration::from_millis(400),
        );
        assert!(r.timed_out, "{r:?}");
        assert!(!r.ok);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(pr_body(7, Some(&r)).contains("timed out"));
    }

    #[test]
    fn no_command_means_the_body_says_nothing_was_run() {
        let body = pr_body(7, None);
        assert!(body.contains("not run or tested"), "{body}");
    }

    #[test]
    fn the_affected_paths_are_substituted_where_the_command_asks() {
        let t = TempTree::new("fixtests-files");
        let files = vec!["src/a.py".to_string(), "src/b.py".to_string()];
        let r = run_tests(t.path(), "echo GOT {files}", &files);
        assert!(r.output.contains("GOT src/a.py src/b.py"), "{r:?}");
        assert!(r.command.contains("src/a.py"), "and the command records it");
    }

    #[test]
    fn a_command_that_cannot_start_is_a_failure_not_a_panic() {
        let t = TempTree::new("fixtests-nocmd");
        let r = run_tests(t.path(), "definitely-not-a-real-binary-xyz", &[]);
        assert!(!r.ok);
    }

    fn patch_for(paths: &[&str]) -> String {
        paths
            .iter()
            .map(|p| format!("--- a/{p}\n+++ b/{p}\n@@ -1 +1 @@\n-old\n+new\n"))
            .collect()
    }

    #[test]
    fn accepts_an_ordinary_source_patch() {
        let files = vet(&patch_for(&["src/app.py", "tests/test_app.py"])).unwrap();
        assert_eq!(files, vec!["src/app.py", "tests/test_app.py"]);
    }

    #[test]
    fn rejects_ci_configuration() {
        // A workflow runs with repository credentials. This is the first thing
        // a hostile issue would aim at, and it must not depend on the model
        // having been well behaved.
        for p in [
            ".github/workflows/ci.yml",
            "src/../.github/workflows/ci.yml",
            ".gitlab-ci.yml",
            ".circleci/config.yml",
            "Jenkinsfile",
        ] {
            assert!(vet(&patch_for(&[p])).is_err(), "{p} should be rejected");
        }
    }

    #[test]
    fn rejects_dependency_manifests_and_lockfiles() {
        // These pull executable code from the network on the next build.
        for p in [
            "package.json",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "Cargo.toml",
            "pyproject.toml",
            "requirements.txt",
            "poetry.lock",
            "setup.py",
            "frontend/package.json",
        ] {
            assert!(vet(&patch_for(&[p])).is_err(), "{p} should be rejected");
        }
    }

    #[test]
    fn rejects_build_and_container_definitions() {
        for p in [
            "Dockerfile",
            "docker-compose.yml",
            "Makefile",
            ".env",
            "deploy/Dockerfile",
        ] {
            assert!(vet(&patch_for(&[p])).is_err(), "{p} should be rejected");
        }
    }

    #[test]
    fn rejects_paths_that_leave_the_repository() {
        for p in ["/etc/passwd", "../../../etc/shadow", "src/../../outside.py"] {
            assert!(vet(&patch_for(&[p])).is_err(), "{p} should be rejected");
        }
    }

    #[test]
    fn rejects_git_internals() {
        assert!(vet(&patch_for(&[".git/config"])).is_err());
        assert!(vet(&patch_for(&["sub/.git/hooks/pre-commit"])).is_err());
    }

    #[test]
    fn rejects_the_empty_and_the_enormous() {
        assert!(vet("").is_err());
        assert!(vet("   \n ").is_err());
        assert!(vet("no file headers at all").is_err());
        let many: Vec<String> = (0..MAX_FILES + 1).map(|i| format!("src/f{i}.py")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(vet(&patch_for(&refs)).is_err());
        assert!(vet(&"x".repeat(MAX_PATCH_BYTES + 1)).is_err());
    }

    #[test]
    fn ignores_deletions_when_listing_targets() {
        // `+++ /dev/null` is a delete; it names no destination to vet.
        let p = "--- a/src/gone.py\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        assert!(touched_files(p).is_empty());
    }

    #[test]
    fn keeps_a_blank_line_of_context() {
        // A blank context line is a lone space. Trimming it leaves the hunk
        // header promising more lines than the body supplies, and git rejects
        // the whole patch — silently turning fix mode into a no-op.
        use crate::server::agent::clean_patch;
        let raw = "```diff\n--- a/src/app.py\n+++ b/src/app.py\n@@ -1,3 +1,3 @@\n \
def add(a, b):\n-    return a - b\n+    return a + b\n \n```";
        let cleaned = clean_patch(raw);
        let body: Vec<&str> = cleaned.lines().skip(3).collect();
        assert_eq!(body.len(), 4, "hunk body lost a line: {body:?}");
        assert_eq!(body[3], " ");
        let old = body
            .iter()
            .filter(|l| l.starts_with(' ') || l.starts_with('-'))
            .count();
        let new = body
            .iter()
            .filter(|l| l.starts_with(' ') || l.starts_with('+'))
            .count();
        assert_eq!((old, new), (3, 3), "counts do not match the @@ header");
    }

    #[test]
    fn strips_fences_a_model_added_anyway() {
        use crate::server::agent::clean_patch;
        let raw = "Here is the fix:\n```diff\n--- a/x.py\n+++ b/x.py\n@@ -1 +1 @@\n-a\n+b\n```";
        let cleaned = clean_patch(raw);
        assert!(cleaned.starts_with("--- a/x.py"), "{cleaned}");
        assert!(!cleaned.contains("```"));
        assert!(!cleaned.contains("Here is"));
    }
}
