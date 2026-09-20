//! Embeds the git commit this binary was built from, so the Settings
//! screen (`update_check.rs`) has something to compare against GitHub's
//! latest commit. Works identically whether built from a plain local
//! checkout (`cargo install --path tui`) or `cargo install --git
//! <url>` (which clones into `~/.cargo/git/checkouts/...` — a real git
//! repo either way, just found by running `git` from this crate's own
//! directory and letting it search upward for `.git`).
//!
//! Falls back to `"unknown"`/empty rather than failing the build if `git`
//! isn't on `PATH` or this somehow isn't a git checkout at all (e.g. a
//! tarball with no `.git`) — the Settings screen already treats an
//! unrecognized local version as "can't tell, go check manually".

use std::process::Command;

fn main() {
    let hash = git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MARAETAI_GIT_HASH={hash}");

    let date = git_output(&["log", "-1", "--format=%cs"]).unwrap_or_default();
    println!("cargo:rustc-env=MARAETAI_GIT_DATE={date}");

    // Re-run this script (so the embedded hash/date stay current) whenever
    // HEAD or the branch it points to changes — not on every build
    // regardless of source changes, which would be needlessly slow.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}
