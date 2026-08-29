//! Shared working-diff analysis used by both `scan` and `commit`.
//!
//! Owns the three pieces every analysis-driven command needs:
//!
//! 1. diff collection (`git diff --staged`, falling back to unstaged
//!    with a warning, or unstaged outright with `--all`),
//! 2. the wire types for the backend's `/analyze` endpoint,
//! 3. the authenticated backend call itself (size guard + long
//!    timeout + friendly HTTP error mapping).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use crate::auth::{self, DASHBOARD_URL};
use crate::config;
use crate::engine::git;

// Re-exported so commands resolve credentials through the analysis
// layer instead of importing `auth` directly.
pub use crate::auth::load_api_key;

/// The backend may run an escalation chain (fast pass, reasoning
/// escalation, recheck and message-quality turns) on a slow model;
/// give the whole request room instead of failing mid-analysis.
/// `COMMITOR_TIMEOUT_SECS` overrides this for slow setups.
const ANALYZE_TIMEOUT_SECS: u64 = 180;

/// Where users upgrade their plan — shown when a free-tier diff is too
/// large to analyze.
const PRICING_URL: &str = "https://commitor.dev/pricing";

/// Free-tier client cap on diff size. The backend enforces its own
/// `max_length`; we stop just short of that so a free request never
/// 422s. Pro users are NOT capped here — the backend enforces the
/// (higher) per-plan maximum and returns a clear error if exceeded.
const MAX_PATCH_CHARS: usize = 190_000;

/// Cached plan for the active key (`None` = couldn't verify → treat as
/// free, the safe default). One key per run, so a `OnceLock` is enough.
static PLAN_CACHE: OnceLock<Option<String>> = OnceLock::new();

/// True when the active account is on a paid plan. Anything other than
/// `"free"` (or an empty/missing plan field) counts as paid; a plan we
/// can't verify is treated as free so oversized diffs stay gated.
fn is_pro(api_key: &str) -> bool {
    let plan = PLAN_CACHE
        .get_or_init(|| crate::auth::plan_for_key(api_key).ok())
        .clone();
    match plan {
        Some(plan) => !plan.eq_ignore_ascii_case("free"),
        None => false,
    }
}

/// Enforce the free-tier diff-size cap. Pro accounts pass through
/// untouched — the server decides their (higher) limit.
fn enforce_size_guard(api_key: &str, patch: &str) -> Result<(), AnalyzeError> {
    if is_pro(api_key) {
        return Ok(());
    }
    if patch.chars().count() > MAX_PATCH_CHARS {
        return Err(AnalyzeError::Other(anyhow!(
            "The change is too large for analysis (>{} characters).\n\
             Large diffs are a Commitor Pro feature — upgrade at {PRICING_URL} to analyze\n\
             changes this big. Or split the changes into smaller commits manually.",
            MAX_PATCH_CHARS
        )));
    }
    Ok(())
}

fn analyze_timeout() -> std::time::Duration {
    let secs = std::env::var("COMMITOR_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(ANALYZE_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// How many leading bytes of an untracked file are inspected when
/// deciding whether it is binary (NUL byte heuristic, same spirit as
/// git's own).
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

// ── diff collection ─────────────────────────────────────────────────

/// The working diff one of the analysis commands operates on.
#[derive(Debug, Clone)]
pub struct CollectedDiff {
    /// `true` when the staged diff was used, `false` for unstaged.
    pub staged_used: bool,
    /// Changed file paths (repo-relative), tracked first then any
    /// included untracked files.
    pub files: Vec<String>,
    /// The full patch text sent to the backend — tracked diffs plus
    /// synthesized new-file sections for untracked files, if any.
    pub patch: String,
    /// Untracked files that were folded into the patch. Empty unless
    /// `collect_diff` was called with `include_untracked: true`.
    pub untracked: Vec<String>,
}

/// Decide which diff to analyze: staged unless `all`, with an
/// explicit warning when falling back to unstaged.
///
/// `include_untracked` folds never-added files into the patch as
/// synthesized "new file" sections (without touching the index) so
/// the backend can plan them; `commit` opts in, `scan` does not, so
/// scan's view of the world is unchanged.
pub fn collect_diff(all: bool, include_untracked: bool) -> Result<CollectedDiff> {
    let mut collected = if all {
        collect_unstaged()?
    } else {
        let staged_files = git::changed_files(true)?;
        if !staged_files.is_empty() {
            CollectedDiff {
                staged_used: true,
                files: staged_files,
                patch: git::diff_patch(true)?,
                untracked: Vec::new(),
            }
        } else {
            println!("No staged changes found, scanning unstaged changes instead");
            collect_unstaged()?
        }
    };

    if include_untracked {
        for path in git::untracked_files()? {
            match synthesize_untracked_section(&path) {
                Ok(section) => {
                    collected.patch.push_str(&section);
                    collected.files.push(path.clone());
                    collected.untracked.push(path);
                }
                Err(err) => {
                    // An unreadable file can't be planned or committed
                    // reliably; warn and leave it out of the analysis
                    // entirely rather than failing the whole command.
                    eprintln!("warning: skipping unreadable file {path}: {err:#}");
                }
            }
        }
    }

    Ok(collected)
}

fn collect_unstaged() -> Result<CollectedDiff> {
    let files = git::changed_files(false)?;
    let patch = if files.is_empty() {
        String::new()
    } else {
        git::diff_patch(false)?
    };
    Ok(CollectedDiff {
        staged_used: false,
        files,
        patch,
        untracked: Vec::new(),
    })
}

/// Build a synthetic unified-diff section presenting an untracked file
/// as a brand-new file, so it flows through the normal wire format and
/// hunk parser without mutating the git index (`no git add -N`).
///
/// - Binary content (NUL byte in the first 8 KiB): a one-line
///   "Binary files … differ" note, no hunk body → parsed as atomic.
/// - Empty file: header lines only, no hunk section → also atomic.
fn synthesize_untracked_section(path: &str) -> Result<String> {
    use std::fs;

    let bytes = fs::read(path).with_context(|| format!("failed to read '{path}'"))?;

    let mut section = format!("diff --git a/{path} b/{path}\nnew file mode 100644\n");

    let sniff_end = bytes.len().min(BINARY_SNIFF_BYTES);
    if bytes[..sniff_end].contains(&0u8) {
        section.push_str(&format!("Binary files /dev/null and b/{path} differ\n"));
        return Ok(section);
    }

    section.push_str("--- /dev/null\n");
    section.push_str(&format!("+++ b/{path}\n"));

    let text = String::from_utf8_lossy(&bytes);
    let line_count = text.lines().count();
    if line_count == 0 {
        // Empty file: header alone marks it changed; no hunk section.
        return Ok(section);
    }

    section.push_str(&format!("@@ -0,0 +1,{line_count} @@\n"));
    for line in text.lines() {
        section.push('+');
        section.push_str(line);
        section.push('\n');
    }
    Ok(section)
}

// ── backend escalation ──────────────────────────────────────────────

/// Quota info the backend attaches to every analyze response via
/// `X-RateLimit-*` headers. All fields optional so older backends
/// (which send no headers) degrade silently.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateStatus {
    pub remaining: Option<u32>,
}

impl RateStatus {
    /// The soft "running out of quota" hint, when one should be shown.
    pub fn low_quota_message(&self) -> Option<String> {
        let n = self.remaining?;
        if n > 3 {
            return None;
        }
        Some(if n == 1 {
            "1 analysis left today".to_string()
        } else {
            format!("{n} analyses left today")
        })
    }
}

/// Run the full backend analysis for `patch`.
///
/// Loads the stored API key (surfacing the standard not-logged-in
/// message when absent), applies the size guard, then POSTs to
/// `/analyze`. This is the entry point `scan` uses once its local
/// heuristics are inconclusive.
///
/// Returns the response plus the quota snapshot from the response
/// headers, so commands can warn when the user is close to their
/// daily limit.
pub fn analyze_patch(patch: &str) -> Result<(AnalyzeResponse, RateStatus), AnalyzeError> {
    let api_key = auth::load_api_key()?;
    analyze_with_key(&api_key, patch)
}

/// Same as [`analyze_patch`] for callers that already loaded the key
/// (e.g. `commit`, which requires auth before touching any state).
pub fn analyze_with_key(
    api_key: &str,
    patch: &str,
) -> Result<(AnalyzeResponse, RateStatus), AnalyzeError> {
    analyze_with_mode(api_key, patch, "scan")
}

/// Commit passes `mode = "commit"` so the backend never answers with
/// the deterministic local tier — a commit deserves a real message.
pub fn analyze_with_mode(
    api_key: &str,
    patch: &str,
    mode: &str,
) -> Result<(AnalyzeResponse, RateStatus), AnalyzeError> {
    enforce_size_guard(api_key, patch)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| AnalyzeError::Other(anyhow!("failed to start async runtime: {err:#}")))?;
    runtime.block_on(analyze_request(api_key, patch, mode))
}

/// POST the diff to `{API_BASE_URL}/analyze`.
async fn analyze_request(
    api_key: &str,
    patch: &str,
    mode: &str,
) -> Result<(AnalyzeResponse, RateStatus), AnalyzeError> {
    use reqwest::Client;

    let url = format!("{}/analyze", config::api_base_url());

    let client = Client::builder()
        .timeout(analyze_timeout())
        .build()
        .map_err(|err| AnalyzeError::Other(anyhow!("failed to set up HTTP client: {err:#}")))?;

    let request = auth::with_key(
        client.post(&url).json(&AnalyzeRequest {
            diff: patch,
            context: None,
            mode: Some(mode),
        }),
        api_key,
    );

    let response = match request.send().await {
        Ok(response) => response,
        Err(err) => return Err(AnalyzeError::Unavailable(format!(
            "couldn't reach the Commitor API at {url} ({}). Is your backend running? \
             Set COMMITOR_API_URL if it lives elsewhere.",
            auth::root_cause(&err)
        ))),
    };

    let status = response.status();
    match status.as_u16() {
        200 => {
            let rate = parse_rate_status(response.headers());
            let parsed = response.json::<AnalyzeResponse>().await.map_err(|err| {
                AnalyzeError::Other(anyhow!(
                    "{url} returned a response that doesn't match the analyze schema: {err}"
                ))
            })?;
            Ok((parsed, rate))
        }
        401 | 403 => Err(AnalyzeError::InvalidKey(format!(
            "Your stored API key was rejected (HTTP {status}) — it may have expired or been revoked.\n\
             Run `commitor login --key <your-key>` again (get a key at {DASHBOARD_URL})"
        ))),
        429 => {
            let body = response.text().await.unwrap_or_default();
            Err(AnalyzeError::RateLimited(describe_rate_limit(&body)))
        }
        413 => {
            let body = response.text().await.unwrap_or_default();
            Err(AnalyzeError::Other(anyhow!(

                "{}",
                describe_diff_too_large(&body)
            )))
        }
        _ => {
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            if snippet.is_empty() {
                return Err(AnalyzeError::Unavailable(format!(
                    "Commitor API returned HTTP {status} for {url}"
                )));
            }
            Err(AnalyzeError::Unavailable(format!(
                "Commitor API returned HTTP {status} for {url}\nServer said: {snippet}"
            )))
        }
    }
}

/// Why an analysis could not be produced. Commit uses the first two
/// variants as a signal to fall back to an offline plan instead of
/// failing; everything else is a real error worth surfacing.
#[derive(Debug)]
pub enum AnalyzeError {
    /// 429 — daily quota exhausted; message is human-friendly already.
    RateLimited(String),
    /// Backend unreachable, timed out, or answered 5xx.
    Unavailable(String),
    /// 401/403 — must be fixed by the user, never papered over.
    InvalidKey(String),
    /// Schema mismatches, oversized patches, config mistakes.
    Other(anyhow::Error),
}

impl std::fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeError::RateLimited(msg)
            | AnalyzeError::Unavailable(msg)
            | AnalyzeError::InvalidKey(msg) => f.write_str(msg),
            AnalyzeError::Other(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for AnalyzeError {}

impl From<anyhow::Error> for AnalyzeError {
    fn from(err: anyhow::Error) -> Self {
        AnalyzeError::Other(err)
    }
}

/// Turn a 429 body into a calm, actionable message. The backend sends
/// `{error, message, limit, reset_at}`; anything else still gets a
/// decent fallback instead of a raw HTTP error.
/// Turn a 413 (diff too large) body into a calm, actionable message.
/// The backend sends `{error, message, limit, upgrade_url}`; anything
/// else still gets a decent fallback instead of a raw HTTP error.
fn describe_diff_too_large(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => v
            .get("message")
            .and_then(|m| m.as_str())
            .map(|m| m.to_string())
            .unwrap_or_else(|| {
                "This change is too large to analyze on your plan. Upgrade at \
                 https://commitor.dev/pricing for larger diffs."
                    .to_string()
            }),
        Err(_) => {
            "This change is too large to analyze on your plan. Upgrade at \
             https://commitor.dev/pricing for larger diffs."
                .to_string()
        }
    }
}

fn describe_rate_limit(body: &str) -> String {
    let pricing = "https://commitor.dev/pricing";
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let limit = v.get("limit").and_then(|l| l.as_u64());
            let reset_at = v
                .get("reset_at")
                .and_then(|r| r.as_str())
                .map(str::to_string);
            match (limit, reset_at) {
                (Some(limit), Some(reset_at)) => format!(
                    "You've hit your daily limit ({limit}/{limit} analyses) — resets at {reset_at}. \
                     Upgrade at {pricing} for more."
                ),
                _ => v
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|m| format!("{m} See {pricing} for higher limits."))
                    .unwrap_or_else(|| {
                        format!("You've hit your usage limit. See {pricing} for higher limits.")
                    }),
            }
        }
        Err(_) => format!(
            "You've hit your usage limit. Wait a bit and try again, or see {pricing} for higher limits."
        ),
    }
}

fn parse_rate_status(headers: &reqwest::header::HeaderMap) -> RateStatus {
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());
    RateStatus { remaining }
}

// ── wire format ─────────────────────────────────────────────────────

/// Matches `AnalyzeRequest` in commitor-api. `mode` is optional so
/// older backends simply ignore it.
#[derive(Serialize)]
pub struct AnalyzeRequest<'a> {
    pub diff: &'a str,
    pub context: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<&'a str>,
}

/// Matches `AnalyzeResponse`. Tolerant by design so newer backends
/// keep working with older CLIs.
///
/// There is no explicit mixed flag on the wire: a single group means
/// one logical change, multiple groups mean the changeset should be
/// split (see `groups.len()` at the call sites).
#[derive(Debug, Deserialize, Serialize)]
pub struct AnalyzeResponse {
    pub groups: Vec<ChangeGroup>,
    #[serde(default)]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub model_tier: Option<String>,
    /// AI-suggested kebab-case branch name (populated by backend
    /// mode="branch"); used to pre-fill `commitor commit -b`.
    #[serde(default)]
    pub branch_name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ChangeGroup {
    /// The file paths wholly belonging to this logical change. Paths
    /// that are split hunk-wise appear in `partial_files` instead.
    pub files: Vec<String>,
    /// The suggested commit message — doubles as the group's name.
    pub commit_message: String,
    /// Why these files belong together (shown to the user).
    pub rationale: String,
    /// Parts of files assigned to this group at hunk granularity.
    /// Absent on older backends → empty.
    #[serde(default)]
    pub partial_files: Vec<PartialFile>,
}

/// A file whose changes are split across commits at hunk level.
/// `hunks` are 1-based indices into that path's hunk sequence, in diff
/// order. Only meaningful together with the actual diff text, which
/// the CLI parses locally (`engine::hunks`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PartialFile {
    pub path: String,
    pub hunks: Vec<usize>,
}
