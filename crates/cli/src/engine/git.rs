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

/// The full patch text of one diff flavor: staged when `staged`,
/// unstaged (working tree) otherwise.
pub fn diff_patch(staged: bool) -> Result<String> {
    if staged {
        run_git(&["diff", "--staged"])
    } else {
        run_git(&["diff"])
    }
}

/// Changed file paths for one diff flavor, one per line.
pub fn changed_files(staged: bool) -> Result<Vec<String>> {
    let out = if staged {
        run_git(&["diff", "--staged", "--name-only"])?
    } else {
        run_git(&["diff", "--name-only"])?
    };
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
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
