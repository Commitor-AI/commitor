//! `commitor apply <code>` — redeem a split plan produced by the PR
//! analyzer.
//!
//! The GitHub bot analyses a PR, decides it bundles unrelated changes,
//! and stores the proposed groups under a short code. The user pastes
//! that code here. We:
//!
//! 1. fetch the plan (repo, base branch, groups) from the backend,
//! 2. check the local checkout matches the analyzed PR (remote repo and
//!    the diff against the merge base),
//! 3. create a NEW branch and reset it to the merge base (the working
//!    tree keeps the PR's content),
//! 4. commit each analyzer group in order using the same validated
//!    hunk-level machinery as `commitor commit`,
//! 5. push the new branch and open a replacement PR against the base.
//!
//! The user's original branch is never rewritten. If validation fails
//! before any commit is created, the new branch is rolled back so the
//! checkout is exactly as it was.

use std::io::{self, Write};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::analysis::{self, ChangeGroup};
use crate::auth;
use crate::commit;
use crate::config;
use crate::engine::git;
use crate::engine::hunks;

const API_TIMEOUT_SECS: u64 = 30;
const GITHUB_API: &str = "https://api.github.com";

/// The backend's `/plans/{code}` response.
#[derive(Debug, Deserialize)]
struct SplitPlan {
    code: String,
    repo_full_name: String,
    pr_number: u64,
    pr_title: String,
    base_ref: String,
    head_ref: String,
    base_sha: Option<String>,
    head_sha: Option<String>,
    groups: Vec<ChangeGroup>,
    applied: bool,
    applied_pr_url: Option<String>,
}

pub fn run(raw_code: String) -> Result<ExitCode> {
    let code = normalize_code(&raw_code);
    if code.is_empty() {
        bail!("No split-plan code given. Paste the code from the PR analysis, e.g. `commitor apply ABCD-EFGH-JKLM`.");
    }

    if !git::is_work_tree() {
        bail!("`commitor apply` must run inside the git checkout of the PR.");
    }

    let api_key = analysis::load_api_key()?;
    println!("Fetching split plan {}…", format_code(&code));
    let plan = fetch_plan(&api_key, &code)?;

    if plan.applied {
        match plan.applied_pr_url.as_deref() {
            Some(url) if !url.is_empty() => bail!(
                "This code was already applied — the replacement PR is {url}.\n\
                 Re-run the PR analysis to get a fresh code."
            ),
            _ => {
                bail!("This code was already applied. Re-run the PR analysis to get a fresh code.")
            }
        }
    }
    if plan.groups.len() < 2 {
        bail!(
            "This plan has fewer than two groups, so there is nothing to split.\n\
             Run `commitor commit` if you still want to create commits from the working tree."
        );
    }

    // ── 1. Does this checkout belong to the analyzed PR? ─────────────
    let remote = git::remote_url()?
        .filter(|url| !url.trim().is_empty())
        .context("This repo has no `origin` remote, so it can't be matched to the PR.")?;
    let repo = github_repo(&remote).with_context(|| {
        format!("Couldn't derive a GitHub repo from the `origin` remote: {remote}")
    })?;
    if !repo.eq_ignore_ascii_case(&plan.repo_full_name) {
        bail!(
            "This checkout is {repo}, but the split plan is for {}.\n\
             Run `commitor apply` in the matching repository.",
            plan.repo_full_name
        );
    }

    if !git::worktree_is_clean()? {
        bail!(
            "The working tree has uncommitted changes.\n\
             Commit or stash them first — `commitor apply` needs a clean checkout so it can\n\
             rebuild the PR's changes as separate commits without mixing in unrelated work."
        );
    }

    let original_branch = git::current_branch()?.context(
        "HEAD is detached. Check out the PR's branch first so `commitor apply` can return you \
         to it if anything goes wrong.",
    )?;
    let original_head = git::head_sha()?;
    if let Some(expected) = plan.head_sha.as_deref() {
        if !expected.eq_ignore_ascii_case(&original_head) {
            println!(
                "note: local HEAD is {} but the plan was analyzed at {} — validating the diff before doing anything.",
                short_sha(&original_head),
                short_sha(expected)
            );
        }
    }

    // ── 2. Reconstruct the PR diff against its merge base ────────────
    // A failed fetch isn't fatal: an existing origin/<base> is still
    // usable, and plan validation refuses a stale/mismatched diff.
    if let Err(err) = git::fetch_ref("origin", &plan.base_ref) {
        eprintln!(
            "warning: couldn't refresh origin/{} ({}); using the local ref",
            plan.base_ref, err
        );
    }
    let base = resolve_base(&plan)?;
    let base_point = git::merge_base(&base, "HEAD")
        .with_context(|| format!("Couldn't find the merge base of {base} and HEAD"))?;
    let pr_diff = analysis::collect_range(&format!("{base_point}..HEAD"))?;
    if pr_diff.files.is_empty() {
        bail!(
            "No changes found between {base} and HEAD — is this the PR's branch?\n\
             Fetch/check out the branch the PR was opened from and try again."
        );
    }
    reject_unsupported(&pr_diff.patch)?;

    let mut groups = plan.groups.clone();
    let actual_paths = pr_diff.files.iter().cloned().collect();
    commit::normalize_group_paths(&mut groups, &actual_paths);
    let plan_groups = commit::build_plan(&groups);
    let file_diffs = hunks::parse(&pr_diff.patch);
    hunks::validate(&file_diffs, &plan_groups, &pr_diff.files).context(
        "The split plan doesn't match this checkout's PR diff.\n\
         The branch may be out of date, or the PR changed since it was analyzed.\n\
         Update the branch (and re-run the PR analysis for a fresh code), then try again.",
    )?;

    let branch = format!("commitor/split-{}", code.to_ascii_lowercase());
    if git::branch_exists(&branch)? {
        bail!(
            "Branch '{branch}' already exists. Delete it (or rename it) and re-run, \
             or re-run the PR analysis for a fresh code."
        );
    }

    print_plan(&plan, &plan_groups, &branch, &original_branch);

    if !prompt_confirm("Create these commits on the new branch and open a PR? [y/N] ")? {
        println!("Cancelled — nothing was changed.");
        return Ok(ExitCode::SUCCESS);
    }

    // ── 3. Split on a new branch, leaving the original untouched ─────
    git::create_branch(&branch).with_context(|| format!("Couldn't create branch '{branch}'"))?;
    if let Err(err) = git::reset_mixed(&base_point) {
        restore_checkout(&original_head, &original_branch, &branch)?;
        return Err(err.context(format!(
            "Couldn't reset branch '{branch}' to the PR's merge base — nothing was committed."
        )));
    }

    // Validate again against the uncommitted working-tree diff; this is
    // the exact view the commit executor will stage from.
    let collected = analysis::collect_diff(true, true)?;
    if collected.files.is_empty() {
        restore_checkout(&original_head, &original_branch, &branch)?;
        bail!(
            "The PR's changes couldn't be reconstructed on the new branch — nothing was committed."
        );
    }
    reject_unsupported(&collected.patch)?;

    let mut groups = plan.groups.clone();
    let actual_paths = collected.files.iter().cloned().collect();
    commit::normalize_group_paths(&mut groups, &actual_paths);
    let mut plan_groups = commit::build_plan(&groups);
    let file_diffs = hunks::parse(&collected.patch);
    if let Err(err) = hunks::validate(&file_diffs, &plan_groups, &collected.files) {
        restore_checkout(&original_head, &original_branch, &branch)?;
        return Err(err.context(
            "The split plan doesn't match the PR's changes after reconstruction.\n\
             Nothing was committed and your original branch was restored.",
        ));
    }

    let baseline = git::status_porcelain()?;
    let exit = commit::execute_commits(
        &mut plan_groups,
        &file_diffs,
        false,
        &baseline,
        Some(&plan.base_ref),
        false,
    )?;
    if exit != ExitCode::SUCCESS {
        eprintln!(
            "The split stopped partway. Your original branch '{original_branch}' is unchanged; \
             the partial work is on '{branch}'."
        );
        return Ok(exit);
    }

    // Every PR change should now be in a commit. If anything is left
    // over, pushing would open a PR that silently drops it.
    if !git::worktree_is_clean()? {
        bail!(
            "The split created the commits but left uncommitted changes on '{branch}':\n{}\n\
             Nothing was pushed. Your original branch '{original_branch}' is unchanged — \
             inspect the branch with `git status`, or reset it and re-run.",
            git::status_porcelain()?
        );
    }

    // ── 4. Push and open the replacement PR ──────────────────────────
    println!("Pushing branch '{branch}'…");
    git::push_branch("origin", &branch).context(
        "The commits were created, but pushing the new branch failed. \
         Push it manually, then re-run with the same code or open the PR from the compare URL.",
    )?;

    let title = replacement_title(&plan);
    let body = replacement_body(&plan, &branch);
    match create_github_pr(&repo, &branch, &plan.base_ref, &title, &body) {
        Ok(url) => {
            println!();
            println!("Opened replacement PR: {url}");
            complete_plan(&api_key, &code, &url);
        }
        Err(err) => {
            eprintln!("warning: couldn't open the PR automatically — {err:#}");
            println!();
            println!("The branch is pushed. Open the PR here:");
            println!("  {}", compare_url(&repo, &plan.base_ref, &branch));
        }
    }

    Ok(ExitCode::SUCCESS)
}

// ── plan wire format ────────────────────────────────────────────────

fn normalize_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn format_code(stored: &str) -> String {
    // The backend already returns the grouped form (`ABCD-EFGH-JKLM`),
    // while locally we may hold either form; normalize before grouping so
    // this is idempotent.
    let normalized = normalize_code(stored);
    let mut parts = Vec::new();
    let mut chars = normalized.chars();
    loop {
        let chunk: String = chars.by_ref().take(4).collect();
        if chunk.is_empty() {
            break;
        }
        parts.push(chunk);
    }
    parts.join("-")
}

fn fetch_plan(api_key: &str, code: &str) -> Result<SplitPlan> {
    let url = format!("{}/plans/{code}", config::api_base_url());
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(API_TIMEOUT_SECS))
        .build()
        .context("failed to set up HTTP client")?;

    let response = auth::with_key(client.get(&url), api_key)
        .send()
        .map_err(|err| {
            anyhow!(
                "Couldn't reach the Commitor API at {url} ({}). \
                 Is your backend running? Set COMMITOR_API_URL if it lives elsewhere.",
                auth::root_cause(&err)
            )
        })?;

    let status = response.status();
    match status.as_u16() {
        200 => response
            .json()
            .with_context(|| format!("{url} returned a response that doesn't match the split-plan schema")),
        401 | 403 => bail!(
            "Your stored API key was rejected (HTTP {status}) — run `commitor login --key <key>` again."
        ),
        404 => bail!(
            "No split plan found for {}. Check the code copied from the PR analysis.",
            format_code(code)
        ),
        410 => bail!(
            "The code {} has expired. Re-run the PR analysis to get a fresh code.",
            format_code(code)
        ),
        _ => {
            let body = response.text().unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            if snippet.is_empty() {
                bail!("Commitor API returned HTTP {status} for {url}");
            }
            bail!("Commitor API returned HTTP {status} for {url}\nServer said: {snippet}")
        }
    }
}

/// Best-effort bookkeeping so the same code can't be applied twice.
fn complete_plan(api_key: &str, code: &str, pr_url: &str) {
    let url = format!("{}/plans/{code}/complete", config::api_base_url());
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(API_TIMEOUT_SECS))
        .build()
    else {
        return;
    };
    let _ = auth::with_key(client.post(&url), api_key)
        .json(&serde_json::json!({ "pr_url": pr_url }))
        .send();
}

// ── local git state ─────────────────────────────────────────────────

/// Prefer the fetched remote base branch, then the local one, then the
/// `base_sha` recorded with the plan.
fn resolve_base(plan: &SplitPlan) -> Result<String> {
    let remote = format!("origin/{}", plan.base_ref);
    if git::rev_parse(&remote).is_ok() {
        return Ok(remote);
    }
    if git::rev_parse(&plan.base_ref).is_ok() {
        return Ok(plan.base_ref.clone());
    }
    if let Some(sha) = plan.base_sha.as_deref() {
        if git::rev_parse(sha).is_ok() {
            return Ok(sha.to_string());
        }
    }
    bail!(
        "Base branch '{}' isn't available locally.\n\
         Run `git fetch origin {}` and try again.",
        plan.base_ref,
        plan.base_ref
    )
}

/// Renames turn into a delete/add pair after a mixed reset, so the
/// analyzer's atomic-file plan can't be mapped reliably. Refuse up front
/// rather than leaving a half-built branch behind.
fn reject_unsupported(patch: &str) -> Result<()> {
    for file in hunks::parse(patch) {
        let rename = file.header.iter().any(|line| {
            line.starts_with("rename from ")
                || line.starts_with("rename to ")
                || line.starts_with("copy from ")
                || line.starts_with("copy to ")
        });
        let submodule = file.header.iter().any(|line| line.contains(" 160000"))
            || file.hunks.iter().any(|hunk| {
                hunk.body_lines
                    .iter()
                    .any(|line| line.starts_with("Subproject commit "))
            });
        if rename {
            bail!(
                "Automatic apply doesn't support renames yet ({}).\n\
                 Split this PR manually, or re-run the analysis after renaming the file back.",
                file.path
            );
        }
        if submodule {
            bail!(
                "Automatic apply doesn't support submodule changes ({}). \
                 Split this PR manually.",
                file.path
            );
        }
    }
    Ok(())
}

/// Return the checkout to the branch/commit it started from after a
/// pre-commit failure, and discard the temporary split branch. The
/// working tree at this point still contains the PR's content, so a hard
/// reset to the original head restores exactly the state we found.
fn restore_checkout(original_head: &str, original_branch: &str, split_branch: &str) -> Result<()> {
    git::reset_hard(original_head)?;
    if git::current_branch()?.as_deref() != Some(original_branch) {
        git::checkout_branch(original_branch)?;
    }
    // The branch was only created for this run and has no commits of its
    // own yet, so removing it leaves the repository as we found it.
    let _ = git::delete_branch(split_branch);
    Ok(())
}

fn print_plan(plan: &SplitPlan, groups: &[hunks::PlanGroup], branch: &str, original: &str) {
    println!();
    println!(
        "Split plan for {}#{} — {} commits:",
        plan.repo_full_name,
        plan.pr_number,
        groups.len()
    );
    if !plan.pr_title.trim().is_empty() {
        println!("  \"{}\"", plan.pr_title.trim());
    }
    println!();
    for (index, group) in groups.iter().enumerate() {
        println!("{}. {}", index + 1, group.message);
        let files = commit::describe_group(group);
        if !files.is_empty() {
            println!("   files: {files}");
        }
        if let Some(rationale) = plan.groups.get(index).map(|g| g.rationale.trim()) {
            if !rationale.is_empty() {
                println!("   why:   {rationale}");
            }
        }
        println!();
    }
    println!("This will:");
    println!("  • create branch '{branch}' from the PR's merge base");
    println!(
        "  • rebuild PR #{}'s changes from '{}' as {} commits on that branch",
        plan.pr_number,
        plan.head_ref,
        groups.len()
    );
    println!(
        "  • push the branch and open a new PR against '{}'",
        plan.base_ref
    );
    println!("Your current branch '{original}' is left untouched.");
    println!();
}

// ── GitHub PR creation ──────────────────────────────────────────────

/// `owner/repo` from the common GitHub remote forms:
/// `git@github.com:owner/repo.git`, `https://github.com/owner/repo.git`,
/// `ssh://git@github.com/owner/repo.git`.
fn github_repo(remote: &str) -> Option<String> {
    let trimmed = remote.trim();
    let without_git = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    if let Some(rest) = without_git.strip_prefix("git@github.com:") {
        return owner_repo(rest);
    }
    if let Some(rest) = without_git.strip_prefix("ssh://git@github.com/") {
        return owner_repo(rest);
    }
    if let Some(rest) = without_git.strip_prefix("https://github.com/") {
        return owner_repo(rest);
    }
    if let Some(rest) = without_git.strip_prefix("http://github.com/") {
        return owner_repo(rest);
    }
    None
}

fn owner_repo(rest: &str) -> Option<String> {
    let parts: Vec<&str> = rest.trim_matches('/').split('/').collect();
    if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        Some(format!("{}/{}", parts[0], parts[1]))
    } else {
        None
    }
}

fn github_token() -> Option<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }

    // Fall back to the GitHub CLI's stored OAuth token, which is
    // already the authentication most developers have set up.
    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn create_github_pr(repo: &str, head: &str, base: &str, title: &str, body: &str) -> Result<String> {
    let token = github_token()
        .context("no GitHub token found — set GH_TOKEN or GITHUB_TOKEN, or run `gh auth login`")?;

    let url = format!("{GITHUB_API}/repos/{repo}/pulls");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to set up GitHub HTTP client")?;

    let response = client
        .post(&url)
        .header("User-Agent", "commitor-cli")
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(token)
        .json(&serde_json::json!({
            "title": title,
            "head": head,
            "base": base,
            "body": body,
        }))
        .send()
        .map_err(|err| {
            anyhow!(
                "couldn't reach the GitHub API at {url} ({})",
                auth::root_cause(&err)
            )
        })?;

    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        bail!("GitHub API returned HTTP {status}: {}", snippet(&text));
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&text).context("GitHub returned a malformed PR response")?;
    parsed
        .get("html_url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .context("GitHub returned no PR URL")
}

fn snippet(text: &str) -> String {
    text.chars().take(300).collect()
}

fn replacement_title(plan: &SplitPlan) -> String {
    let title = plan.pr_title.trim();
    if title.is_empty() {
        format!(
            "Split #{} into {} commits",
            plan.pr_number,
            plan.groups.len()
        )
    } else {
        format!("{title} (split)")
    }
}

fn replacement_body(plan: &SplitPlan, branch: &str) -> String {
    let commits = plan
        .groups
        .iter()
        .map(|group| format!("- {}", group.commit_message.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    let plan_code = format_code(&plan.code);
    format!(
        "Splits #{pr} into {count} focused commits so each change can be reviewed and reverted on its own.\n\n\
         {commits}\n\n\
         ---\n\
         Generated by `commitor apply` from the Commitor PR analysis (plan {plan}, branch `{branch}`).",
        pr = plan.pr_number,
        count = plan.groups.len(),
        plan = plan_code,
    )
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn compare_url(repo: &str, base: &str, head: &str) -> String {
    format!("https://github.com/{repo}/compare/{base}...{head}?expand=1")
}

// ── prompting ───────────────────────────────────────────────────────

fn prompt_confirm(question: &str) -> Result<bool> {
    print!("{question}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read your input")?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_grouped_codes() {
        assert_eq!(normalize_code("abcd-efgh-jklm"), "ABCDEFGHJKLM");
        assert_eq!(normalize_code("  AB-CD "), "ABCD");
        assert_eq!(format_code("ABCDEFGHJKLM"), "ABCD-EFGH-JKLM");
        assert_eq!(format_code("ABCD-EFGH-JKLM"), "ABCD-EFGH-JKLM");
    }

    #[test]
    fn parses_common_github_remotes() {
        for remote in [
            "git@github.com:acme/widgets.git",
            "https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets",
            "ssh://git@github.com/acme/widgets.git",
        ] {
            assert_eq!(
                github_repo(remote).as_deref(),
                Some("acme/widgets"),
                "{remote}"
            );
        }
        assert_eq!(github_repo("https://gitlab.com/acme/widgets.git"), None);
    }

    #[test]
    fn compare_url_uses_github_form() {
        assert_eq!(
            compare_url("acme/widgets", "main", "commitor/split-abcd"),
            "https://github.com/acme/widgets/compare/main...commitor/split-abcd?expand=1"
        );
    }

    #[test]
    fn rejects_renames() {
        let patch = "\
diff --git a/old.rs b/new.rs\n\
similarity index 100%\n\
rename from old.rs\n\
rename to new.rs\n";
        assert!(reject_unsupported(patch).is_err());
    }

    #[test]
    fn rejects_submodule_changes() {
        let patch = "\
diff --git a/vendor/lib b/vendor/lib\n\
index 1111111..2222222 160000\n\
--- a/vendor/lib\n\
+++ b/vendor/lib\n\
@@ -1 +1 @@\n\
-Subproject commit 1111111111111111111111111111111111111111\n\
+Subproject commit 2222222222222222222222222222222222222222\n";
        assert!(reject_unsupported(patch).is_err());
    }

    #[test]
    fn allows_plain_modifications() {
        let patch = "\
diff --git a/src/a.rs b/src/a.rs\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1 +1 @@\n\
-old\n\
+new\n";
        assert!(reject_unsupported(patch).is_ok());
    }
}
