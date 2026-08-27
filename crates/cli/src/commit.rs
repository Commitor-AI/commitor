//! `commitor commit` — turn the analyzed working diff into real git
//! commits, with the user approving every message before anything is
//! written to history.
//!
//! Unlike `scan`, this command ALWAYS calls the backend: local
//! heuristics can label a changeset ("docs-only change") but that is
//! not a usable commit message, so their verdict here is an
//! informational hint only, never a substitute for the AI response.
//!
//! Splitting granularity: whole files, plus hunk-level splits of
//! individual files when the backend assigns specific hunks
//! (`partial_files`). Atomic files (binary/new/deleted/renamed/
//! mode-only) always go whole. Before anything runs, the plan must
//! partition the ENTIRE diff — tracked and untracked, every hunk
//! claimed exactly once — otherwise the command refuses and asks for
//! a re-run rather than committing a mangled plan.
//!
// NOTE on mechanics: hunk-level staging uses `git apply --cached`
// with per-hunk patches; sequential applications rely on git's
// context/offset matching because later groups' patches carry line
// numbers from the original diff. Disjoint hunks anchor reliably;
// pathological overlaps would fail loudly and hit the partial-failure
// report. Untracked files never need patches (plain `git add`).

use std::collections::HashSet;
use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

use crate::analysis::{self, ChangeGroup};
use crate::engine::git;
use crate::engine::hunks::{self, FileDiff, PlanGroup};
use crate::heuristics::{self, Verdict};

#[derive(Debug, Default)]
pub struct CommitFlags {
    pub all: bool,
    /// Skip the AI entirely: build a basic local plan without needing
    /// an account, a backend, or remaining quota.
    pub offline: bool,
    /// Pick (or create) the branch to commit to before committing.
    pub branch: bool,
}

pub fn run(flags: CommitFlags) -> Result<ExitCode> {
    // ── 1. Collect the diff ─────────────────────────────────────────
    // Same collection rules as scan: staged unless --all, with the
    // same unstaged-fallback warning — plus untracked files folded in
    // as synthetic new-file sections (commit's expanded scope).
    let mut collected = analysis::collect_diff(flags.all, true)?;
    if collected.files.is_empty() {
        println!("Nothing to commit — no staged, unstaged, or untracked changes.");
        return Ok(ExitCode::SUCCESS);
    }

    // ── 0. Branch selection (optional) ─────────────────────────────
    // Switch to the chosen branch up front so every resulting commit
    // lands there. A name is suggested from the diff; uncommitted
    // changes are carried over by git when they don't conflict with
    // the target. After switching, re-collect so the diff reflects the
    // (possibly new) branch HEAD before anything is staged/committed.
    if flags.branch {
        // Prefer an AI-recommended name that reads the diff; fall back to
        // the local heuristic when offline, unauthenticated, or if the
        // backend can't suggest one. The suggestion only pre-fills the
        // "create a new branch" prompt — the user can still edit it.
        let suggestion = if flags.offline {
            suggest_branch_name(&collected)
        } else {
            match analysis::load_api_key() {
                Ok(key) => match analysis::analyze_with_mode(&key, &collected.patch, "branch") {
                    Ok((resp, _)) => resp
                        .branch_name
                        .map(|name| slugify(&name))
                        .filter(|name| !name.is_empty() && is_valid_branch_name(name))
                        .unwrap_or_else(|| suggest_branch_name(&collected)),
                    Err(_) => suggest_branch_name(&collected),
                },
                Err(_) => suggest_branch_name(&collected),
            }
        };
        select_branch(&suggestion)?;
        collected = analysis::collect_diff(flags.all, true)?;
        if collected.files.is_empty() {
            println!("Nothing to commit after switching branches.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    println!(
        "Analyzing {} changed file(s) ({})…",
        collected.files.len(),
        if collected.staged_used { "staged" } else { "unstaged" }
    );

    // Snapshot of tracked-change state at plan time, for the
    // stale-plan guard below.
    let baseline = git::status_porcelain()?;

    // Explicit --offline: no account, no backend, no quota needed.
    if flags.offline {
        println!("Offline mode — building a basic local plan (no AI).");
        return commit_offline(&collected, &baseline);
    }

    // ── 2. Auth + local hint ────────────────────────────────────────
    let api_key = analysis::load_api_key()?;
    let local_hint = match heuristics::evaluate(&collected.files) {
        Verdict::Inconclusive { reason } => Some(reason),
        Verdict::Clean { .. } => None,
    };

    // ── 3. Backend analysis — always, even if heuristics look clean ─
    // mode="commit": the backend never answers with its deterministic
    // tier; commits get model-written messages.
    let (response, rate) = match analysis::analyze_with_mode(&api_key, &collected.patch, "commit") {
        Ok(ok) => ok,
        // Quota gone or AI unreachable: degrade to an offline plan so a
        // commit is never blocked by the service. Auth problems are NOT
        // degraded — they need fixing.
        Err(analysis::AnalyzeError::RateLimited(reason))
        | Err(analysis::AnalyzeError::Unavailable(reason)) => {
            println!();
            println!("AI analysis unavailable — {reason}");
            println!("Falling back to an offline plan; rerun later for an AI-crafted message.");
            return commit_offline(&collected, &baseline);
        }
        Err(err) => return Err(err.into()),
    };

    // The plan must describe the tree as it is right now; bail before
    // showing the user anything built on stale data.
    ensure_tree_unchanged(&baseline)?;

    // ── 4. Build and validate the plan BEFORE showing or running it ─
    let file_diffs = hunks::parse(&collected.patch);
    let mut plan = build_plan(&response.groups);

    if let Err(err) = hunks::validate(&file_diffs, &plan, &collected.files) {
        // The model's proposed split was internally inconsistent (e.g.
        // a file claimed by two groups, or a partial claim that left a
        // hunk unassigned). Rather than refuse and force the user into
        // an endless re-run loop, degrade safely: commit everything in
        // a single commit — reusing the same approved single-commit
        // flow, so no change is ever lost or duplicated.
        println!();
        println!(
            "note: the suggested split was inconsistent ({err}), so all changes\n\
             will be committed in a single commit instead of being refused.\n\
             Re-run later (or use `commitor commit --offline`) for an AI split."
        );
        let mut single = vec![PlanGroup {
            message: offline_commit_message(&collected),
            whole: collected.files.clone(),
            partial: Vec::new(),
        }];
        return commit_single(
            &mut single,
            &file_diffs,
            collected.staged_used,
            &baseline,
        );
    }

    // ── 5. Act on the verdict ───────────────────────────────────────
    let code = if response.groups.len() <= 1 {
        commit_single(&mut plan, &file_diffs, collected.staged_used, &baseline)?
    } else {
        commit_split(
            &response.groups,
            local_hint.as_deref(),
            &mut plan,
            &file_diffs,
            collected.staged_used,
            &baseline,
        )?
    };

    // Soft quota hint, only after a fully successful run (stderr, so
    // scripted consumers of stdout are unaffected).
    if code == ExitCode::SUCCESS {
        if let Some(message) = rate.low_quota_message() {
            eprintln!();
            eprintln!("{message}");
        }
    }

    Ok(code)
}

/// Build a single-group plan locally: every changed file whole, one
/// commit, message derived from the diff headers. Used for explicit
/// `--offline` runs and as the automatic fallback when the AI is
/// unavailable (quota exhausted, backend down). Splitting needs the
/// model — offline always proposes exactly one commit.
fn commit_offline(collected: &analysis::CollectedDiff, baseline: &str) -> Result<ExitCode> {
    ensure_tree_unchanged(baseline)?;
    let file_diffs = hunks::parse(&collected.patch);
    let mut plan = vec![PlanGroup {
        message: offline_commit_message(collected),
        whole: collected.files.clone(),
        partial: Vec::new(),
    }];
    if let Err(err) = hunks::validate(&file_diffs, &plan, &collected.files) {
        bail!(
            "The offline plan doesn't match the actual diff:\n  {err:#}\n\n\
             Nothing was staged or committed."
        );
    }
    commit_single(&mut plan, &file_diffs, collected.staged_used, baseline)
}

/// Derive a short, human-readable list of "verb file" actions from the
/// patch (add/update/remove per file). Shared by the offline commit
/// message and the `commit -b` branch-name suggestion.
fn diff_actions(collected: &analysis::CollectedDiff) -> Vec<String> {
    let mut actions: Vec<String> = Vec::new();
    let mut old_path: Option<&str> = None;

    let summarize = |path: &str| -> String {
        path.trim_start_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .to_string()
    };

    for line in collected.patch.lines() {
        if line.starts_with("diff --git ") {
            old_path = None;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            old_path = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("+++ b/") {
            let name = summarize(rest);
            let is_new = matches!(old_path, None | Some("/dev/null"));
            let verb = if is_new { "add" } else { "update" };
            push_unique(&mut actions, format!("{verb} {name}"));
        } else if line.starts_with("+++ /dev/null") {
            let name = old_path.map(summarize).unwrap_or_else(|| "file".into());
            push_unique(&mut actions, format!("remove {name}"));
        }
    }

    actions
}

/// Deterministic message naming what each file does ("add auth.py;
/// update api.py"), prefixed with the common top-level directory when
/// there is one. Same spirit as the backend's local tier.
fn offline_commit_message(collected: &analysis::CollectedDiff) -> String {
    let actions = diff_actions(collected);
    if actions.is_empty() {
        return "chore: update working changes".to_string();
    }

    let listed: Vec<String> = actions.iter().take(5).cloned().collect();
    let mut summary = listed.join("; ");
    if actions.len() > 5 {
        summary += &format!("; +{} more", actions.len() - 5);
    }

    let tops: Vec<&str> = collected
        .files
        .iter()
        .filter_map(|p| p.split('/').next())
        .collect();
    if !tops.is_empty() && tops.iter().all(|t| *t == tops[0]) && !tops[0].is_empty() {
        format!("chore({}): {}", tops[0], summary)
    } else {
        format!("chore: {summary}")
    }
}

/// Suggest a branch name from the diff, the same way the offline commit
/// message is derived, but lowercased and hyphenated into a valid,
/// branch-safe slug (e.g. "add auth.py; update api.py" →
/// "add-auth-py-update-api-py").
fn suggest_branch_name(collected: &analysis::CollectedDiff) -> String {
    let actions = diff_actions(collected);
    if actions.is_empty() {
        return "changes".to_string();
    }

    let slug = slugify(&actions.join(" "));
    if slug.is_empty() {
        "changes".to_string()
    } else {
        slug
    }
}

/// Turn arbitrary text into a lowercase, hyphen-separated slug using
/// only `[a-z0-9-]`, collapsing runs and trimming edge hyphens.
fn slugify(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed: String = out.trim_matches('-').to_string();
    trimmed.chars().take(50).collect()
}

fn push_unique(actions: &mut Vec<String>, action: String) {
    if !actions.contains(&action) {
        actions.push(action);
    }
}

/// Map backend groups onto executable plan groups: `partial_files`
/// become hunk claims; `files` stay whole unless that path is split
/// somewhere in the plan (validator catches real ambiguity).
fn build_plan(groups: &[ChangeGroup]) -> Vec<PlanGroup> {
    let partially_claimed: HashSet<&str> = groups
        .iter()
        .flat_map(|group| group.partial_files.iter().map(|pf| pf.path.as_str()))
        .collect();

    groups
        .iter()
        .map(|group| {
            let mut whole = Vec::new();
            let mut seen: HashSet<&str> = HashSet::new();
            for path in &group.files {
                if !partially_claimed.contains(path.as_str()) && seen.insert(path.as_str()) {
                    whole.push(path.clone());
                }
            }
            PlanGroup {
                message: group.commit_message.clone(),
                whole,
                partial: group
                    .partial_files
                    .iter()
                    .map(|pf| (pf.path.clone(), pf.hunks.clone()))
                    .collect(),
            }
        })
        .collect()
}

/// Single group → one commit with the (editable) suggested message.
fn commit_single(
    plan: &mut [PlanGroup],
    file_diffs: &[FileDiff],
    staged_used: bool,
    baseline: &str,
) -> Result<ExitCode> {
    let Some(group) = plan.first_mut() else {
        bail!(
            "The analysis returned no suggestions — nothing to base a commit message on.\n\
             Try again, or commit manually with `git commit -m`."
        );
    };

    let message = group.message.trim().to_string();
    if message.is_empty() {
        bail!(
            "The analysis returned an empty commit message — try again,\n\
             or commit manually with `git commit -m`."
        );
    }

    println!();
    println!("Proposed commit message:");
    println!("  {message}");

    loop {
        match prompt_choice("\nUse this message? [a]ccept · [e]dit · [c]ancel (a/Enter): ")? {
            Choice::Accept => break,
            Choice::Edit => {
                let replacement = prompt_line("New message: ")?;
                if replacement.is_empty() {
                    println!("Empty input — keeping the current message.");
                } else {
                    group.message = replacement.clone();
                }
                println!();
                println!("Message:");
                println!("  {}", group.message);
            }
            Choice::Cancel => {
                println!("Commit cancelled, nothing was changed.");
                return Ok(ExitCode::SUCCESS);
            }
        }
    }

    execute_commits(plan, file_diffs, staged_used, baseline)
}

/// Multiple groups → show the proposed split and commit each group in
/// order after all-or-nothing approval.
fn commit_split(
    groups: &[ChangeGroup],
    local_hint: Option<&str>,
    plan: &mut [PlanGroup],
    file_diffs: &[FileDiff],
    staged_used: bool,
    baseline: &str,
) -> Result<ExitCode> {
    println!();
    println!(
        "This changeset looks mixed — proposed split into {} commits:",
        plan.len()
    );
    if let Some(hint) = local_hint {
        println!("  local hint: {hint}");
    }
    println!();

    for (index, group) in plan.iter().enumerate() {
        let number = index + 1;
        println!("{number}. {}", group.message);
        let files_display = describe_group(group);
        if !files_display.is_empty() {
            println!("   files: {files_display}");
        }
        if let Some(rationale) = groups.get(index).map(|g| g.rationale.as_str()) {
            if !rationale.is_empty() {
                println!("   why:   {rationale}");
            }
        }
        println!();
    }

    if plan.iter().any(|group| !group.partial.is_empty()) {
        let total_hunks: usize = file_diffs.iter().map(|f| f.hunks.len()).sum();
        println!(
            "All {total_hunks} hunks across {} changed files are accounted for.",
            file_diffs.len()
        );
        println!();
    }

    loop {
        match prompt_choice("Approve whole plan? [a]pprove · [e]dit messages · [c]ancel (a): ")? {
            Choice::Accept => break,
            Choice::Edit => {
                edit_plan_messages(plan);
                // Fall through to re-prompt so the edited plan still
                // needs an explicit approval before any git call.
            }
            Choice::Cancel => {
                println!("Commit cancelled, nothing was changed.");
                return Ok(ExitCode::SUCCESS);
            }
        }
    }

    execute_commits(plan, file_diffs, staged_used, baseline)
}

/// One line listing a group's contents: whole paths plain, partial
/// ones annotated with their hunk numbers.
fn describe_group(group: &PlanGroup) -> String {
    let mut parts: Vec<String> = group.whole.clone();
    for (path, hunk_ids) in &group.partial {
        parts.push(format!("{} (hunks {})", path, join_ids(hunk_ids)));
    }
    parts.join(", ")
}

fn join_ids(ids: &[usize]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Let the user rewrite each group's message, one at a time. An empty
/// answer keeps the current message for that group.
fn edit_plan_messages(plan: &mut [PlanGroup]) {
    let total = plan.len();
    for (index, group) in plan.iter_mut().enumerate() {
        let number = index + 1;
        let current = group.message.clone();
        let answer = prompt_line(&format!(
            "Message {number}/{total} (current: \"{current}\"): "
        ));
        if let Ok(replacement) = answer {
            if !replacement.is_empty() {
                group.message = replacement;
            }
        }
    }
    println!();
    println!("Updated plan:");
    for (index, group) in plan.iter().enumerate() {
        println!("{}. {} — files: {}", index + 1, group.message, describe_group(group));
    }
}

/// Run the approved plan: stage each group's share (hunk patches via
/// `git apply --cached`, whole files via `git add`), then commit, in
/// order, stopping at the first failure.
///
/// No automatic rollback on failure — history already written stays,
/// and everything not yet committed is reported below so `git status`
/// / `git log` tell the user exactly where things stand.
fn execute_commits(
    plan: &mut [PlanGroup],
    file_diffs: &[FileDiff],
    staged_used: bool,
    baseline: &str,
) -> Result<ExitCode> {
    // Final sanity check immediately before mutating anything.
    ensure_tree_unchanged(baseline)?;

    // Guard A: committing the UNSTAGED flavor sweeps in whatever is
    // already in the index (every group's `git commit` captures the
    // full index). If unrelated work is parked there, refuse instead
    // of silently bundling it into the first commit.
    if !staged_used && has_other_staged_work()? {
        bail!(
            "You have other staged-but-uncommitted changes outside what was analyzed.\n\
             Commit or stash them first — otherwise they would be swept into these\n\
             commits as a side effect. Then re-run `commitor commit`."
        );
    }

    // Guard B: hunk patches are written against a clean index via
    // `git apply --cached`. On the staged flavor the index already
    // contains EVERY change, so rebuilding is required first; the end
    // state after all commits matches what the user staged. Whole-file
    // plans skip this (`git add` is idempotent there).
    let has_partial = plan.iter().any(|group| !group.partial.is_empty());
    if staged_used && has_partial {
        println!("Splitting hunks separately — rebuilding the index first…");
        git::reset_index()?;
    }

    let total = plan.len();
    let mut committed: Vec<String> = Vec::new();

    for (index, group) in plan.iter().enumerate() {
        let number = index + 1;

        if !group.partial.is_empty() {
            let patch: String = group
                .partial
                .iter()
                .map(|claim| hunks::build_patch(file_diffs, claim))
                .collect();
            if let Err(err) = git::apply_cached(&patch) {
                eprintln!("error: couldn't stage hunks for commit {number}/{total}: {err:#}");
                return report_partial_failure(&committed, &plan[index..]);
            }
        }

        if !group.whole.is_empty() {
            if let Err(err) = git::add(&group.whole) {
                eprintln!("error: couldn't stage files for commit {number}/{total}: {err:#}");
                return report_partial_failure(&committed, &plan[index..]);
            }
        }

        match git::commit(&group.message) {
            Ok(output) => {
                println!("[{number}/{total}] Committed: {}", group.message);
                let summary = output.trim();
                if !summary.is_empty() {
                    for line in summary.lines() {
                        println!("  {line}");
                    }
                }
                committed.push(group.message.clone());
            }
            Err(err) => {
                eprintln!("error: {err:#}");
                return report_partial_failure(&committed, &plan[index..]);
            }
        }
    }

    println!();
    println!("Done — created {total} commit(s).");
    Ok(ExitCode::SUCCESS)
}

/// True when any status entry has a non-space INDEX column (X), i.e.
/// something is staged. `-uno` already excludes untracked lines.
fn has_other_staged_work() -> Result<bool> {
    let status = git::status_porcelain()?;
    Ok(status
        .lines()
        .any(|line| !line.is_empty() && !line.starts_with(' ')))
}

/// Explain where a partially-executed split stopped. Prints which
/// commits landed and which files were left behind; never rolls back.
fn report_partial_failure(committed: &[String], pending: &[PlanGroup]) -> Result<ExitCode> {
    println!();
    if committed.is_empty() {
        println!("No commits were created.");
    } else {
        println!("Commits that succeeded before the failure:");
        for message in committed {
            println!("  - {message}");
        }
    }

    println!("Files not included in any commit:");
    for group in pending {
        for path in &group.whole {
            println!("  - {path}");
        }
        for (path, hunk_ids) in &group.partial {
            println!("  - {path} (hunks {})", join_ids(hunk_ids));
        }
    }
    println!("Some of them may already be staged — `git status` shows which.");

    println!();
    println!("The split stopped here and was NOT rolled back.");
    println!("Run `git status` and `git log` to see exactly where things stand,");
    println!("then finish the remaining commits manually or fix the issue and");
    println!("run `commitor commit` again.");

    Ok(ExitCode::FAILURE)
}

/// Refuse to act when the working tree moved since the diff was
/// collected — the approved plan would no longer match reality.
fn ensure_tree_unchanged(baseline: &str) -> Result<()> {
    let current = git::status_porcelain()?;
    if current != baseline {
        bail!(
            "The working tree changed since the analysis started.\n\
             Re-run `commitor commit` so the plan matches the current diff\n\
             instead of committing against a stale one."
        );
    }
    Ok(())
}

// ── prompting ───────────────────────────────────────────────────────

enum Choice {
    Accept,
    Edit,
    Cancel,
}

/// Read a single-line answer from stdin, trimmed.
fn prompt_line(question: &str) -> Result<String> {
    print!("{question}");
    io::stdout().flush()?;

    let mut buf = String::new();
    io::stdin()
        .read_line(&mut buf)
        .context("failed to read your input")?;
    Ok(buf.trim().to_string())
}

/// Read an a/e/c menu answer.
///
/// Enter accepts (the common case). True EOF on stdin (no bytes read
/// at all) cancels rather than accepts, so piping/closing stdin can
/// never auto-commit anything.
fn prompt_choice(question: &str) -> Result<Choice> {
    loop {
        print!("{question}");
        io::stdout().flush()?;

        let mut buf = String::new();
        io::stdin()
            .read_line(&mut buf)
            .context("failed to read your input")?;
        if buf.is_empty() {
            return Ok(Choice::Cancel);
        }

        match buf.trim() {
            "" | "a" | "A" => return Ok(Choice::Accept),
            "e" | "E" => return Ok(Choice::Edit),
            "c" | "C" => return Ok(Choice::Cancel),
            _ => println!("Please answer a, e, or c (Enter = accept)."),
        }
    }
}

/// Interactive branch picker for `commitor commit -b`.
///
/// Lists every local branch (marking the current one) plus a "create
/// new branch" entry, then checks out the selection so the commits
/// created by the rest of the run land on it. Loops on invalid input
/// or git failures rather than aborting the whole commit. `suggestion`
/// is a diff-derived name offered (via Tab or Enter) when creating a
/// new branch.
fn select_branch(suggestion: &str) -> Result<()> {
    let (branches, current) = git::list_branches()?;

    loop {
        println!();
        println!("Choose a branch to commit to:");
        for (index, name) in branches.iter().enumerate() {
            if current.as_deref() == Some(name.as_str()) {
                println!("  {}: {} (current)", index + 1, name);
            } else {
                println!("  {}: {}", index + 1, name);
            }
        }
        let new_index = branches.len() + 1;
        println!("  {}: create a new branch", new_index);
        println!();

        let answer = prompt_line(&format!(
            "Enter a number [1-{}] (or 'n' for a new branch): ",
            new_index
        ))?;
        let answer = answer.trim();

        // Empty input (e.g. piped EOF) — skip selection rather than
        // looping forever on a closed stdin.
        if answer.is_empty() {
            println!("No selection — committing on the current branch.");
            return Ok(());
        }

        if answer.eq_ignore_ascii_case("n") || answer == new_index.to_string() {
            match read_branch_name(suggestion)? {
                Some(name) if !name.trim().is_empty() => {
                    let name = name.trim();
                    if !is_valid_branch_name(name) {
                        println!("'{name}' is not a valid branch name.");
                        continue;
                    }
                    match git::create_branch(name) {
                        Ok(()) => {
                            println!("Switched to new branch '{name}'.");
                            return Ok(());
                        }
                        Err(err) => {
                            println!("Couldn't create branch: {err:#}");
                            continue;
                        }
                    }
                }
                Some(_) => {
                    println!("Branch name can't be empty.");
                    continue;
                }
                // Ctrl-C / abort: keep the current branch and proceed.
                None => {
                    println!("Branch selection skipped — committing on the current branch.");
                    return Ok(());
                }
            }
        } else if let Ok(num) = answer.parse::<usize>() {
            if num >= 1 && num <= branches.len() {
                let name = &branches[num - 1];
                if current.as_deref() == Some(name.as_str()) {
                    println!("Already on '{name}'.");
                    return Ok(());
                }
                match git::checkout_branch(name) {
                    Ok(()) => {
                        println!("Switched to branch '{name}'.");
                        return Ok(());
                    }
                    Err(err) => {
                        println!("Couldn't switch: {err:#}");
                        println!("Commit or stash the conflicting changes first.");
                        continue;
                    }
                }
            } else {
                println!("Please enter a number between 1 and {new_index}.");
            }
        } else {
            println!("Please enter a number or 'n'.");
        }
    }
}

/// Lightweight git branch-name check: rejects the paths and characters
/// git itself forbids. Real validation still happens in git, so this
/// is just a friendlier early error.
fn is_valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("..")
        && !name
            .chars()
            .any(|c| matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

/// Read a branch name from the user, with a diff-derived `suggestion`.
///
/// Uses a real line editor (rustyline) so **Tab** behaves like a
/// shell: it completes the input against the suggested name and also
/// shows it as a grey inline hint, while the user can freely type
/// their own name instead. Ctrl-C / Ctrl-D aborts the picker.
///
/// Returns `Ok(None)` when the user aborts; callers should treat that
/// as "skip branch selection".
fn read_branch_name(suggestion: &str) -> io::Result<Option<String>> {
    use rustyline::completion::{Completer, Pair};
    use rustyline::hint::Hinter;
    use rustyline::highlight::Highlighter;
    use rustyline::validate::Validator;
    use rustyline::{CompletionType, Context, Editor, Helper, Result as RlResult};
    use rustyline::history::DefaultHistory;
    use std::borrow::Cow;

    /// Offers the diff-derived suggestion(s) as Tab completions and as
    /// an inline (grey) hint, exactly like a shell would.
    struct BranchCompleter {
        candidates: Vec<String>,
    }

    impl Completer for BranchCompleter {
        type Candidate = Pair;

        fn complete(
            &self,
            line: &str,
            _pos: usize,
            _ctx: &Context<'_>,
        ) -> RlResult<(usize, Vec<Pair>)> {
            let matches: Vec<Pair> = self
                .candidates
                .iter()
                .filter(|c| c.starts_with(line))
                .map(|c| Pair {
                    display: c.clone(),
                    replacement: c.clone(),
                })
                .collect();
            // Complete against the whole current input.
            Ok((0, matches))
        }
    }

    impl Hinter for BranchCompleter {
        type Hint = String;

        fn hint(&self, line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
            self.candidates
                .iter()
                .find(|c| c.starts_with(line) && c.as_str() != line)
                .map(|c| c[line.len()..].to_string())
        }
    }

    impl Highlighter for BranchCompleter {
        // Render the suggestion as a dim/transparent inline hint so it
        // reads as a ghost of what Tab will fill in, not normal text.
        fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
            Cow::Owned(format!("\x1b[2m{hint}\x1b[0m"))
        }

        // Keep the listed completion (if ever shown) consistent with
        // the dimmed hint style.
        fn highlight_candidate<'c>(
            &self,
            candidate: &'c str,
            _completion: CompletionType,
        ) -> Cow<'c, str> {
            Cow::Owned(format!("\x1b[2m{candidate}\x1b[0m"))
        }
    }
    impl Validator for BranchCompleter {}
    impl Helper for BranchCompleter {}

    let candidates = branch_candidates(suggestion);
    let mut editor =         Editor::<BranchCompleter, DefaultHistory>::new()
        .map_err(io::Error::other)?;
    editor.set_helper(Some(BranchCompleter { candidates }));

    println!("Tab to complete, or type your own; Ctrl-C to skip");
    match editor.readline("New branch name: ") {
        Ok(line) => {
            let name = line.trim().to_string();
            if name.is_empty() {
                Ok(Some(suggestion.to_string()))
            } else {
                Ok(Some(name))
            }
        }
        Err(rustyline::error::ReadlineError::Interrupted) => Ok(None),
        Err(rustyline::error::ReadlineError::Eof) => Ok(None),
        Err(e) => Err(io::Error::other(e)),
    }
}

/// Suggested branch-name completions offered on Tab. This is currently
/// the single diff-derived suggestion (mirroring the offline commit
/// message), but returning several candidates here would let Tab
/// cycle/list them shell-style.
fn branch_candidates(suggestion: &str) -> Vec<String> {
    vec![suggestion.to_string()]
}

