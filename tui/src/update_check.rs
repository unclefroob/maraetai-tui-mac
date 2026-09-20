//! Checks GitHub for a newer commit than this binary was built from, and
//! can apply an update by re-running `cargo install --git` for both
//! binaries — the Settings tab's (`8`) whole reason to exist.
//!
//! "Newer" is judged by commit, not a version number: this project
//! doesn't tag releases, so the meaningful comparison is "is there a
//! commit on the remote's default branch that isn't the one this binary
//! was built from" (see `build.rs` for how that gets embedded at compile
//! time).

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Where this repo lives — hardcoded, since this (the macOS fork) is a
/// genuinely separate GitHub repository from the Linux original, not a
/// runtime choice. Point this at `maraetai-tui-mac`, not `maraetai-tui` —
/// this binary was built from the fork, and that's what it needs to check
/// and update against.
const REPO_OWNER: &str = "unclefroob";
const REPO_NAME: &str = "maraetai-tui-mac";
const REPO_BRANCH: &str = "master";

/// The commit (and its date) this binary was actually built from — see
/// `build.rs`. `"unknown"` if `git` wasn't available at build time.
pub const BUILT_FROM_HASH: &str = env!("MARAETAI_GIT_HASH");
pub const BUILT_FROM_DATE: &str = env!("MARAETAI_GIT_DATE");

/// One commit's worth of info from GitHub's API — just enough to show
/// "here's what you'd be updating to" before asking for confirmation.
#[derive(Debug, Clone)]
pub struct RemoteCommit {
    pub sha: String,
    pub date: String,
    pub summary: String,
}

#[derive(Deserialize)]
struct ApiCommitResponse {
    sha: String,
    commit: ApiCommitDetail,
}

#[derive(Deserialize)]
struct ApiCommitDetail {
    committer: ApiCommitter,
    message: String,
}

#[derive(Deserialize)]
struct ApiCommitter {
    date: String,
}

pub struct CheckResult {
    pub update_available: bool,
    pub remote: RemoteCommit,
}

/// Whether `remote_sha` is actually a different commit than the one this
/// binary was built from — a prefix check (not exact equality), since
/// `built_from_hash` is a short (12-char) hash and GitHub's API always
/// returns the full 40-char one. `"unknown"` (no `git` at build time)
/// can't be judged either way, so it's reported as "no update" rather
/// than a false positive on every check.
fn is_update_available(remote_sha: &str, built_from_hash: &str) -> bool {
    built_from_hash != "unknown" && !remote_sha.starts_with(built_from_hash)
}

/// Fetches the latest commit on the repo's default branch. GitHub's API
/// requires a `User-Agent` header on every request (a bare 403 without
/// one), and needs no authentication at this request volume — a manual
/// "check for updates" button, not something polling in the background.
async fn fetch_latest_commit() -> Result<RemoteCommit> {
    let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/commits/{REPO_BRANCH}");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", "maraetai-tui-mac")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("requesting the latest commit from GitHub")?
        .error_for_status()
        .context("GitHub returned an error status")?;
    let parsed: ApiCommitResponse = resp.json().await.context("parsing GitHub's response")?;
    let summary = parsed.commit.message.lines().next().unwrap_or_default().to_string();
    Ok(RemoteCommit { sha: parsed.sha, date: parsed.commit.committer.date, summary })
}

pub async fn check_for_update() -> Result<CheckResult> {
    let remote = fetch_latest_commit().await?;
    let update_available = is_update_available(&remote.sha, BUILT_FROM_HASH);
    Ok(CheckResult { update_available, remote })
}

/// Re-installs both binaries straight from GitHub. `cargo install --git`
/// clones (or reuses an existing clone of) the repo into cargo's own git
/// cache, so this works regardless of whether the user still has — or
/// ever had — a local checkout on this machine; `cargo install --path`
/// alone can't do that, since it keeps no link back to any repository
/// once installed. `--force` makes this idempotent rather than relying on
/// cargo's own "is this already installed" heuristic, which isn't
/// guaranteed to notice a new commit at the same crate version.
pub async fn apply_update() -> Result<()> {
    let repo_url = format!("https://github.com/{REPO_OWNER}/{REPO_NAME}.git");
    for package in ["maraetai-daemon", "maraetai-tui"] {
        let output = tokio::process::Command::new("cargo")
            .args(["install", "--git", &repo_url, package, "--locked", "--force"])
            .output()
            .await
            .with_context(|| format!("running cargo install for {package}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Only the last few lines — a failed compile can produce a very
            // long stderr, most of which isn't useful in a status message.
            let tail: Vec<&str> = stderr.lines().rev().take(6).collect();
            let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
            bail!("{package} failed to install:\n{tail}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_available_when_remote_sha_does_not_start_with_the_built_from_hash() {
        assert!(is_update_available("abcdef1234567890", "000000000000"));
    }

    #[test]
    fn no_update_when_remote_sha_starts_with_the_built_from_hash() {
        assert!(!is_update_available("abcdef123456fedcba0987", "abcdef123456"));
    }

    #[test]
    fn no_update_reported_when_built_from_hash_is_unknown() {
        // Can't judge staleness without knowing what we were built from —
        // report "nothing to say" rather than a false positive on every
        // single check.
        assert!(!is_update_available("abcdef123456", "unknown"));
    }
}
