//! Shared working-diff analysis used by both `scan` and `commit`.
//!
//! Owns the three pieces every analysis-driven command needs:
//!
//! 1. diff collection (`git diff --staged`, falling back to unstaged
//!    with a warning, or unstaged outright with `--all`),
//! 2. the wire types for the backend's `/analyze` endpoint,
//! 3. the authenticated backend call itself (size guard + long
//!    timeout + friendly HTTP error mapping).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::auth::{self, DASHBOARD_URL};
use crate::config;
use crate::engine::git;

// Re-exported so commands resolve credentials through the analysis
// layer instead of importing `auth` directly.
pub use crate::auth::load_api_key;

/// The backend may route to a slow reasoning model; give it room
/// instead of failing mid-analysis.
const ANALYZE_TIMEOUT_SECS: u64 = 120;

/// The backend rejects diffs over 200k characters (`AnalyzeRequest`
/// `max_length`); stop just short of that so the request never 422s.
/// Applies to the full patch, including synthesized untracked sections.
const MAX_PATCH_CHARS: usize = 190_000;

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

/// Run the full backend analysis for `patch`.
///
/// Loads the stored API key (surfacing the standard not-logged-in
/// message when absent), applies the size guard, then POSTs to
/// `/analyze`. This is the entry point `scan` uses once its local
/// heuristics are inconclusive.
pub fn analyze_patch(patch: &str) -> Result<AnalyzeResponse> {
    let api_key = auth::load_api_key()?;
    analyze_with_key(&api_key, patch)
}

/// Same as [`analyze_patch`] for callers that already loaded the key
/// (e.g. `commit`, which requires auth before touching any state).
pub fn analyze_with_key(api_key: &str, patch: &str) -> Result<AnalyzeResponse> {
    if patch.chars().count() > MAX_PATCH_CHARS {
        bail!(
            "The change is too large for analysis (>200k characters).\n\
             Try scanning a narrower set of changes, or split it manually."
        );
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start async runtime")?;
    runtime.block_on(analyze_request(api_key, patch))
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
pub struct AnalyzeRequest<'a> {
    pub diff: &'a str,
    pub context: Option<&'a str>,
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
