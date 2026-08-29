//! `commitor scan` — read-only analysis of the working diff.
//!
//! Runs cheap local heuristics first; for a working-tree scan it only
//! escalates to the backend's `/analyze` endpoint when the heuristics
//! cannot confidently call the changeset one logical change. For
//! `--diff-range` (PR-scale) runs the local heuristic is advisory only —
//! the backend is always consulted, because PR diffs mix unrelated areas
//! the path-clustering can't see.
//!
//! All analysis mechanics (diff collection, wire types, backend call)
//! live in [`crate::analysis`]; this module is collection policy and
//! rendering only.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{bail, Result};

use crate::analysis;
use crate::heuristics::{self, Verdict};

#[derive(Debug, Default)]
pub struct ScanFlags {
    pub all: bool,
    pub offline: bool,
    pub strict: bool,
    pub json: bool,
    /// Optional explicit git range to analyze (e.g. `origin/main...HEAD`),
    /// instead of the working tree. Incompatible with `all`.
    pub diff_range: Option<String>,
    /// Emit GitHub-flavored Markdown suited to a PR comment.
    pub markdown: bool,
    /// Optional PR-scoped context (title/description) forwarded to the
    /// model. Supplied by the GitHub Action so PR-scale analysis can weigh
    /// the stated intent against the files actually touched.
    pub context: Option<String>,
}

/// Whether a scan must escalate to the backend rather than trust the
/// local heuristic verdict. `--diff-range` (PR-scale) scans always
/// escalate — the heuristic is advisory only there. A working-tree scan
/// escalates unless the heuristic confidently called the change clean.
/// Offline scans never reach this (they're handled before any call).
fn scan_escalates(verdict: &Verdict, diff_range: bool) -> bool {
    diff_range || !matches!(verdict, Verdict::Clean { .. })
}

pub fn run(flags: ScanFlags) -> Result<ExitCode> {
    // Machine-readable output modes suppress the human progress lines.
    let quiet = flags.json || flags.markdown;

    // ── 1. Collect the diff ─────────────────────────────────────────
    let collected = if let Some(range) = &flags.diff_range {
        if flags.all {
            bail!(
                "`--diff-range` and `--all` are incompatible — a fixed range already \
                 defines exactly what to analyze, so there is no working-tree flavor \
                 (staged vs. unstaged) to choose between."
            );
        }
        // A fixed range is its own scope: skip the staged/unstaged
        // fallback warning entirely and never pull in untracked files.
        analysis::collect_range(range)?
    } else {
        // Scan deliberately ignores untracked files — its scope is the
        // tracked working diff (`commit` opts into untracked files).
        analysis::collect_diff(flags.all, false)?
    };

    if collected.files.is_empty() {
        if let Some(range) = &flags.diff_range {
            if flags.markdown {
                println!("<!-- commitor-analysis -->\n");
                println!("## 🔍 Commitor Analysis\n");
                println!("_No changes to analyze for range `{range}`._\n");
                print_powered_by();
            } else {
                println!("No changes to analyze for range `{range}`.");
            }
        } else {
            println!("Nothing to scan — no staged or unstaged changes.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if !quiet {
        if let Some(range) = &flags.diff_range {
            println!(
                "Analyzing {} changed file(s) in range `{range}`.",
                collected.files.len()
            );
        } else {
            println!(
                "Scanning {} changed file(s) ({}).",
                collected.files.len(),
                if collected.staged_used { "staged" } else { "unstaged" }
            );
        }
    }

    // ── 2. Local heuristics ─────────────────────────────────────────
    let verdict = heuristics::evaluate(&collected.files);
    let reason = match &verdict {
        Verdict::Inconclusive { reason } => reason.clone(),
        Verdict::Clean { .. } => String::new(),
    };

    // `--offline` has no backend to consult: trust a "clean" verdict and
    // otherwise report the heuristic's inconclusive reason.
    if flags.offline {
        if matches!(verdict, Verdict::Clean { .. }) {
            if flags.markdown {
                print_markdown_clean("single logical change");
            } else if flags.json {
                print_json_local("single logical change");
            } else {
                print_success("Looks like a single logical change.");
            }
        } else if flags.markdown {
            print_markdown_offline(&reason);
        } else if !flags.json {
            println!();
            println!(
                "Local heuristics could not confirm one logical change ({reason})."
            );
            println!(
                "Rerun without --offline for a full AI analysis (requires `commitor login`)."
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Online. For `--diff-range` (PR-scale) runs the local heuristic is
    // advisory only and we *always* escalate to the backend — PR diffs
    // routinely combine unrelated areas the path-clustering can't see
    // (most notably a root-level dependency bump, which has *no*
    // top-level directory and is invisible to the clustering). A
    // working-tree scan only escalates when the heuristic couldn't call
    // the change clean, preserving the pre-existing short-circuit.
    if !scan_escalates(&verdict, flags.diff_range.is_some()) {
        if let Verdict::Clean { summary } = &verdict {
            if flags.markdown {
                print_markdown_clean(summary);
            } else if flags.json {
                print_json_local(summary);
            } else {
                print_success(&format!("Looks like a single logical change — {summary}."));
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    // ── 3. Backend escalation ───────────────────────────────────────
    // Credentials and the size guard are handled inside the shared
    // analysis call; this surfaces the standard not-logged-in message
    // before any network attempt. PR-scoped context (title/description)
    // is forwarded when the caller supplied it (e.g. the GitHub Action).
    let (response, rate) = analysis::analyze_patch_with_context(
        &collected.patch,
        flags.context.as_deref(),
    )?;

    // ── 4./5. Verdict + report ──────────────────────────────────────
    if flags.markdown {
        if response.groups.len() <= 1 {
            let summary = response
                .groups
                .first()
                .map(|group| group.commit_message.clone())
                .unwrap_or_else(|| "single logical change".to_string());
            print_markdown_clean(&summary);
        } else {
            print_markdown_mixed(&response, &reason);
        }
    } else if flags.json {
        print_json(&response);
    } else if response.groups.len() <= 1 {
        let summary = response
            .groups
            .first()
            .map(|group| group.commit_message.clone())
            .unwrap_or_else(|| "single logical change".to_string());
        print_success(&summary);
    } else {
        print_mixed_report(&response, &reason);
    }

    // Soft quota hint on human output only — JSON/Markdown and piped
    // output must stay machine-clean.
    if !flags.json && !flags.markdown {
        if let Some(message) = rate.low_quota_message() {
            eprintln!();
            eprintln!("{message}");
        }
    }

    if response.groups.len() > 1 && flags.strict {
        // For pre-commit hooks and CI: mixed fails the run.
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

// ── output rendering ────────────────────────────────────────────────

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn green(text: &str) -> String {
    if use_color() {
        format!("\x1b[32m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn red(text: &str) -> String {
    if use_color() {
        format!("\x1b[31m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn print_success(summary: &str) {
    println!("{}", green("✓ Looks like a single logical change"));
    println!("  {summary}");
}

fn print_mixed_report(response: &analysis::AnalyzeResponse, heuristic_reason: &str) {
    println!("{}", red("✗ This commit looks mixed"));
    println!("  local hint: {heuristic_reason}");
    println!();

    for (index, group) in response.groups.iter().enumerate() {
        let number = index + 1;
        println!("{number}. {}", group.commit_message);
        if !group.files.is_empty() {
            println!("   files: {}", group.files.join(", "));
        }
        if !group.rationale.is_empty() {
            println!("   why:   {}", group.rationale);
        }
        println!();
    }

    if let Some(confidence) = response.confidence {
        match &response.model_tier {
            Some(tier) => println!("(confidence {confidence:.2}, model tier: {tier})"),
            None => println!("(confidence {confidence:.2})"),
        }
    }
    println!("Run `commitor commit` to split this into separate commits.");
}

fn print_json(response: &analysis::AnalyzeResponse) {
    match serde_json::to_string_pretty(response) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("error: failed to serialize analysis result: {err}"),
    }
}

fn print_json_local(summary: &str) {
    let payload = serde_json::json!({
        "groups": [{
            "files": [],
            "commit_message": summary,
            "rationale": "local heuristics found a single cluster",
        }],
        "model_tier": "local",
    });
    println!("{payload}");
}

// ── Markdown (PR-comment) rendering ─────────────────────────────────
//
// Every Markdown variant opens with the hidden `<!-- commitor-analysis -->`
// marker so the GitHub Action can find and update a previous comment
// instead of posting a fresh one on each push. Output is deliberately
// compact — PR comments that are walls of text get ignored. Severity is
// derived, not from the model: a single logical change reads "Low", while
// any group that's part of a mixed (split-worthy) PR reads "Medium", and
// the PR as a whole is flagged "High".

fn print_powered_by() {
    println!(
        "\n<sub>Powered by [Commitor](https://github.com/Commitor-AI/commitor) — \
         catches unrelated changes before they're buried.</sub>"
    );
}

fn print_markdown_clean(summary: &str) {
    println!("<!-- commitor-analysis -->\n");
    println!("## 🔍 Commitor Analysis\n");
    println!("✅ **Looks like a single logical change** — {summary}\n");
    print_powered_by();
}

fn print_markdown_offline(reason: &str) {
    println!("<!-- commitor-analysis -->\n");
    println!("## 🔍 Commitor Analysis\n");
    println!(
        "⚠️ Local heuristics couldn't confirm a single logical change ({reason}). \
         Run `commitor login` for a full AI analysis.\n"
    );
    print_powered_by();
}

fn print_markdown_mixed(response: &analysis::AnalyzeResponse, reason: &str) {
    println!("<!-- commitor-analysis -->\n");
    println!("## 🔍 Commitor Analysis\n");
    println!("**Severity: High** — this PR bundles multiple unrelated changes.\n");
    println!("⚠️ {reason} Consider splitting it into separate commits:\n");

    println!("| # | Suggested commit | Severity | Files | Why |");
    println!("|---|---|---|---|---|");
    for (index, group) in response.groups.iter().enumerate() {
        let number = index + 1;
        let files = if group.files.is_empty() {
            group
                .partial_files
                .iter()
                .map(|p| p.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            group.files.join(", ")
        };
        let why = if group.rationale.is_empty() {
            "—".to_string()
        } else {
            group.rationale.replace('|', "\\|")
        };
        println!(
            "| {number} | {} | Medium | {files} | {why} |",
            group.commit_message
        );
    }
    println!();
    println!("Run `commitor commit` to split this into separate commits.");
    print_powered_by();
}

#[cfg(test)]
mod tests {
    use super::*;
    use heuristics::Verdict;

    fn paths(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    // The escalation decision that fixes the PR-scale blind spot.
    #[test]
    fn working_tree_clean_short_circuits() {
        let verdict = Verdict::Clean {
            summary: "all changes under src".into(),
        };
        assert!(!scan_escalates(&verdict, false));
    }

    #[test]
    fn working_tree_inconclusive_escalates() {
        let verdict = Verdict::Inconclusive {
            reason: "spreads across areas".into(),
        };
        assert!(scan_escalates(&verdict, false));
    }

    #[test]
    fn diff_range_never_short_circuits_on_clean() {
        // The heart of the fix: a PR-scale "clean" verdict must still
        // reach the backend, because the path heuristic is advisory only
        // at that scale.
        let verdict = Verdict::Clean {
            summary: "all changes under src".into(),
        };
        assert!(scan_escalates(&verdict, true));
    }

    #[test]
    fn diff_range_inconclusive_escalates() {
        let verdict = Verdict::Inconclusive {
            reason: "spreads across areas".into(),
        };
        assert!(scan_escalates(&verdict, true));
    }

    // DOCUMENTED BLIND SPOT: the local heuristic calls a root-level
    // dependency bump combined with a feature "Clean", because files
    // without a top-level directory (Cargo.toml, Cargo.lock, …) are
    // invisible to the top-directory clustering. This is exactly the
    // class of PR that `--diff-range` must still escalate — which the
    // test above guarantees.
    #[test]
    fn root_dependency_bump_plus_feature_looks_clean_to_heuristic() {
        let verdict = heuristics::evaluate(&paths(&[
            "Cargo.toml",
            "Cargo.lock",
            "src/feature/login.rs",
        ]));
        assert_eq!(
            verdict,
            Verdict::Clean {
                summary: "all changes under src".to_string()
            },
            "heuristic blind spot assumption must hold for the diff-range fix to matter"
        );
    }

    #[test]
    fn mixed_pr_with_root_bump_is_clean_locally_but_must_escalate() {
        let verdict = heuristics::evaluate(&paths(&[
            "Cargo.toml",
            "Cargo.lock",
            "src/feature/login.rs",
        ]));
        assert!(matches!(verdict, Verdict::Clean { .. }));
        // …yet a --diff-range scan must escalate it to the backend.
        assert!(scan_escalates(&verdict, true));
    }
}

