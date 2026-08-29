use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// Run `git diff --staged` and combine with `git diff` (unstaged).
/// If `staged_only` is true, only staged changes are returned.
// Unwired engine helpers kept for upcoming commands (`pr`); silence
// until then.
#[allow(dead_code)]
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
#[allow(dead_code)]
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

/// Stage exactly the given paths (`git add -- <paths>`).
///
/// The `--` separator keeps paths starting with `-` from being read
/// as options. Safe to call on already-staged paths; also stages
/// deletions (git 2.x `add` semantics).
pub fn add(paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        bail!("no files to stage — the analysis returned a group without files");
    }

    let mut args: Vec<&str> = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    run_git(&args).map(drop)
}

/// Create a commit with the given message (`git commit -m`).
///
/// Returns git's own summary output on success. Hooks are NOT
/// bypassed: a rejecting hook surfaces here as an error so the
/// caller can report it and stop.
///
// NOTE for future work: this is file-level splitting only
// (`git add <file>`), not hunk-level (`git add -p`) splitting. When a
// single file itself contains two unrelated changes, those changes
// stay glued together — hunk-level splitting is a reasonable v2
// improvement, but out of scope.
pub fn commit(message: &str) -> Result<String> {
    if message.trim().is_empty() {
        bail!("refusing to create a commit with an empty message");
    }

    let output = Command::new("git")
        .args(["commit", "-m", message])
        .output()
        .context("failed to execute `git` — is git installed and in PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git rejected the commit: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Machine-readable working-tree status (tracked changes only).
///
/// `-uno` excludes untracked files: they never appear in `git diff`,
/// so they can't affect an analysis plan, and counting them would
/// make the stale-plan check fire on unrelated noise (editor swap
/// files, build artifacts, ...).
pub fn status_porcelain() -> Result<String> {
    run_git(&["status", "--porcelain", "-uno"])
}

/// Untracked file paths (`git ls-files --others
/// --exclude-standard`). Respects `.gitignore`, so ignored build
/// artifacts never leak into an analysis.
pub fn untracked_files() -> Result<Vec<String>> {
    let out = run_git(&["ls-files", "--others", "--exclude-standard"])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Stage part of a diff by piping a patch into `git apply --cached`.
///
/// Used for hunk-level staging, where whole-file `git add` would
/// overstage. The patch is applied against the current index, which
/// callers are responsible for having put into the expected state.
pub fn apply_cached(patch: &str) -> Result<()> {
    if patch.trim().is_empty() {
        bail!("no changes to stage — the plan produced an empty patch");
    }

    let mut child = Command::new("git")
        .args(["apply", "--cached", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to execute `git apply` — is git installed and in PATH?")?;

    // Scope the stdin handle so it is closed before we wait; leaving
    // it open would deadlock git waiting for EOF on the patch.
    {
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open stdin for `git apply`")?;
        stdin
            .write_all(patch.as_bytes())
            .context("failed to pipe the patch to `git apply`")?;
    } // dropped → closed

    let output = child
        .wait_with_output()
        .context("failed to wait for `git apply`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git rejected staging: {}", stderr.trim());
    }
    Ok(())
}

/// Mixed reset (`git reset -q`): point the index back at HEAD while
/// keeping the working tree intact. Used before staged-flavor splits
/// so hunk patches apply against a clean index instead of one that
/// already contains every change.
pub fn reset_index() -> Result<()> {
    run_git(&["reset", "-q"]).map(drop)
}

/// All local branch names plus the current one (if any). `current` is
/// `None` in a detached HEAD or a brand-new repository with no commits
/// yet — callers should treat that as "no usable current branch".
pub fn list_branches() -> Result<(Vec<String>, Option<String>)> {
    // `git branch` errors on a repo with no commits, so tolerate that
    // and just report an empty list: the user will create the first
    // branch via the `-b` "new branch" option.
    let branches = match run_git(&["branch", "--format=%(refname:short)"]) {
        Ok(out) => out
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    };

    let current = match run_git(&["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(out) => {
            let name = out.trim().to_string();
            if name.is_empty() || name == "HEAD" {
                None
            } else {
                Some(name)
            }
        }
        Err(_) => None,
    };

    Ok((branches, current))
}

/// Switch to an existing local branch (`git checkout <branch>`).
///
/// Fails (via git) when the working tree has conflicting uncommitted
/// changes — those must be committed or stashed first.
pub fn checkout_branch(branch: &str) -> Result<()> {
    run_git(&["checkout", branch]).map(drop)
}

/// Create a new branch from the current HEAD and switch to it
/// (`git checkout -b <branch>`). Uncommitted changes are carried over
/// to the new branch by git.
pub fn create_branch(branch: &str) -> Result<()> {
    run_git(&["checkout", "-b", branch]).map(drop)
}

/// Full SHA of the current HEAD (`git rev-parse HEAD`).
pub fn head_sha() -> Result<String> {
    run_git(&["rev-parse", "HEAD"]).map(|s| s.trim().to_string())
}

/// Current branch name, or `None` in a detached HEAD / unborn repo.
pub fn current_branch() -> Result<Option<String>> {
    let out = run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let name = out.trim().to_string();
    if name.is_empty() || name == "HEAD" {
        Ok(None)
    } else {
        Ok(Some(name))
    }
}

/// Absolute path of the repo root (`git rev-parse --show-toplevel`).
pub fn repo_toplevel() -> Result<String> {
    run_git(&["rev-parse", "--show-toplevel"]).map(|s| s.trim().to_string())
}

/// The `origin` remote's URL, or `None` if the repo has no `origin` or
/// it isn't configured. Used to derive a stable per-repo history id.
pub fn remote_url() -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .context("failed to execute `git` — is git installed and in PATH?")?;
    if !output.status.success() {
        return Ok(None);
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if url.is_empty() { None } else { Some(url) })
}

/// True when the working tree has tracked, uncommitted changes. Untracked
/// files are intentionally ignored: a hard reset leaves them in place, so
/// they are never at risk from a revert/reset.
pub fn has_uncommitted_changes() -> Result<bool> {
    let out = run_git(&["status", "--porcelain", "-uno"])?;
    Ok(!out.trim().is_empty())
}

/// True when `sha` has been pushed to a remote-tracking branch (i.e. it
/// shows up in `git branch -r --contains <sha>`). Fails clearly when the
/// commit no longer exists — usually a rebase or a `reset --hard` upstream
/// has rewritten history out from under the recorded session.
pub fn check_pushed(sha: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["branch", "-r", "--contains", sha])
        .output()
        .context("failed to execute `git` — is git installed and in PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "commit {} could not be found in this repository — it may have been rewritten \
             or removed by a rebase or a hard reset since `commitor commit` created it. \
             History cannot be reverted safely. ({})",
            sha,
            stderr.trim()
        );
    }
    let out = String::from_utf8_lossy(&output.stdout);
    Ok(!out.trim().is_empty())
}

/// Create a revert commit for `sha` without opening an editor.
pub fn revert_commit(sha: &str) -> Result<()> {
    run_git(&["revert", "--no-edit", sha]).map(drop)
}

/// Move HEAD and the index/working tree to `sha` (`git reset --hard`).
pub fn reset_hard(sha: &str) -> Result<()> {
    run_git(&["reset", "--hard", sha]).map(drop)
}

/// SHA of the parent of `sha` (`git rev-parse <sha>^`). Fails clearly when
/// `sha` is a root commit with no parent — there is nothing before it to
/// reset back to.
pub fn parent_of(sha: &str) -> Result<String> {
    let arg = format!("{sha}^");
    run_git(&["rev-parse", &arg]).map(|s| s.trim().to_string())
}

/// Name of the current branch's upstream tracking ref (e.g. `origin/main`),
/// or `None` when the branch has no upstream configured.
pub fn upstream() -> Option<String> {
    let out = run_git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).ok()?;
    let name = out.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// True when a remote with the given name is configured.
pub fn remote_exists(name: &str) -> bool {
    Command::new("git")
        .args(["remote", "get-url", name])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Push the current branch to its remote. Uses the configured upstream when
/// one exists; otherwise pushes `-u origin <branch>` to set one up. Fails
/// clearly on a detached HEAD or when there is no `origin` remote.
pub fn push_current_branch() -> Result<()> {
    if upstream().is_some() {
        run_git(&["push"]).map(drop)
    } else {
        let branch = current_branch()?
            .context("cannot push: HEAD is detached, so there is no branch to push")?;
        run_git(&["push", "-u", "origin", &branch]).map(drop)
    }
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
