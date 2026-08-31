//! `commitor changelog` — generate Conventional Commit changelogs from git history.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::git;

const KNOWN_TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "chore", "ci", "build", "revert",
];

const CATEGORY_ORDER: &[(&str, &str)] = &[
    ("feat", "Features"),
    ("fix", "Bug Fixes"),
    ("revert", "Reverts"),
    ("perf", "Performance Improvements"),
    ("refactor", "Refactoring"),
    ("docs", "Documentation"),
    ("build", "Build System"),
    ("ci", "Continuous Integration"),
    ("test", "Tests"),
    ("style", "Styles"),
    ("chore", "Chores & Maintenance"),
];

const MARKER: &str = "<!-- commitor:changelog -->";

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
    /// Only include commits whose scope matches this value (case-insensitive)
    pub scope_filter: Option<String>,
    /// Write the changelog to this file instead of stdout
    pub output: Option<PathBuf>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
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

    let (effective_range, used_tag) = resolve_range(flags.range.as_deref());

    let fetch_limit = if effective_range.is_some() {
        None
    } else {
        flags.limit
    };
    let commits = fetch_commits(effective_range.as_deref(), fetch_limit)?;
    let total_scanned = commits.len();

    if commits.is_empty() {
        let range_label = effective_range.as_deref().unwrap_or("HEAD");
        if flags.markdown || flags.output.is_some() {
            let md = render_markdown_empty(range_label);
            if let Some(path) = &flags.output {
                write_changelog_file(path, &md)?;
                println!("Wrote changelog to {}", path.display());
            } else {
                print!("{md}");
            }
        } else if flags.json {
            let empty_report = ChangelogReport {
                range: effective_range.unwrap_or_else(|| "HEAD".into()),
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

    if let Some(ref filter) = flags.scope_filter {
        let filter_lower = filter.to_lowercase();
        conventional.retain(|c| {
            c.scope
                .as_ref()
                .map(|s| s.to_lowercase() == filter_lower)
                .unwrap_or(false)
        });
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

    let range_display = used_tag
        .map(|tag| format!("since {tag}"))
        .unwrap_or_else(|| effective_range.clone().unwrap_or_else(|| "HEAD".into()));

    let report = ChangelogReport {
        range: range_display,
        total_scanned,
        conventional_count,
        excluded_release_chores,
        categories,
        breaking_changes,
    };

    if flags.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if flags.markdown || flags.output.is_some() {
        let md = render_markdown_string(&report);
        if let Some(path) = &flags.output {
            write_changelog_file(path, &md)?;
            println!("Wrote changelog to {}", path.display());
        } else {
            print!("{md}");
        }
    } else {
        render_terminal(&report);
    }

    Ok(ExitCode::SUCCESS)
}

fn resolve_range(explicit_range: Option<&str>) -> (Option<String>, Option<String>) {
    if let Some(r) = explicit_range {
        return (Some(r.to_string()), None);
    }

    if let Ok(tag) = last_tag() {
        return (Some(format!("{tag}..HEAD")), Some(tag));
    }

    (None, None)
}

fn last_tag() -> Result<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .context("failed to execute git describe")?;

    if !output.status.success() {
        bail!("no tags found");
    }

    let tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if tag.is_empty() {
        bail!("no tags found");
    }

    Ok(tag)
}

fn write_changelog_file(path: &Path, new_section: &str) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();

    if let Some(pos) = existing.find(MARKER) {
        let marker_end = pos + MARKER.len();
        let before = &existing[..marker_end];
        let after = &existing[marker_end..];
        let after = after.trim_start_matches('\n');
        let updated = format!("{before}\n\n{new_section}\n{after}");
        fs::write(path, updated).context("failed to write changelog file")?;
    } else {
        let content = format!("{MARKER}\n\n{new_section}\n");
        fs::write(path, content).context("failed to write changelog file")?;
    }

    Ok(())
}

fn fetch_commits(range: Option<&str>, limit: Option<usize>) -> Result<Vec<CommitEntry>> {
    let mut args = vec![
        "log",
        "--pretty=format:%h|%an|%ad|%s%x1f%b%x1e",
        "--date=short",
    ];

    let limit_arg;
    if let Some(r) = range {
        args.push(r);
    } else {
        let count = limit.unwrap_or(20);
        limit_arg = format!("-n{count}");
        args.push(&limit_arg);
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

    for raw_record in stdout.split('\x1e') {
        let record = raw_record.trim();
        if record.is_empty() {
            continue;
        }

        let (header_body, rest) = match record.split_once('\x1f') {
            Some((h, b)) => (h, Some(b)),
            None => (record, None),
        };

        let parts: Vec<&str> = header_body.splitn(4, '|').collect();
        if parts.len() < 4 {
            continue;
        }

        let hash = parts[0].trim().to_string();
        let author = parts[1].trim().to_string();
        let date = parts[2].trim().to_string();
        let raw_subject = parts[3].trim();
        let body = rest.map(|b| b.trim().to_string()).filter(|b| !b.is_empty());

        if let Some(entry) = parse_conventional(hash, author, date, raw_subject, body.as_deref()) {
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
    body: Option<&str>,
) -> Option<CommitEntry> {
    let subject_lower = subject.to_lowercase();
    let is_revert_msg = subject_lower.starts_with("revert \"");

    if is_revert_msg {
        let commit_type = "revert".to_string();
        let summary = subject.to_string();
        let is_breaking = body.is_some_and(|b| b.contains("BREAKING CHANGE"));

        return Some(CommitEntry {
            hash,
            commit_type,
            scope: None,
            summary,
            is_breaking,
            author,
            date,
            body: body.map(String::from),
        });
    }

    let is_breaking_text = subject.contains("BREAKING CHANGE")
        || body.is_some_and(|b| b.contains("BREAKING CHANGE"));
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
        body: body.map(String::from),
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

fn render_markdown_string(report: &ChangelogReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Changelog ({})\n\n", report.range));
    out.push_str(&format!(
        "_Scanned {} commits \u{00b7} {} conventional \u{00b7} {} release chores excluded_\n\n",
        report.total_scanned, report.conventional_count, report.excluded_release_chores
    ));

    if !report.breaking_changes.is_empty() {
        out.push_str("## BREAKING CHANGES\n\n");
        for entry in &report.breaking_changes {
            let scope_str = entry
                .scope
                .as_ref()
                .map(|s| format!("**{s}**: "))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {scope_str}{} (`{}`)\n",
                entry.summary, entry.hash
            ));
        }
        out.push('\n');
    }

    for (category, entries) in &report.categories {
        out.push_str(&format!("## {category}\n\n"));
        for entry in entries {
            let scope_str = entry
                .scope
                .as_ref()
                .map(|s| format!("**{s}**: "))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {scope_str}{} (`{}`)\n",
                entry.summary, entry.hash
            ));
        }
        out.push('\n');
    }

    out.push_str("---\n");
    out.push_str("_Generated by [Commitor](https://github.com/Commitor-AI/commitor)_\n");
    out
}

fn render_markdown_empty(range_label: &str) -> String {
    format!(
        "# Changelog\n\n_No Conventional Commits found in range `{range_label}`._\n"
    )
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        );

        assert!(entry.is_none());
    }

    #[test]
    fn breaking_change_detected_in_body() {
        let entry = parse_conventional(
            "b0d1e5c".into(),
            "Frank".into(),
            "2026-08-30".into(),
            "feat(api): add new endpoint",
            Some("BREAKING CHANGE: the /old endpoint is removed"),
        )
        .unwrap();

        assert!(entry.is_breaking);
        assert_eq!(entry.commit_type, "feat");
    }

    #[test]
    fn breaking_not_set_when_absent_from_body() {
        let entry = parse_conventional(
            "b0d1e5c".into(),
            "Frank".into(),
            "2026-08-30".into(),
            "feat(api): add new endpoint",
            Some("some unrelated body text"),
        )
        .unwrap();

        assert!(!entry.is_breaking);
    }

    #[test]
    fn parses_revert_message() {
        let entry = parse_conventional(
            "c0ffee1".into(),
            "Grace".into(),
            "2026-08-30".into(),
            "Revert \"feat(api): add new endpoint\"",
            None,
        )
        .unwrap();

        assert_eq!(entry.commit_type, "revert");
        assert_eq!(entry.summary, "Revert \"feat(api): add new endpoint\"");
        assert!(!entry.is_breaking);
    }

    #[test]
    fn parses_revert_conventional_type() {
        let entry = parse_conventional(
            "c0ffee2".into(),
            "Grace".into(),
            "2026-08-30".into(),
            "revert: undo database migration",
            None,
        )
        .unwrap();

        assert_eq!(entry.commit_type, "revert");
        assert_eq!(entry.summary, "undo database migration");
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
            body: None,
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
            body: None,
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
                body: None,
            },
            CommitEntry {
                hash: "c2".into(),
                commit_type: "feat".into(),
                scope: None,
                summary: "add widget".into(),
                is_breaking: false,
                author: "A".into(),
                date: "2026-08-30".into(),
                body: None,
            },
            CommitEntry {
                hash: "c3".into(),
                commit_type: "fix".into(),
                scope: None,
                summary: "patch bug".into(),
                is_breaking: false,
                author: "A".into(),
                date: "2026-08-30".into(),
                body: None,
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

    #[test]
    fn scope_filter_retains_only_matching_commits() {
        let commits = vec![
            CommitEntry {
                hash: "s1".into(),
                commit_type: "feat".into(),
                scope: Some("api".into()),
                summary: "add endpoint".into(),
                is_breaking: false,
                author: "A".into(),
                date: "2026-08-30".into(),
                body: None,
            },
            CommitEntry {
                hash: "s2".into(),
                commit_type: "fix".into(),
                scope: Some("cli".into()),
                summary: "fix flag".into(),
                is_breaking: false,
                author: "A".into(),
                date: "2026-08-30".into(),
                body: None,
            },
            CommitEntry {
                hash: "s3".into(),
                commit_type: "feat".into(),
                scope: None,
                summary: "add widget".into(),
                is_breaking: false,
                author: "A".into(),
                date: "2026-08-30".into(),
                body: None,
            },
        ];

        let filter = "API".to_lowercase();
        let filtered: Vec<&CommitEntry> = commits
            .iter()
            .filter(|c| {
                c.scope
                    .as_ref()
                    .map(|s| s.to_lowercase() == filter)
                    .unwrap_or(false)
            })
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].hash, "s1");
    }
}
