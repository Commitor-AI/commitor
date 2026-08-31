//! `commitor changelog` — generate Conventional Commit changelogs from git history.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::git;

#[derive(Debug, Default)]
pub struct ChangelogFlags {
    /// Revision range to analyze (e.g. `v0.1.0..HEAD` or `origin/main..HEAD`)
    pub range: Option<String>,
    /// Number of commits to analyze if no range is specified (default: 20)
    pub limit: Option<usize>,
    /// Emit GitHub-flavored Markdown suited to CHANGELOG.md
    pub markdown: bool,
    /// Print machine-readable JSON output
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitEntry {
    pub hash: String,
    pub commit_type: String,
    pub scope: Option<String>,
    pub summary: String,
    pub is_breaking: bool,
    pub author: String,
    pub date: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ChangelogReport {
    pub range: String,
    pub total_commits: usize,
    pub categories: BTreeMap<String, Vec<CommitEntry>>,
    pub breaking_changes: Vec<CommitEntry>,
}

pub fn run(flags: ChangelogFlags) -> Result<ExitCode> {
    if !git::is_work_tree() {
        bail!("this doesn't look like a git repository — run `commitor changelog` from inside a git repository");
    }

    let commits = fetch_commits(flags.range.as_deref(), flags.limit)?;

    if commits.is_empty() {
        if flags.markdown {
            println!(
                "# Changelog\n\n_No Conventional Commits found in range `{}`._",
                flags.range.as_deref().unwrap_or("HEAD")
            );
        } else if flags.json {
            let empty_report = ChangelogReport {
                range: flags.range.unwrap_or_else(|| "HEAD".into()),
                total_commits: 0,
                categories: BTreeMap::new(),
                breaking_changes: Vec::new(),
            };
            println!("{}", serde_json::to_string_pretty(&empty_report)?);
        } else {
            println!("No Conventional Commits found.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut categories: BTreeMap<String, Vec<CommitEntry>> = BTreeMap::new();
    let mut breaking_changes: Vec<CommitEntry> = Vec::new();

    for commit in &commits {
        if commit.is_breaking {
            breaking_changes.push(commit.clone());
        }

        let cat_name = match commit.commit_type.as_str() {
            "feat" => "Features",
            "fix" => "Bug Fixes",
            "docs" => "Documentation",
            "refactor" => "Refactoring",
            "perf" => "Performance Improvements",
            "test" => "Tests",
            "style" => "Styles",
            "chore" => "Chores & Maintenance",
            "ci" => "Continuous Integration",
            "build" => "Build System",
            _ => "Other Changes",
        };

        categories
            .entry(cat_name.to_string())
            .or_default()
            .push(commit.clone());
    }

    let report = ChangelogReport {
        range: flags
            .range
            .clone()
            .unwrap_or_else(|| format!("Last {} commits", commits.len())),
        total_commits: commits.len(),
        categories,
        breaking_changes,
    };

    if flags.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if flags.markdown {
        render_markdown(&report);
    } else {
        render_terminal(&report);
    }

    Ok(ExitCode::SUCCESS)
}

fn fetch_commits(range: Option<&str>, limit: Option<usize>) -> Result<Vec<CommitEntry>> {
    let mut args = vec!["log", "--pretty=format:%h|%an|%ad|%s", "--date=short"];
    
    let limit_str;
    if let Some(r) = range {
        args.push(r);
    } else {
        let count = limit.unwrap_or(20);
        limit_str = format!("-n{count}");
        args.push(&limit_str);
    }

    let output = Command::new("git")
        .args(&args)
        .output()
        .context("failed to execute git log — is git installed and in PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git log failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(4, '|').collect();
        if parts.len() < 4 {
            continue;
        }

        let hash = parts[0].trim().to_string();
        let author = parts[1].trim().to_string();
        let date = parts[2].trim().to_string();
        let raw_subject = parts[3].trim();

        if let Some(entry) = parse_conventional(hash, author, date, raw_subject) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

pub fn parse_conventional(
    hash: String,
    author: String,
    date: String,
    subject: &str,
) -> Option<CommitEntry> {
    let is_breaking_text = subject.contains("BREAKING CHANGE");
    let colon_pos = subject.find(':')?;
    let header_part = subject[..colon_pos].trim();
    let summary = subject[colon_pos + 1..].trim().to_string();

    let (type_scope, is_breaking_mark) = if header_part.ends_with('!') {
        (&header_part[..header_part.len() - 1], true)
    } else {
        (header_part, is_breaking_text)
    };

    let (commit_type, scope) = if let Some(open_paren) = type_scope.find('(') {
        let close_paren = type_scope.find(')')?;
        if close_paren <= open_paren {
            return None;
        }
        let c_type = type_scope[..open_paren].trim().to_lowercase();
        let c_scope = type_scope[open_paren + 1..close_paren].trim().to_string();
        (c_type, Some(c_scope))
    } else {
        (type_scope.to_lowercase(), None)
    };

    if commit_type.is_empty() || summary.is_empty() {
        return None;
    }

    Some(CommitEntry {
        hash,
        commit_type,
        scope,
        summary,
        is_breaking: is_breaking_mark,
        author,
        date,
    })
}

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn bold(text: &str) -> String {
    if use_color() {
        format!("\x1b[1m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn green(text: &str) -> String {
    if use_color() {
        format!("\x1b[32m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn yellow(text: &str) -> String {
    if use_color() {
        format!("\x1b[33m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn render_terminal(report: &ChangelogReport) {
    println!("{}", bold(&format!("📋 Commitor Changelog ({})", report.range)));
    println!("Total Conventional Commits: {}\n", report.total_commits);

    if !report.breaking_changes.is_empty() {
        println!("{}", yellow("🚨 BREAKING CHANGES:"));
        for entry in &report.breaking_changes {
            let scope_str = entry
                .scope
                .as_ref()
                .map(|s| format!("({s})"))
                .unwrap_or_default();
            println!(
                "  • {}{}: {} [{}]",
                entry.commit_type, scope_str, entry.summary, entry.hash
            );
        }
        println!();
    }

    for (category, entries) in &report.categories {
        println!("{}", green(&format!("### {category}")));
        for entry in entries {
            let scope_str = entry
                .scope
                .as_ref()
                .map(|s| format!("({s})"))
                .unwrap_or_default();
            println!(
                "  • {}{}: {} ({})",
                entry.commit_type, scope_str, entry.summary, entry.hash
            );
        }
        println!();
    }
}

fn render_markdown(report: &ChangelogReport) {
    println!("# Changelog ({})\n", report.range);

    if !report.breaking_changes.is_empty() {
        println!("## 🚨 BREAKING CHANGES\n");
        for entry in &report.breaking_changes {
            let scope_str = entry
                .scope
                .as_ref()
                .map(|s| format!("**{s}**: "))
                .unwrap_or_default();
            println!("- {scope_str}{} (`{}`)", entry.summary, entry.hash);
        }
        println!();
    }

    for (category, entries) in &report.categories {
        println!("## {category}\n");
        for entry in entries {
            let scope_str = entry
                .scope
                .as_ref()
                .map(|s| format!("**{s}**: "))
                .unwrap_or_default();
            println!("- {scope_str}{} (`{}`)", entry.summary, entry.hash);
        }
        println!();
    }

    println!("---");
    println!("_Generated by [Commitor](https://github.com/Commitor-AI/commitor)_");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_conventional_commit() {
        let entry = parse_conventional(
            "abc1234".into(),
            "Alice".into(),
            "2026-08-30".into(),
            "feat: add user authentication flow",
        )
        .unwrap();

        assert_eq!(entry.commit_type, "feat");
        assert_eq!(entry.scope, None);
        assert_eq!(entry.summary, "add user authentication flow");
        assert!(!entry.is_breaking);
    }

    #[test]
    fn parses_scoped_conventional_commit() {
        let entry = parse_conventional(
            "def5678".into(),
            "Bob".into(),
            "2026-08-30".into(),
            "fix(cli): resolve race condition in scan",
        )
        .unwrap();

        assert_eq!(entry.commit_type, "fix");
        assert_eq!(entry.scope, Some("cli".into()));
        assert_eq!(entry.summary, "resolve race condition in scan");
        assert!(!entry.is_breaking);
    }

    #[test]
    fn parses_breaking_change_commit() {
        let entry = parse_conventional(
            "9998887".into(),
            "Charlie".into(),
            "2026-08-30".into(),
            "feat(api)!: breaking API endpoint restructuring",
        )
        .unwrap();

        assert_eq!(entry.commit_type, "feat");
        assert_eq!(entry.scope, Some("api".into()));
        assert_eq!(entry.summary, "breaking API endpoint restructuring");
        assert!(entry.is_breaking);
    }

    #[test]
    fn ignores_non_conventional_commit() {
        let entry = parse_conventional(
            "1112223".into(),
            "David".into(),
            "2026-08-30".into(),
            "updated README and fixed typos",
        );

        assert!(entry.is_none());
    }
}
