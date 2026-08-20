//! Cloning and updating repositories.
//!
//! The token is never written to disk. `git clone https://token@host/...` puts
//! the credential in `.git/config`, where it survives the process, gets copied
//! by backups, and shows up in any bug report that includes the config. Passing
//! it per-invocation via `http.extraHeader` keeps it in the process environment
//! and nowhere else.
//!
//! It is still visible in the child's argv for the life of the call, which is a
//! real if narrow exposure on a shared host. The alternative — a credential
//! helper — needs a second executable and a socket; noted rather than pretended
//! away.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a repository lives under the data directory. Derived from the name
/// rather than stored, so the layout is inspectable and a stale row cannot point
/// somewhere unexpected.
pub fn work_dir(data_dir: &Path, full_name: &str) -> PathBuf {
    let safe: String = full_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '/' {
                c
            } else {
                '-'
            }
        })
        .collect();
    data_dir.join("repos").join(safe)
}

/// Reject anything that is not a plain https git URL, and anything pointing
/// inside the network the server is running on.
///
/// `file://` would read the host filesystem, `ssh://` and the scp-like
/// `git@host:path` form would use ambient key material, and `ext::` runs an
/// arbitrary command. A URL arrives over the API, so none of those may be
/// reachable from it.
///
/// The address check is the part that matters in a container. `https://` alone
/// is satisfied by `https://169.254.169.254/latest/meta-data/iam/...`, the cloud
/// metadata endpoint that hands out the instance's credentials, and by every
/// private address on whatever network the server can see. A repository URL is
/// the one user-supplied value in this product that causes an outbound
/// connection to a host of the caller's choosing, which is the definition of
/// server-side request forgery, so it is where the egress policy is enforced.
pub fn check_url(url: &str) -> Result<()> {
    let host = check_shape(url)?;
    if let Some(allowed) = allowlist() {
        if !allowed.iter().any(|a| host_matches(&host, a)) {
            bail!("{host} is not in LEANGRAPH_ALLOWED_HOSTS");
        }
        // An explicit allowlist is a deliberate statement about where this
        // install may reach, including somewhere private. It overrides the
        // address check rather than stacking with it.
        return Ok(());
    }
    check_public(&host)
}

/// The syntactic half, separated so it can be tested without a resolver.
/// Returns the host, since the caller needs it next either way.
fn check_shape(url: &str) -> Result<String> {
    let u = url.trim();
    if !u.starts_with("https://") {
        bail!("only https:// URLs are accepted");
    }
    if u.contains('@') {
        bail!("credentials in the URL are not accepted; configure a token instead");
    }
    if u.contains("..") || u.contains('\n') || u.contains('\r') {
        bail!("malformed URL");
    }
    host_of(u).context("cannot read a host from that URL")
}

/// Host part of an https URL, lowercased, port stripped.
fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// `example.com` matches itself and any subdomain of it. Written out rather
/// than done with `ends_with`, which would let `evil-example.com` through.
fn host_matches(host: &str, pattern: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    host == p || host.ends_with(&format!(".{p}"))
}

fn allowlist() -> Option<Vec<String>> {
    let raw = std::env::var("LEANGRAPH_ALLOWED_HOSTS").ok()?;
    let hosts: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    (!hosts.is_empty()).then_some(hosts)
}

/// Refuse a host that resolves anywhere but the public internet.
///
/// This resolves now and git resolves again when it connects, so a name whose
/// answer changes in between would slip past — DNS rebinding. Closing that
/// needs a resolver the connection itself is pinned to, which git does not
/// offer. Stated rather than papered over; `LEANGRAPH_ALLOWED_HOSTS` is the
/// airtight control, and a network policy on the container is the real one.
fn check_public(host: &str) -> Result<()> {
    use std::net::{IpAddr, ToSocketAddrs};

    let addrs: Vec<IpAddr> = format!("{host}:443")
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {host}"))?
        .map(|s| s.ip())
        .collect();
    if addrs.is_empty() {
        bail!("{host} resolves to nothing");
    }
    // Every address, not the first: a name that returns one public and one
    // private answer is exactly how this check gets bypassed.
    for ip in &addrs {
        if !is_public(*ip) {
            // Reads badly as "1.2.3.4 resolves to 1.2.3.4" when the host was
            // already an address, which is the commonest way this is probed.
            let what = if host == ip.to_string() {
                format!("{host} is not a public address")
            } else {
                format!("{host} resolves to {ip}, which is not a public address")
            };
            bail!("{what}; set LEANGRAPH_ALLOWED_HOSTS to permit an internal host deliberately");
        }
    }
    Ok(())
}

pub fn is_public(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            // 169.254.0.0/16 is the one that matters most: it carries the cloud
            // metadata endpoint and therefore the instance's credentials.
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
                || v4.octets()[0] == 127
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])) // CGNAT
                || v4.octets()[0] >= 224) // multicast and reserved
        }
        IpAddr::V6(v6) => {
            if let Some(m) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(m));
            }
            let seg = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (seg & 0xfe00) == 0xfc00 // unique local
                || (seg & 0xffc0) == 0xfe80 // link local
                || (seg & 0xff00) == 0xff00) // multicast
        }
    }
}

fn run(dir: Option<&Path>, token: Option<&str>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    // Never prompt: a hung credential prompt inside a worker is a job that
    // never finishes and a queue that never drains.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(t) = token {
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: Bearer {t}"));
    }
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        // The token cannot appear here — it is in a `-c` argument, not the
        // output — but scrub anyway rather than rely on that staying true.
        let err = String::from_utf8_lossy(&out.stderr);
        let err = match token {
            Some(t) => err.replace(t, "***"),
            None => err.into_owned(),
        };
        bail!("git {}: {}", args.first().unwrap_or(&""), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Clone if absent, otherwise fast-forward to the remote branch.
///
/// Returns the HEAD before and after, so a sync can diff two trees instead of
/// walking — the cheap path measured in BENCH.md.
pub fn fetch(
    dir: &Path,
    url: &str,
    branch: &str,
    token: Option<&str>,
) -> Result<(Option<String>, String)> {
    check_url(url)?;

    if !dir.join(".git").is_dir() {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Blobless rather than shallow: co-change needs history, and `--depth`
        // would make `git log` useless while a filtered clone keeps it and
        // fetches file contents lazily.
        run(
            None,
            token,
            &[
                "clone",
                "--filter=blob:none",
                "--branch",
                branch,
                url,
                &dir.to_string_lossy(),
            ],
        )?;
        // A blobless clone fetches history lazily, and the first `git log
        // --name-only` — which co-change runs — triggers those fetches. Measured
        // on a 677-commit repo: the first index took 6.8s where every later one
        // took 44ms. Doing it here moves that cost into the clone, where a user
        // expects to wait, instead of into the first answer, where they do not.
        warm_history(dir, token);
        let head = head(dir, token).context("cloned repository has no HEAD")?;
        return Ok((None, head));
    }

    let before = head(dir, token);
    run(Some(dir), token, &["fetch", "--prune", "origin", branch])?;
    // Reset rather than merge: this is a mirror of the remote, and a merge
    // conflict in a directory nobody edits is a job that fails forever.
    run(
        Some(dir),
        token,
        &["reset", "--hard", &format!("origin/{branch}")],
    )?;
    let after = head(dir, token).context("repository has no HEAD after fetch")?;
    Ok((before, after))
}

/// Force any lazy object fetches a partial clone deferred. Best effort: a
/// failure here costs latency later, not correctness.
///
/// `--no-renames` matters more than it looks. Rename detection compares blob
/// contents, and on a blobless clone every comparison is a lazy fetch — one
/// network round trip per historical blob. Measured on pallets/click: 23.8s
/// with it, 0.04s without, against a clone that itself took 2.2s. The argument
/// list here must stay in step with `cochange::edges`, since the point is to
/// warm exactly what that call will need.
fn warm_history(dir: &Path, token: Option<&str>) {
    let _ = run(
        Some(dir),
        token,
        &[
            "log",
            "--no-merges",
            "--no-renames",
            "-n",
            "3000",
            "--name-only",
            "--format=%H",
        ],
    );
}

pub fn head(dir: &Path, token: Option<&str>) -> Option<String> {
    run(Some(dir), token, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
}

/// `https://github.com/owner/repo(.git)` -> `owner/repo`.
pub fn name_from_url(url: &str) -> Option<String> {
    let rest = url.trim().trim_end_matches('/').strip_prefix("https://")?;
    let mut parts = rest.splitn(2, '/');
    let _host = parts.next()?;
    let path = parts.next()?.trim_end_matches(".git");
    (path.matches('/').count() == 1 && !path.is_empty()).then(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_scheme_rules_still_hold() {
        // The shape half only — asserting on `check_url` here would make the
        // test need a working resolver, and a test that fails on a train is a
        // test people learn to ignore.
        assert_eq!(
            check_shape("https://github.com/owner/name").unwrap(),
            "github.com"
        );
        for u in [
            "http://github.com/o/n",
            "file:///etc/passwd",
            "ssh://git@github.com/o/n",
            "git@github.com:o/n",
            "ext::sh -c whoami",
            "https://user:token@github.com/o/n",
            "https://github.com/../../etc",
        ] {
            assert!(check_shape(u).is_err(), "{u} should be rejected");
        }
    }

    #[test]
    fn the_metadata_endpoint_is_not_a_git_host() {
        // The single most valuable target in any container: it answers with the
        // instance's credentials to anything that can make an outbound request.
        assert!(!is_public(ip("169.254.169.254")));
        // A literal address needs no resolver, so this one stays end to end.
        assert!(check_url("https://169.254.169.254/latest/meta-data/").is_err());
        assert!(!is_public(ip("fd00:ec2::254")));
    }

    #[test]
    fn private_and_local_addresses_are_refused() {
        for a in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip(a)), "{a} should not count as public");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        for a in [
            "140.82.121.4",
            "1.1.1.1",
            "8.8.8.8",
            "2606:4700::1111",
            "2001:4860::8888",
        ] {
            assert!(is_public(ip(a)), "{a} should count as public");
        }
    }

    #[test]
    fn the_host_is_parsed_the_way_a_client_would() {
        assert_eq!(
            host_of("https://github.com/o/n").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_of("https://GitHub.COM/o/n").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_of("https://ghe.example.com:8443/o/n").as_deref(),
            Some("ghe.example.com")
        );
        assert_eq!(host_of("https://[::1]/o/n").as_deref(), Some("::1"));
        assert_eq!(host_of("https://github.com").as_deref(), Some("github.com"));
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn an_allowlist_entry_covers_subdomains_but_not_lookalikes() {
        assert!(host_matches("github.com", "github.com"));
        assert!(host_matches("codeload.github.com", "github.com"));
        assert!(!host_matches("evil-github.com", "github.com"));
        assert!(!host_matches("github.com.evil.test", "github.com"));
        assert!(!host_matches("notgithub.com", "github.com"));
    }
}
