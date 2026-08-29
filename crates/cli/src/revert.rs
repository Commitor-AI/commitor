//! `commitor revert` — undo commits made by a previous `commitor commit`.
//!
//! Every `commitor commit` writes a session to a local per-repo log
//! (see [`crate::engine::history`]). `revert` reads that log, lets the
//! user pick a session (most recent by default, or `--list` to choose an
//! older one), and then undoes it one of two ways:
//!
//! * **hard reset** — when every commit in the session is still local-only
//!   (not pushed). This cleanly erases the commits with no history
//!   pollution, since nothing shared depends on them yet.
//! * **revert commits** — when any commit has been pushed. Rewriting shared
//!   history is unsafe for anyone who has already pulled, so we create
//!   proper revert commits instead (newest-to-oldest).
//!
//! Both paths require an explicit `y/N` confirmation, and the command
//! refuses to run with a dirty working tree unless `--force` is given.

use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

use crate::engine::git;
use crate::engine::history::{self, Session};

#[derive(Debug, Default)]
pub struct RevertFlags {
    /// Show the last N sessions instead of acting on the most recent.
    pub list: bool,
    /// Skip the working-tree-dirty safety check.
    pub force: bool,
    /// Target a specific session by id, or any commit sha it contains.
    pub session: Option<String>,
    /// How many sessions `--list` should show (default 10).
    pub limit: Option<usize>,
}

pub fn run(flags: RevertFlags) -> Result<ExitCode> {
    let mut sessions = history::load_sessions()?;
    if sessions.is_empty() {
        println!("No commitor commit history found for this repo");
        return Ok(ExitCode::SUCCESS);
    }
    // Newest first for display and default targeting.
    sessions.reverse();

    if flags.list {
        return list_sessions(&sessions, flags.limit.unwrap_or(10));
    }

    // Pick the target session.
    let target = match &flags.session {
        Some(id) => find_session(&sessions, id)?,
        None => &sessions[0],
    };

    if target.reverted {
        println!(
            "Session {} was already reverted{} — nothing to do.",
            target.session_id,
            target
                .reverted_at
                .as_deref()
                .map(|t| format!(" at {t}"))
                .unwrap_or_default()
        );
        return Ok(ExitCode::SUCCESS);
    }

    // Never silently discard uncommitted work.
    if !flags.force && git::has_uncommitted_changes()? {
        bail!(
            "Your working tree has uncommitted changes, so `commitor revert` could destroy or \
             clash with that work (a hard reset would discard it entirely).\n\
             Commit or stash your changes first, or pass --force to proceed anyway."
        );
    }

    // Show what the session contained.
    show_session(target);

    // Decide reset vs. revert by checking each commit against the remote.
    let mut any_pushed = false;
    let mut any_local = false;
    for commit in &target.commits {
        let pushed = git::check_pushed(&commit.sha)
            .with_context(|| format!("couldn't check whether {} is pushed", commit.sha))?;
        if pushed {
            any_pushed = true;
        } else {
            any_local = true;
        }
    }

    // A mix within one session is treated as "pushed" for safety: revert,
    // don't reset, and say so.
    let strategy = if any_pushed {
        Strategy::Revert
    } else {
        Strategy::Reset
    };

    let reset_target = if let Strategy::Reset = strategy {
        Some(
            git::parent_of(&target.commits[0].sha).with_context(|| {
                "the first commit in this session is the repository's root commit — there is \
                 nothing before it to reset back to. Revert it manually with \
                 `git revert <sha>`, or reset the branch ref yourself."
            })?,
        )
    } else {
        None
    };

    // Preview what will happen.
    println!();
    match &strategy {
        Strategy::Reset => {
            let reset_ref = reset_target.as_deref().unwrap();
            println!(
                "This will DISCARD the {} commit(s) in this session with a hard reset to {}",
                target.commits.len(),
                &reset_ref[..7.min(reset_ref.len())]
            );
            println!(
                "Warning: the commits will be gone, not just unstaged. This cannot be undone \
                 for commits that have no other ref pointing at them."
            );
        }
        Strategy::Revert => {
            println!(
                "This will create {} new revert commit(s) for the session's commits \
                 (newest first).",
                target.commits.len()
            );
            if any_local {
                println!(
                    "Some commits were already pushed, so a reset would rewrite shared \
                     history — `git revert` is used instead to stay safe."
                );
            } else {
                println!("At least one commit is already on a remote; rewriting history is unsafe.");
            }
        }
    }

    if !confirm("Proceed with the revert? [y/N] ")? {
        println!("Aborted — nothing was changed.");
        return Ok(ExitCode::SUCCESS);
    }

    // Execute.
    match &strategy {
        Strategy::Reset => {
            let target = reset_target.unwrap();
            println!("Resetting to {}…", &target[..7.min(target.len())]);
            git::reset_hard(&target)?;
        }
        Strategy::Revert => {
            // Newest-to-oldest so the original ordering is undone cleanly.
            for commit in target.commits.iter().rev() {
                let summary = commit.message.lines().next().unwrap_or("");
                println!("Reverting {} — {summary}", &commit.sha[..7.min(commit.sha.len())]);
                git::revert_commit(&commit.sha)?;
            }
        }
    }

    history::mark_reverted(&target.session_id)?;
    println!("Marked session {} as reverted.", target.session_id);
    Ok(ExitCode::SUCCESS)
}

/// Strategy for undoing a session.
enum Strategy {
    /// `git reset --hard <parent-of-first-commit>` — erases local commits.
    Reset,
    /// `git revert` each commit — safe for pushed/shared commits.
    Revert,
}

/// Print the most recent `limit` sessions as a pick list.
fn list_sessions(sessions: &[Session], limit: usize) -> Result<ExitCode> {
    let shown: Vec<&Session> = sessions.iter().take(limit).collect();
    println!("Recent commitor sessions (newest first):\n");
    for (i, session) in shown.iter().enumerate() {
        let branch = session
            .branch
            .as_deref()
            .map(|b| format!(" on {b}"))
            .unwrap_or_default();
        let reverted = if session.reverted { " (reverted)" } else { "" };
        println!(
            "{}. {} — {} commit(s){} at {}{}",
            i + 1,
            session.session_id,
            session.commits.len(),
            branch,
            session.timestamp,
            reverted
        );
        if let Some(first) = session.commits.first() {
            let summary = first.message.lines().next().unwrap_or("");
            println!(
                "   first commit: {} {} (use `commitor revert {}` to target this session)",
                &first.sha[..7.min(first.sha.len())],
                summary,
                session.session_id
            );
        }
    }
    println!("\nPass `commitor revert <session_id>` to undo a specific session.");
    Ok(ExitCode::SUCCESS)
}

/// Print a single session's commits.
fn show_session(session: &Session) {
    println!(
        "Session {} — {} commit(s), recorded at {}",
        session.session_id,
        session.commits.len(),
        session.timestamp
    );
    if let Some(branch) = &session.branch {
        println!("Branch: {branch}");
    } else {
        println!("Branch: (detached HEAD)");
    }
    if let Some(base) = &session.base_branch {
        println!("Forked from: {base}");
    }
    for commit in &session.commits {
        let summary = commit.message.lines().next().unwrap_or("");
        println!(
            "  {} {}",
            &commit.sha[..7.min(commit.sha.len())],
            summary
        );
        if !commit.files.is_empty() {
            println!("    files: {}", commit.files.join(", "));
        }
    }
}

/// Find a session by its id, or by any commit sha it contains.
fn find_session<'a>(sessions: &'a [Session], id: &str) -> Result<&'a Session> {
    let by_id = sessions.iter().find(|s| s.session_id == id);
    let by_sha = sessions
        .iter()
        .find(|s| s.commits.iter().any(|c| c.sha == id || c.sha.starts_with(id)));
    by_id
        .or(by_sha)
        .with_context(|| format!("no commitor session matches `{id}` in this repo's history"))
}

/// Prompt for a yes/no answer; only `y`/`yes` (case-insensitive) returns
/// true, and the default (empty input) is No.
fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush().context("failed to write prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read your answer")?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}
