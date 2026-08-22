use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProposedCommit {
    pub message: String,
    pub files: Vec<String>,
    pub reasoning: String,
    pub bundling_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupingResult {
    pub commits: Vec<ProposedCommit>,
    pub summary: String,
}

/// Build a prompt asking the model to group a diff into logical, atomic commits.
///
/// The prompt requests:
/// - Conventional Commits-style messages
/// - Explicit flags for unrelated changes bundled together
/// - Strict JSON output matching `GroupingResult`'s shape
pub fn build_commit_prompt(diff_text: &str) -> String {
    format!(
        r#"You are Commitor, a tool that catches unrelated changes before they get buried in a commit.

Given the following diff, group the changed files into logical, atomic commits. Each commit should contain only files that belong together conceptually.

For each commit:
- Write a Conventional Commits-style message (e.g. "feat: add user login endpoint", "fix: resolve null pointer in parser")
- List the files that belong in that commit
- Provide brief reasoning for why these files are grouped together
- If you notice unrelated changes that appear bundled together and cannot be cleanly separated, set `bundling_warning` to a description of the concern. Otherwise set it to null.

Diffs:
{diff_text}

Respond with **only** valid JSON — no preamble, no markdown fences. The JSON must match this exact structure:

{{
  "commits": [
    {{
      "message": "type(scope): description",
      "files": ["file/path.ext"],
      "reasoning": "why these files belong together",
      "bundling_warning": null
    }}
  ],
  "summary": "One-line overview of what this diff accomplishes"
}}"#
    )
}

/// Parse the model's raw response into a `GroupingResult`.
///
/// Defensively strips markdown code fences in case the model adds them
/// despite instructions not to.
pub fn parse_grouping_response(raw: &str) -> Result<GroupingResult> {
    let trimmed = raw.trim();

    let json_str = strip_code_fences(trimmed);

    serde_json::from_str(json_str)
        .with_context(|| format!("failed to parse JSON from model response:\n{trimmed}"))
}

fn strip_code_fences(s: &str) -> &str {
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_prefix("```").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

/// Group files by their top-level directory without any API call.
///
/// This is the `--no-ai` fallback.
pub fn group_files_locally(changed_files: &[String]) -> GroupingResult {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file in changed_files {
        let dir = file.split('/').next().unwrap_or("root").to_string();
        groups.entry(dir).or_default().push(file.clone());
    }

    let total_files = changed_files.len();
    let commits: Vec<ProposedCommit> = groups
        .into_iter()
        .map(|(dir, files)| {
            let count = files.len();
            ProposedCommit {
                message: format!("chore: update {count} file(s) in {dir}"),
                files,
                reasoning: format!("all files under the {dir} directory"),
                bundling_warning: None,
            }
        })
        .collect();

    GroupingResult {
        commits,
        summary: format!("locally grouped {total_files} file(s) by top-level directory"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_grouping_response ──────────────────────────────────────

    #[test]
    fn parse_valid_json() {
        let json = r#"{
            "commits": [
                {
                    "message": "feat: add login",
                    "files": ["src/auth.rs", "src/main.rs"],
                    "reasoning": "authentication related",
                    "bundling_warning": null
                }
            ],
            "summary": "adds login feature"
        }"#;

        let result = parse_grouping_response(json).unwrap();
        assert_eq!(result.commits.len(), 1);
        assert_eq!(result.commits[0].message, "feat: add login");
        assert_eq!(result.commits[0].bundling_warning, None);
        assert_eq!(result.summary, "adds login feature");
    }

    #[test]
    fn parse_json_inside_markdown_fences() {
        let input = r#"```json
{
    "commits": [
        {
            "message": "fix: resolve crash",
            "files": ["bug.rs"],
            "reasoning": "single file fix",
            "bundling_warning": null
        }
    ],
    "summary": "hotfix"
}
```"#;

        let result = parse_grouping_response(input).unwrap();
        assert_eq!(result.commits[0].message, "fix: resolve crash");
        assert_eq!(result.summary, "hotfix");
    }

    #[test]
    fn parse_json_inside_plain_fences() {
        let input = "```\n{\"commits\":[],\"summary\":\"empty\"}\n```";
        let result = parse_grouping_response(input).unwrap();
        assert!(result.commits.is_empty());
        assert_eq!(result.summary, "empty");
    }

    #[test]
    fn parse_malformed_json_returns_err() {
        let input = "this is not json at all";
        let result = parse_grouping_response(input);
        assert!(result.is_err());
    }

    #[test]
    fn parse_bundling_warning_preserved() {
        let json = r#"{
            "commits": [
                {
                    "message": "feat: add feature",
                    "files": ["a.rs", "b.rs"],
                    "reasoning": "related",
                    "bundling_warning": "these files seem unrelated"
                }
            ],
            "summary": "mixed changes"
        }"#;

        let result = parse_grouping_response(json).unwrap();
        assert_eq!(
            result.commits[0].bundling_warning.as_deref(),
            Some("these files seem unrelated")
        );
    }

    // ── group_files_locally ──────────────────────────────────────────

    #[test]
    fn local_grouping_by_top_dir() {
        let files: Vec<String> = vec![
            "src/main.rs".into(),
            "src/lib.rs".into(),
            "tests/integration.rs".into(),
            "README.md".into(),
        ];

        let result = group_files_locally(&files);
        assert_eq!(result.commits.len(), 3);

        let messages: Vec<&str> = result.commits.iter().map(|c| c.message.as_str()).collect();
        assert!(messages.contains(&"chore: update 2 file(s) in src"));
        assert!(messages.contains(&"chore: update 1 file(s) in tests"));
        assert!(messages.contains(&"chore: update 1 file(s) in README.md"));

        assert_eq!(
            result.summary,
            "locally grouped 4 file(s) by top-level directory"
        );
    }

    #[test]
    fn local_grouping_single_top_level_file() {
        let files: Vec<String> = vec!["Cargo.toml".into()];
        let result = group_files_locally(&files);
        assert_eq!(result.commits.len(), 1);
        assert_eq!(
            result.commits[0].message,
            "chore: update 1 file(s) in Cargo.toml"
        );
        assert_eq!(result.commits[0].files, vec!["Cargo.toml"]);
    }

    #[test]
    fn local_grouping_empty_input() {
        let result = group_files_locally(&[]);
        assert!(result.commits.is_empty());
        assert_eq!(
            result.summary,
            "locally grouped 0 file(s) by top-level directory"
        );
    }
}
