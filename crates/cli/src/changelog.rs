//! `commitor changelog` — generate Conventional Commit changelogs from git history.

use std::io::IsTerminal;
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::git;

const KNOWN_TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "chore", "ci", "build",
];

const CATEGORY_ORDER: &[(&str, &str)] = &[
    ("feat", "Features"),
    ("fix", "Bug Fixes"),
    ("perf", "Performance Improvements"),
    ("refactor", "Refactoring"),
    ("docs", "Documentation"),
    ("build", "Build System"),
    ("ci", "Continuous Integration"),
    ("test", "Tests"),
    ("style", "Styles"),
    ("chore", "Chores & Maintenance"),
];

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
    /// Include chore commits whose summary starts with "release v"
    pub include_release_chores: bool,
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
    pub total_scanned: usize,
    pub conventional_count: usize,
    pub excluded_release_chores: usize,
    pub categories: Vec<(String, Vec<CommitEntry>)>,
    pub breaking_changes: Vec<CommitEntry>,
}

pub fn run(flags: ChangelogFlags) -> Result<ExitCode> {
    if !git::is_work_tree() {
        bail!("this doesn't look like a git repository — run `commitor changelog` from inside a git repository");
    }

    let commits = fetch_commits(flags.range.as_deref(), flags.limit)?;
    let total_scanned = commits.len();

    if commits.is_empty() {
        if flags.markdown {
            println!(
                "# Changelog\n\n_No Conventional Commits found in range `{}`._",
                flags.range.as_deref().unwrap_or("HEAD")
            );
        } else if flags.json {
            let empty_report = ChangelogReport {
                range: flags.range.unwrap_or_else(|| "HEAD".into()),
                total_scanned: 0,
                conventional_count: 0,
                excluded_release_chores: 0,
                categories: Vec::new(),
                breaking_changes: Vec::new(),
            };
            println!("{}", serde_json::to_string_pretty(&empty_report)?);
        } else {
            println!("No Conventional Commits found.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut conventional: Vec<CommitEntry> = Vec::new();
    let mut excluded_release_chores: usize = 0;

    for commit in commits {
        if commit.commit_type == "chore"
            && commit
                .summary
                .to_lowercase()
                .starts_with("release v")
            && !flags.include_release_chores
        {
            excluded_release_chores += 1;
            continue;
        }
        conventional.push(commit);
    }

    let conventional_count = conventional.len();

    let mut breaking_changes: Vec<CommitEntry> = Vec::new();
    let mut buckets: Vec<(String, Vec<CommitEntry>)> = CATEGORY_ORDER
        .iter()
        .map(|(_, label)| (label.to_string(), Vec::new()))
        .collect();

    for commit in &conventional {
        if commit.is_breaking {
            breaking_changes.push(commit.clone());
        }

        if let Some(pos) = CATEGORY_ORDER
            .iter()
            .position(|(ty, _)| *ty == commit.commit_type.as_str())
        {
            buckets[pos].1.push(commit.clone());
        }
    }

    let categories: Vec<(String, Vec<CommitEntry>)> = buckets
        .into_iter()
        .filter(|(_, entries)| !entries.is_empty())
        .collect();

    let report = ChangelogReport {
        range: flags
            .range
            .clone()
            .unwrap_or_else(|| format!("Last {total_scanned} commits")),
        total_scanned,
        conventional_count,
        excluded_release_chores,
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

    if !KNOWN_TYPES.contains(&commit_type.as_str()) {
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
    println!("{}", bold(&format!("Commitor Changelog ({})", report.range)));
    println!(
        "Scanned {} commits \u{00b7} {} conventional \u{00b7} {} release chores excluded\n",
        report.total_scanned, report.conventional_count, report.excluded_release_chores
    );

    if !report.breaking_changes.is_empty() {
        println!("{}", yellow("BREAKING CHANGES:"));
        for entry in &report.breaking_changes {
            let scope_str = entry
                .scope
                .as_ref()
                .map(|s| format!("({s})"))
                .unwrap_or_default();
            println!(
                "  \u{2022} {}{}: {} [{}]",
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
                "  \u{2022} {}{}: {} ({})",
                entry.commit_type, scope_str, entry.summary, entry.hash
            );
        }
        println!();
    }
}

fn render_markdown(report: &ChangelogReport) {
    println!("# Changelog ({})\n", report.range);
    println!(
        "_Scanned {} commits \u{00b7} {} conventional \u{00b7} {} release chores excluded_\n",
        report.total_scanned, report.conventional_count, report.excluded_release_chores
    );

    if !report.breaking_changes.is_empty() {
        println!("## BREAKING CHANGES\n");
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

    #[test]
    fn rejects_unknown_commit_type() {
        let entry = parse_conventional(
            "aaa1111".into(),
            "Eve".into(),
            "2026-08-30".into(),
            "modernize cli auth: add tokio-macros",
        );

        assert!(entry.is_none());
    }

    #[test]
    fn release_chore_is_excluded_by_default() {
        let flags = ChangelogFlags {
            include_release_chores: false,
            ..Default::default()
        };

        let entry = CommitEntry {
            hash: "aaa2222".into(),
            commit_type: "chore".into(),
            scope: None,
            summary: "release v1.2.0".into(),
            is_breaking: false,
            author: "Alice".into(),
            date: "2026-08-30".into(),
        };

        let commits = vec![entry];
        let mut conventional: Vec<CommitEntry> = Vec::new();
        let mut excluded = 0usize;

        for commit in commits {
            if commit.commit_type == "chore"
                && commit
                    .summary
                    .to_lowercase()
                    .starts_with("release v")
                && !flags.include_release_chores
            {
                excluded += 1;
                continue;
            }
            conventional.push(commit);
        }

        assert_eq!(excluded, 1);
        assert!(conventional.is_empty());
    }

    #[test]
    fn release_chore_included_when_flag_set() {
        let flags = ChangelogFlags {
            include_release_chores: true,
            ..Default::default()
        };

        let entry = CommitEntry {
            hash: "aaa3333".into(),
            commit_type: "chore".into(),
            scope: None,
            summary: "release v1.2.0".into(),
            is_breaking: false,
            author: "Alice".into(),
            date: "2026-08-30".into(),
        };

        let commits = vec![entry];
        let mut conventional: Vec<CommitEntry> = Vec::new();
        let mut excluded = 0usize;

        for commit in commits {
            if commit.commit_type == "chore"
                && commit
                    .summary
                    .to_lowercase()
                    .starts_with("release v")
                && !flags.include_release_chores
            {
                excluded += 1;
                continue;
            }
            conventional.push(commit);
        }

        assert_eq!(excluded, 0);
        assert_eq!(conventional.len(), 1);
    }

    #[test]
    fn categories_follow_fixed_order() {
        let entries: Vec<CommitEntry> = vec![
            CommitEntry {
                hash: "c1".into(),
                commit_type: "chore".into(),
                scope: None,
                summary: "clean up".into(),
                is_breaking: false,
                author: "A".into(),
                date: "2026-08-30".into(),
            },
            CommitEntry {
                hash: "c2".into(),
                commit_type: "feat".into(),
                scope: None,
                summary: "add widget".into(),
                is_breaking: false,
                author: "A".into(),
                date: "2026-08-30".into(),
            },
            CommitEntry {
                hash: "c3".into(),
                commit_type: "fix".into(),
                scope: None,
                summary: "patch bug".into(),
                is_breaking: false,
                author: "A".into(),
                date: "2026-08-30".into(),
            },
        ];

        let mut buckets: Vec<(String, Vec<CommitEntry>)> = CATEGORY_ORDER
            .iter()
            .map(|(_, label)| (label.to_string(), Vec::new()))
            .collect();

        for commit in &entries {
            if let Some(pos) = CATEGORY_ORDER
                .iter()
                .position(|(ty, _)| *ty == commit.commit_type.as_str())
            {
                buckets[pos].1.push(commit.clone());
            }
        }

        let categories: Vec<(String, Vec<CommitEntry>)> = buckets
            .into_iter()
            .filter(|(_, entries)| !entries.is_empty())
            .collect();

        let labels: Vec<&str> = categories.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Features", "Bug Fixes", "Chores & Maintenance"]
        );
    }
}
