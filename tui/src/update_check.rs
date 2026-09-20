//! Checks GitHub for a newer commit than this binary was built from, and
//! can apply an update by re-running `cargo install --git` for both
//! binaries — the Settings tab's (`8`) whole reason to exist.
//!
//! "Newer" is judged by commit, not a version number: this project
//! doesn't tag releases, so the meaningful comparison is "is there a
//! commit on the remote's default branch that isn't the one this binary
//! was built from" (see `build.rs` for how that gets embedded at compile
//! time).

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

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
///
/// `on_progress` is called with each line of `cargo`'s own build output
/// (compiling/downloading/etc. — cargo writes all of this to stderr, not
/// stdout) as it's produced, not just once at the end — so the caller can
/// show real, live progress instead of a static "please wait". Streamed
/// rather than collected via `Command::output()` (which only hands back
/// everything at once, after the process has already exited).
pub async fn apply_update(mut on_progress: impl FnMut(String) + Send + 'static) -> Result<()> {
    let repo_url = format!("https://github.com/{REPO_OWNER}/{REPO_NAME}.git");
    let cargo = cargo_binary_path();
    for package in ["maraetai-daemon", "maraetai-tui"] {
        on_progress(format!("── installing {package} ──"));
        let mut child = tokio::process::Command::new(&cargo)
            .args(["install", "--git", &repo_url, package, "--locked", "--force"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("could not start {} for {package}: {e}", cargo.display()))?;

        let stderr = child.stderr.take().expect("stderr was piped above");
        let mut lines = BufReader::new(stderr).lines();
        // Kept alongside the live callback so a failure can still report
        // "here's what went wrong" even though the individual lines were
        // already streamed out (and, in the TUI, likely scrolled off the
        // small on-screen log by the time the process actually exits).
        let mut tail: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            on_progress(line.clone());
            tail.push(line);
            if tail.len() > 6 {
                tail.remove(0);
            }
        }

        let status = child.wait().await.with_context(|| format!("waiting for cargo install {package}"))?;
        if !status.success() {
            bail!("{package} failed to install:\n{}", tail.join("\n"));
        }
    }
    Ok(())
}

/// Finds Cargo without assuming the process inherited an interactive
/// shell's PATH. A rustup installation normally puts `cargo`, `maraetai`,
/// and `maraetaid` beside one another in `~/.cargo/bin`, so the sibling
/// lookup covers a TUI launched from a shell, Finder, or another process
/// with a restricted environment. `CARGO` remains an explicit override.
fn cargo_binary_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(mut exe) = std::env::current_exe() {
        exe.set_file_name("cargo");
        if exe.is_file() {
            return exe;
        }
    }
    PathBuf::from("cargo")
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
