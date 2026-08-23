//! `commitor scan` — read-only analysis of the working diff.
//!
//! Runs cheap local heuristics first; only escalates to the backend's
//! `/analyze` endpoint when the heuristics cannot confidently call
//! the changeset one logical change.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use crate::auth::{self, DASHBOARD_URL};
use crate::config;
use crate::engine::git;
use crate::heuristics::{self, Verdict};

/// The backend may route to a slow reasoning model; give it room
/// instead of failing mid-analysis.
const ANALYZE_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Default)]
pub struct ScanFlags {
    pub all: bool,
    pub offline: bool,
    pub strict: bool,
    pub json: bool,
}

pub fn run(flags: ScanFlags) -> Result<ExitCode> {
    // ── 1. Collect the diff ─────────────────────────────────────────
    let (staged_used, files, patch) = collect_diff(&flags)?;
    if files.is_empty() {
        println!("Nothing to scan — no staged or unstaged changes.");
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "Scanning {} changed file(s) ({}).",
        files.len(),
        if staged_used { "staged" } else { "unstaged" }
    );

    // ── 2. Local heuristics ─────────────────────────────────────────
    if let Verdict::Clean { summary } = heuristics::evaluate(&files) {
        if flags.json {
            print_json_local(&summary);
        } else {
            print_success(&format!("Looks like a single logical change — {summary}."));
        }
        return Ok(ExitCode::SUCCESS);
    }

    let Verdict::Inconclusive { reason } = heuristics::evaluate(&files) else {
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
    if patch.chars().count() > 190_000 {
        bail!(
            "The change is too large for analysis (>200k characters).\n\
             Try scanning a narrower set of changes, or split it manually."
        );
    }
    // Credentials are needed from here on; this surfaces the standard
    // not-logged-in message before any network call.
    let api_key = auth::load_api_key()?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start async runtime")?;
    let response = runtime.block_on(analyze_request(&api_key, &patch))?;

    // ── 4./5. Verdict + report ──────────────────────────────────────
    if response.groups.len() <= 1 {
        let summary = response
            .groups
            .first()
            .map(|group| group.commit_message.clone())
            .unwrap_or_else(|| "single logical change".to_string());
        if flags.json {
            print_json(&response);
        } else {
            print_success(&summary);
        }
        return Ok(ExitCode::SUCCESS);
    }

    if flags.json {
        print_json(&response);
    } else {
        print_mixed_report(&response, &reason);
    }
    if flags.strict {
        // For pre-commit hooks and CI: mixed fails the run.
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// Decide which diff to scan: staged unless `--all`, with an explicit
/// warning when falling back to unstaged.
fn collect_diff(flags: &ScanFlags) -> Result<(bool, Vec<String>, String)> {
    if flags.all {
        return collect_unstaged();
    }

    let staged_files = git::changed_files(true)?;
    if !staged_files.is_empty() {
        let patch = git::diff_patch(true)?;
        return Ok((true, staged_files, patch));
    }

    println!("No staged changes found, scanning unstaged changes instead");
    collect_unstaged()
}

fn collect_unstaged() -> Result<(bool, Vec<String>, String)> {
    let files = git::changed_files(false)?;
    let patch = if files.is_empty() {
        String::new()
    } else {
        git::diff_patch(false)?
    };
    Ok((false, files, patch))
}

/// POST the diff to `{API_BASE_URL}/analyze`.
async fn analyze_request(api_key: &str, patch: &str) -> Result<AnalyzeResponse> {
    use reqwest::Client;

    let url = format!("{}/analyze", config::api_base_url());

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(ANALYZE_TIMEOUT_SECS))
        .build()
        .context("failed to set up HTTP client")?;

    let request = auth::with_key(
        client.post(&url).json(&AnalyzeRequest {
            diff: patch,
            context: None,
        }),
        api_key,
    );

    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => bail!(
            "Couldn't reach the Commitor API at {url} ({}). \
             Is your backend running? Set COMMITOR_API_URL if it lives elsewhere.",
            auth::root_cause(&err)
        ),
    };

    let status = response.status();
    match status.as_u16() {
        200 => response.json::<AnalyzeResponse>().await.with_context(|| {
            format!("{url} returned a response that doesn't match the analyze schema")
        }),
        401 | 403 => bail!(
            "Your stored API key was rejected (HTTP {status}) — it may have expired or been revoked.\n\
             Run `commitor login --key <your-key>` again (get a key at {DASHBOARD_URL})"
        ),
        404 => bail!(
            "{url} does not exist on this backend — is COMMITOR_API_URL pointing at a server with /analyze?"
        ),
        429 => bail!("Rate limited by the Commitor API — wait a moment and try again."),
        _ => {
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            if snippet.is_empty() {
                bail!("Commitor API returned HTTP {status} for {url}");
            }
            bail!("Commitor API returned HTTP {status} for {url}\nServer said: {snippet}");
        }
    }
}

// ── wire format ─────────────────────────────────────────────────────

/// Matches `AnalyzeRequest` in commitor-api.
#[derive(Serialize)]
struct AnalyzeRequest<'a> {
    diff: &'a str,
    context: Option<&'a str>,
}

/// Matches `AnalyzeResponse`. Tolerant by design so newer backends
/// keep working with older CLIs.
#[derive(Debug, Deserialize, Serialize)]
struct AnalyzeResponse {
    groups: Vec<ChangeGroup>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    model_tier: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChangeGroup {
    /// The backend names each group by its theme via the suggested
    /// commit message.
    files: Vec<String>,
    commit_message: String,
    rationale: String,
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

fn print_mixed_report(response: &AnalyzeResponse, heuristic_reason: &str) {
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

fn print_json(response: &AnalyzeResponse) {
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
