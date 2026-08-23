//! `commitor scan` — read-only analysis of the working diff.
//!
//! Runs cheap local heuristics first; only escalates to the backend's
//! `/analyze` endpoint when the heuristics cannot confidently call
//! the changeset one logical change.
//!
//! All analysis mechanics (diff collection, wire types, backend call)
//! live in [`crate::analysis`]; this module is collection policy and
//! rendering only.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::Result;

use crate::analysis;
use crate::heuristics::{self, Verdict};

#[derive(Debug, Default)]
pub struct ScanFlags {
    pub all: bool,
    pub offline: bool,
    pub strict: bool,
    pub json: bool,
}

pub fn run(flags: ScanFlags) -> Result<ExitCode> {
    // ── 1. Collect the diff ─────────────────────────────────────────
    let collected = analysis::collect_diff(flags.all)?;
    if collected.files.is_empty() {
        println!("Nothing to scan — no staged or unstaged changes.");
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "Scanning {} changed file(s) ({}).",
        collected.files.len(),
        if collected.staged_used { "staged" } else { "unstaged" }
    );

    // ── 2. Local heuristics ─────────────────────────────────────────
    let verdict = heuristics::evaluate(&collected.files);

    if let Verdict::Clean { summary } = &verdict {
        if flags.json {
            print_json_local(summary);
        } else {
            print_success(&format!("Looks like a single logical change — {summary}."));
        }
        return Ok(ExitCode::SUCCESS);
    }

    let Verdict::Inconclusive { reason } = verdict else {
        unreachable!("evaluate() only returns Clean or Inconclusive");
    };

    if flags.offline {
        if !flags.json {
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

    // ── 3. Backend escalation ───────────────────────────────────────
    // Credentials and the size guard are handled inside the shared
    // analysis call; this surfaces the standard not-logged-in message
    // before any network attempt.
    let response = analysis::analyze_patch(&collected.patch)?;

    // ── 4./5. Verdict + report ──────────────────────────────────────
    if flags.json {
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
