use std::process::Command;

use anyhow::{Context, Result};

/// Run `git diff --staged` and combine with `git diff` (unstaged).
/// If `staged_only` is true, only staged changes are returned.
pub fn get_diff(staged_only: bool) -> Result<String> {
    let mut parts = Vec::new();

    let staged = run_git(&["diff", "--staged"])?;
    if !staged.is_empty() {
        parts.push(staged);
    }

    if !staged_only {
        let unstaged = run_git(&["diff"])?;
        if !unstaged.is_empty() {
            parts.push(unstaged);
        }
    }

    Ok(parts.join("\n"))
}

/// Run `git diff {base}...HEAD` to get the diff between a base branch
/// and the current HEAD. Used by the PR command.
pub fn get_branch_diff(base: &str) -> Result<String> {
    run_git(&["diff", &format!("{base}...HEAD")])
}

fn run_git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .context("failed to execute `git` — is git installed and in PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
