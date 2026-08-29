//! Local, per-repo session log for commits made by `commitor commit`.
//!
//! Each successful `commitor commit` run appends one JSON line (a
//! "session") to `~/.commitor/history/<repo-id>.jsonl`. The repo id is a
//! stable hash of the repo's remote URL (or, failing that, its root path),
//! so history never leaks across unrelated repos on the same machine.
//!
//! The log is append-only for new sessions. Marking a session reverted is
//! the one case that rewrites the file, and it does so atomically (write a
//! temp file, then rename over the original) so a crash mid-write can't
//! corrupt the whole history.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::engine::git;

/// One commit produced by a session, enough to show and to revert.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionCommit {
    pub sha: String,
    pub message: String,
    pub files: Vec<String>,
}

/// A single `commitor commit` run.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Session {
    pub session_id: String,
    /// ISO-8601 UTC timestamp of when the session was recorded.
    pub timestamp: String,
    /// Branch the commits landed on (`None` in a detached HEAD).
    #[serde(default)]
    pub branch: Option<String>,
    /// Branch the new branch was forked from, only for `commit -b` runs.
    #[serde(default)]
    pub base_branch: Option<String>,
    pub commits: Vec<SessionCommit>,
    /// Whether any commit in the session had been pushed at record time.
    /// Out of scope to detect live, so this is always `false` here and
    /// `revert` re-checks the remote at run time.
    #[serde(default)]
    pub pushed: bool,
    /// Set once `revert` has undone this session.
    #[serde(default)]
    pub reverted: bool,
    /// ISO-8601 UTC timestamp of the revert, when `reverted` is true.
    #[serde(default)]
    pub reverted_at: Option<String>,
}

/// Stable, filesystem-safe identifier for the current repository, derived
/// from its remote URL (preferred) or root path (fallback).
pub fn repo_id() -> Result<String> {
    let key = match git::remote_url()? {
        Some(remote) if !remote.trim().is_empty() => remote,
        _ => git::repo_toplevel()?,
    };

    // FNV-1a 64-bit: cheap, dependency-free, and stable across runs of
    // the same binary. We only need a well-distributed, non-reversible
    // handle for the on-disk filename, not cryptographic strength.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in key.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok(format!("{hash:016x}"))
}

/// `~/.commitor/history`.
fn history_dir() -> Result<PathBuf> {
    Ok(config::commitor_dir()?.join("history"))
}

/// `<history-dir>/<repo-id>.jsonl`.
fn history_path(id: &str) -> Result<PathBuf> {
    Ok(history_dir()?.join(format!("{id}.jsonl")))
}

/// Build a fresh session id from the current time and the session's first
/// commit sha — unique and stable per session without extra dependencies.
pub fn new_session_id(first_sha: &str) -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    format!("{nanos:x}-{first_sha}", nanos = nanos, first_sha = &first_sha[..8])
}

/// Current UTC time as an ISO-8601 string (e.g. `2025-01-02T03:04:05Z`).
pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Append a new session to the repo's history log.
pub fn record_session(session: &Session) -> Result<()> {
    let dir = history_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }

    let path = history_path(&repo_id()?)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let line = serde_json::to_string(session).context("failed to serialize session")?;
    writeln!(file, "{line}").context("failed to write session to history")?;
    Ok(())
}

/// Load every recorded session for the current repo, oldest first.
pub fn load_sessions() -> Result<Vec<Session>> {
    let path = history_path(&repo_id()?)?;
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read {}", path.display()))
        }
    };

    let reader = BufReader::new(file);
    let mut sessions = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Session>(line) {
            Ok(session) => sessions.push(session),
            Err(err) => {
                eprintln!(
                    "warning: ignoring malformed history line {} in {}: {err}",
                    index + 1,
                    path.display()
                );
            }
        }
    }
    Ok(sessions)
}

/// Record that a session has been reverted. Rewrites the whole file
/// atomically; a no-op (beyond a warning-free early return) if the id is
/// not present, since the caller surfaces that as its own error.
pub fn mark_reverted(session_id: &str) -> Result<()> {
    let mut sessions = load_sessions()?;
    let now = now_iso();
    let mut found = false;
    for session in sessions.iter_mut() {
        if session.session_id == session_id {
            session.reverted = true;
            session.reverted_at = Some(now);
            found = true;
            break;
        }
    }
    if !found {
        return Ok(());
    }

    let path = history_path(&repo_id()?)?;
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        for session in &sessions {
            let line = serde_json::to_string(session).context("failed to serialize session")?;
            writeln!(file, "{line}").context("failed to write session to history")?;
        }
        file.flush().context("failed to flush history")?;
    }
    fs::rename(&tmp, &path).with_context(|| {
        format!("failed to replace {} with updated history", path.display())
    })?;
    Ok(())
}
