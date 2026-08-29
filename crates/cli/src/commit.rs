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

use std::collections::{BTreeMap, HashSet};
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

/// Total attempts at the AI backend before we give up and degrade to the
/// offline plan (initial try + retry prompts).
const MAX_AI_ATTEMPTS: usize = 4;
/// Total passes at building/validating a plan before we stop offering to
/// retry and fall back to a single offline commit.
const MAX_PLAN_ATTEMPTS: usize = 3;

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

    // Captured before any `-b` branch switch so we can record what the new
    // branch was forked from in the session log.
    let starting_branch = git::current_branch()?;

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
                Ok(key) => match analysis::analyze_with_mode(&key, &collected.patch, "branch", None) {
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

    // The branch the commits will land on is the current one; if `-b` was
    // used, the branch we forked from is what we record as `base_branch`.
    let base_branch = if flags.branch {
        starting_branch
    } else {
        None
    };

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
        return commit_offline(&collected, &baseline, base_branch.as_deref());
    }

    // ── 2. Auth + local hint ────────────────────────────────────────
    let api_key = analysis::load_api_key()?;
    let local_hint = match heuristics::evaluate(&collected.files) {
        Verdict::Inconclusive { reason } => Some(reason),
        Verdict::Clean { .. } => None,
    };

    // ── 3. Backend analysis — always, even if heuristics look clean ─
    // mode="commit": the backend never answers with its deterministic
    // tier; commits get model-written messages. Transient service errors
    // (quota/availability) prompt a retry rather than silently degrading.
    let (mut response, mut rate) = {
        let mut attempts = 0;
        loop {
            attempts += 1;
            match analysis::analyze_with_mode(&api_key, &collected.patch, "commit", None) {
                Ok(ok) => break ok,
                // Quota gone or AI unreachable: offer to retry, fall back
                // to an offline plan, or cancel — never block the commit
                // silently. Auth problems are NOT retried; they need fixing.
                Err(analysis::AnalyzeError::RateLimited(reason))
                | Err(analysis::AnalyzeError::Unavailable(reason)) => {
                    if attempts >= MAX_AI_ATTEMPTS {
                        println!();
                        println!("AI analysis still unavailable after {attempts} tries — {reason}");
                        println!("Falling back to an offline plan; rerun later for an AI-crafted message.");
                        return commit_offline(&collected, &baseline, base_branch.as_deref());
                    }
                    match prompt_retry(&format!("AI analysis unavailable — {reason}"))? {
                        RetryDecision::Retry => continue,
                        RetryDecision::Offline => {
                            println!("Falling back to an offline plan.");
                            return commit_offline(&collected, &baseline, base_branch.as_deref());
                        }
                        RetryDecision::Cancel => {
                            println!("Commit cancelled, nothing was changed.");
                            return Ok(ExitCode::SUCCESS);
                        }
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }
    };

    // The plan must describe the tree as it is right now; bail before
    // showing the user anything built on stale data.
    ensure_tree_unchanged(&baseline)?;

    // ── 4/5. Build, validate, and act on the plan ──────────────────
    // The model can return an internally inconsistent split (a file
    // claimed twice, or a hunk left unassigned). On failure we offer to
    // re-request a fresh split (the model is nondeterministic) before
    // degrading to the offline plan.
    let mut plan_attempt: usize = 0;
    loop {
        let file_diffs = hunks::parse(&collected.patch);
        // The model sometimes returns abbreviated paths (a basename, or a
        // `./`/`b/` prefix) that don't match the diff verbatim. Repair them
        // against the actual changed-file set so a salvageable AI split isn't
        // thrown away and forced into the coarse offline fallback.
        let actual_paths: HashSet<String> = collected.files.iter().cloned().collect();
        normalize_group_paths(&mut response.groups, &actual_paths);
        let mut plan = build_plan(&response.groups);

        if let Err(err) = hunks::validate(&file_diffs, &plan, &collected.files) {
            plan_attempt += 1;
            if plan_attempt >= MAX_PLAN_ATTEMPTS {
                println!();
                println!(
                    "note: the suggested split was still inconsistent ({err}), so all changes\n\
                     will be committed in a single commit instead of being refused.\n\
                     Re-run later (or use `commitor commit --offline`) for an AI split."
                );
                let plan = offline_groups(&collected);
                return run_offline_plan(plan, &file_diffs, collected.staged_used, &baseline, base_branch.as_deref());
            }
            match prompt_retry(&format!(
                "The suggested split was inconsistent ({err}). Retry for a fresh split?"
            ))? {
                RetryDecision::Retry => {
                    println!("Re-requesting an analysis…");
                    match analysis::analyze_with_mode(&api_key, &collected.patch, "commit", None) {
                        Ok(ok) => {
                            response = ok.0;
                            rate = ok.1;
                            continue;
                        }
                        Err(analysis::AnalyzeError::RateLimited(reason))
                        | Err(analysis::AnalyzeError::Unavailable(reason)) => {
                            println!("AI unavailable ({reason}) — falling back to an offline plan.");
                            let plan = offline_groups(&collected);
                            return run_offline_plan(plan, &file_diffs, collected.staged_used, &baseline, base_branch.as_deref());
                        }
                        Err(err) => return Err(err.into()),
                    }
                }
                RetryDecision::Offline => {
                    let plan = offline_groups(&collected);
                    return run_offline_plan(plan, &file_diffs, collected.staged_used, &baseline, base_branch.as_deref());
                }
                RetryDecision::Cancel => {
                    println!("Commit cancelled, nothing was changed.");
                    return Ok(ExitCode::SUCCESS);
                }
            }
        }

        let code = if response.groups.len() <= 1 {
            commit_single(&mut plan, &file_diffs, collected.staged_used, &baseline, base_branch.as_deref())?
        } else {
            commit_split(
                &response.groups,
                local_hint.as_deref(),
                &mut plan,
                &file_diffs,
                collected.staged_used,
                &baseline,
                base_branch.as_deref(),
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

        return Ok(code);
    }
}

/// Build a single-group plan locally: every changed file whole, one
/// commit, messages derived from the diff structure. Used for explicit
/// `--offline` runs and as the automatic fallback when the AI is
/// unavailable (quota exhausted, backend down). It splits the diff into
/// one commit per `(type, scope)` (features, fixes, and the remainder in
/// their own commits) using only local heuristics.
fn commit_offline(
    collected: &analysis::CollectedDiff,
    baseline: &str,
    base_branch: Option<&str>,
) -> Result<ExitCode> {
    ensure_tree_unchanged(baseline)?;
    let file_diffs = hunks::parse(&collected.patch);
    let plan = offline_groups(collected);
    run_offline_plan(plan, &file_diffs, collected.staged_used, baseline, base_branch)
}

/// Validate an offline-derived plan and execute it, committing each
/// `(type, scope)` group separately when there's more than one.
fn run_offline_plan(
    mut plan: Vec<PlanGroup>,
    file_diffs: &[FileDiff],
    staged_used: bool,
    baseline: &str,
    base_branch: Option<&str>,
) -> Result<ExitCode> {
    if let Err(err) = hunks::validate(file_diffs, &plan, &plan_files(&plan)) {
        bail!(
            "The offline plan doesn't match the actual diff:\n  {err:#}\n\n\
             Nothing was staged or committed."
        );
    }
    if plan.len() <= 1 {
        commit_single(&mut plan, file_diffs, staged_used, baseline, base_branch)
    } else {
        commit_split(&[], None, &mut plan, file_diffs, staged_used, baseline, base_branch)
    }
}

/// Flatten the file list across all groups, for plan validation.
fn plan_files(plan: &[PlanGroup]) -> Vec<String> {
    let mut files = Vec::new();
    for group in plan {
        files.extend(group.whole.iter().cloned());
    }
    files
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

/// Deterministic, Conventional-Commits plan derived purely from the diff
/// — used by the offline path and the "inconsistent plan" fallback.
///
/// The changeset is split into one commit per `(type, scope)` so distinct
/// features, distinct fixes, and the remaining changes each land in their
/// own commit instead of being mashed into one. Each commit reads like
/// `feat(auth): add login` rather than a flat file listing.
///
/// Classification rules (in priority order):
/// - every changed file is a test   → `test`
/// - every changed file is docs     → `docs`
/// - every changed file is build/cfg→ `build`
/// - new functionality              → `feat`: a new source file, *or* code
///   added to existing files (pure additions, no removals)
/// - only modifications/edits       → `fix`, unless it's a large
///   restructure (more lines removed than added across many files) →
///   `refactor`
fn offline_groups(collected: &analysis::CollectedDiff) -> Vec<PlanGroup> {
    let changes = parse_file_changes(collected);
    if changes.is_empty() {
        return vec![PlanGroup {
            message: format!("{}: update working changes", ChangeType::Chore),
            whole: collected.files.clone(),
            partial: Vec::new(),
        }];
    }

    // One commit per (type, scope): features in different directories,
    // fixes, and the misc remainder become separate commits.
    let mut buckets: BTreeMap<(ChangeType, Option<String>), Vec<FileChange>> = BTreeMap::new();
    for change in changes {
        let change_type = classify_change(std::slice::from_ref(&change));
        let scope = scope_for(&change.path);
        buckets.entry((change_type, scope)).or_default().push(change);
    }

    buckets
        .into_iter()
        .map(|((change_type, scope), bucket_changes)| {
            let files: Vec<String> = bucket_changes.iter().map(|c| c.path.clone()).collect();
            let prefix = match scope {
                Some(scope) => format!("{change_type}({scope})"),
                None => change_type.to_string(),
            };
            PlanGroup {
                message: format!("{prefix}: {}", subject_line(&bucket_changes)),
                whole: files,
                partial: Vec::new(),
            }
        })
        .collect()
}

/// Single-message view of the offline plan (first group), kept for the
/// deterministic fallback where a lone commit is expected.
#[allow(dead_code)]
fn offline_commit_message(collected: &analysis::CollectedDiff) -> String {
    offline_groups(collected)
        .into_iter()
        .next()
        .map(|g| g.message)
        .unwrap_or_else(|| format!("{}: update working changes", ChangeType::Chore))
}

/// Parent directory of a path. Files at the repo root have no parent
/// (`None`).
fn file_scope(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    (parts.len() > 1).then(|| parts[..parts.len() - 1].join("/"))
}

/// Conventional-Commits scope for a file: its parent directory with
/// generic source roots stripped, so scopes read like `auth` or
/// `cli/auth` instead of `crates/cli/src/auth` (or, worse, `src/src`).
fn scope_for(path: &str) -> Option<String> {
    let parent = file_scope(path)?;
    let trimmed = trim_source_root(&parent);
    let trimmed = if trimmed.is_empty() { &parent } else { &trimmed };
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Drop leading boilerplate from a directory: `src/`, `lib/`, `app/`,
/// `include/`, `tests/`, `test/`, and the `crates/<name>/src` prefix.
///
/// A crate's own source root collapses to the crate name — so
/// `crates/cli/src` becomes `cli` (not the uninformative `src`), while
/// `crates/cli/src/auth` becomes `cli/auth`.
fn trim_source_root(dir: &str) -> String {
    if let Some(rest) = dir.strip_prefix("crates/") {
        // rest is "<crate>/src/..." or "<crate>/..."; shed to the crate
        // name plus whatever meaningful directory remains.
        let mut segs = rest.splitn(2, '/');
        let crate_name = segs.next().unwrap_or("");
        if let Some(after) = segs.next() {
            if after == "src" {
                return crate_name.to_string();
            }
            if let Some(inner) = after.strip_prefix("src/") {
                if inner.is_empty() {
                    return crate_name.to_string();
                }
                return format!("{crate_name}/{inner}");
            }
            return format!("{crate_name}/{after}");
        }
        return crate_name.to_string();
    }
    for root in ["src/", "lib/", "app/", "include/", "tests/", "test/"] {
        if let Some(rest) = dir.strip_prefix(root) {
            return rest.to_string();
        }
    }
    dir.to_string()
}

/// One file's contribution to the changeset, reconstructed from the patch.
struct FileChange {
    path: String,
    kind: FileKind,
    added: usize,
    removed: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum FileKind {
    Added,
    Deleted,
    Modified,
}

#[derive(Clone, Copy, PartialEq)]
enum Category {
    Source,
    Test,
    Docs,
    Build,
    Other,
}

/// A Conventional Commits type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ChangeType {
    Feat,
    Fix,
    Refactor,
    Docs,
    Test,
    Build,
    Chore,
}

impl std::fmt::Display for ChangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ChangeType::Feat => "feat",
            ChangeType::Fix => "fix",
            ChangeType::Refactor => "refactor",
            ChangeType::Docs => "docs",
            ChangeType::Test => "test",
            ChangeType::Build => "build",
            ChangeType::Chore => "chore",
        };
        f.write_str(s)
    }
}

/// Walk the patch and reconstruct per-file change metadata: whether the
/// file is new/deleted/modified, and how many lines were added/removed.
fn parse_file_changes(collected: &analysis::CollectedDiff) -> Vec<FileChange> {
    let mut changes = Vec::new();
    let mut current: Option<FileChange> = None;

    let finalize = |current: &mut Option<FileChange>, changes: &mut Vec<FileChange>| {
        if let Some(c) = current.take() {
            changes.push(c);
        }
    };

    for line in collected.patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            finalize(&mut current, &mut changes);
            let path = rest.split(" b/").last().unwrap_or(rest).to_string();
            current = Some(FileChange {
                path,
                kind: FileKind::Modified,
                added: 0,
                removed: 0,
            });
        } else if line.starts_with("new file mode") {
            if let Some(c) = current.as_mut() {
                c.kind = FileKind::Added;
            }
        } else if line.starts_with("deleted file mode") {
            if let Some(c) = current.as_mut() {
                c.kind = FileKind::Deleted;
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            if let Some(c) = current.as_mut() {
                c.added += 1;
            }
        } else if line.starts_with('-') && !line.starts_with("---") {
            if let Some(c) = current.as_mut() {
                c.removed += 1;
            }
        }
    }
    finalize(&mut current, &mut changes);
    changes
}

/// Bucket a path into a coarse category used for type classification.
fn categorize(path: &str) -> Category {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);

    if lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("__tests__")
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.js")
        || name.ends_with("_spec.rs")
        || name.ends_with("_spec.py")
    {
        return Category::Test;
    }

    if name.ends_with(".md")
        || name.ends_with(".rst")
        || lower.contains("/docs/")
        || name.starts_with("readme")
        || name.starts_with("changelog")
        || name.starts_with("license")
    {
        return Category::Docs;
    }

    if matches!(
        name,
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "dockerfile"
            | "makefile"
            | "justfile"
            | "build.rs"
            | "requirements.txt"
            | "pyproject.toml"
            | "setup.py"
            | "setup.cfg"
            | "docker-compose.yml"
            | "composer.json"
            | "gemfile"
            | ".gitignore"
    ) || lower.contains("/.github/")
        || lower.contains("/.gitlab/")
        || lower.ends_with(".tf")
        || lower.ends_with(".toml")
        || lower.ends_with(".yml")
        || lower.ends_with(".yaml")
    {
        return Category::Build;
    }

    const SOURCE_EXTS: &[&str] = &[
        "rs", "py", "js", "ts", "jsx", "tsx", "go", "java", "c", "h", "cpp", "hpp", "cc", "rb",
        "php", "swift", "kt", "scala", "sh", "sql", "html", "css", "scss", "sass", "vue", "elm",
        "ex", "exs", "clj", "lua", "dart",
    ];
    if let Some(ext) = name.rsplit('.').next() {
        if SOURCE_EXTS.contains(&ext) {
            return Category::Source;
        }
    }

    Category::Other
}

/// True for code-bearing files we treat as feature/fix candidates.
fn is_code_like(category: Category) -> bool {
    matches!(category, Category::Source | Category::Other)
}

/// Decide the Conventional Commits type from the whole changeset.
fn classify_change(changes: &[FileChange]) -> ChangeType {
    if !changes.is_empty() && changes.iter().all(|c| categorize(&c.path) == Category::Test) {
        return ChangeType::Test;
    }
    if !changes.is_empty() && changes.iter().all(|c| categorize(&c.path) == Category::Docs) {
        return ChangeType::Docs;
    }
    if !changes.is_empty() && changes.iter().all(|c| categorize(&c.path) == Category::Build) {
        return ChangeType::Build;
    }

    // New functionality: a newly added code file, *or* a feature that
    // extends existing files by adding code (pure additions, no removals).
    // Either way it "falls under feat".
    let adds_new_code = changes.iter().any(|c| {
        is_code_like(categorize(&c.path))
            && matches!(c.kind, FileKind::Added | FileKind::Modified)
            && c.added > 0
            && c.removed == 0
    });
    if adds_new_code {
        return ChangeType::Feat;
    }

    // Otherwise we're editing existing code. A large net deletion across
    // several files looks like a restructure rather than a targeted fix.
    let added: usize = changes.iter().map(|c| c.added).sum();
    let removed: usize = changes.iter().map(|c| c.removed).sum();
    if changes.len() > 1 && removed > added && removed > 0 {
        return ChangeType::Refactor;
    }

    // Edits / deletions of existing code are treated as fixes.
    if removed > 0 || changes.iter().any(|c| c.kind == FileKind::Deleted) {
        return ChangeType::Fix;
    }

    ChangeType::Refactor
}

/// The imperative subject line, e.g. "add auth" or "update parser and 2
/// more". Prefers a newly added code file as the primary subject.
fn subject_line(changes: &[FileChange]) -> String {
    let primary = changes
        .iter()
        .find(|c| c.kind == FileKind::Added && is_code_like(categorize(&c.path)))
        .or_else(|| changes.iter().find(|c| is_code_like(categorize(&c.path))))
        .or_else(|| changes.first());

    let Some(primary) = primary else {
        return "update working changes".to_string();
    };

    let filename = primary.path.rsplit('/').next().unwrap_or(&primary.path);
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| if s.is_empty() { filename } else { s })
        .unwrap_or(filename);

    let verb = match primary.kind {
        FileKind::Added => "add",
        FileKind::Deleted => "remove",
        FileKind::Modified => "update",
    };

    let n = changes.len();
    if n == 1 {
        format!("{verb} {stem}")
    } else {
        format!("{verb} {stem} and {} more", n - 1)
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

/// Repair backend group paths that don't match the analyzed diff
/// verbatim — e.g. the model returned a basename (`analysis.rs`) or a
/// `./`/`b/` prefix instead of the full path (`crates/cli/src/analysis.rs`).
/// Falls back to a basename match against the real changed files so a
/// usable AI split isn't rejected and forced into the offline fallback.
fn normalize_group_paths(groups: &mut [ChangeGroup], actual: &HashSet<String>) {
    let resolve = |path: &str| -> String {
        if actual.contains(path) {
            return path.to_string();
        }
        let cleaned = path
            .trim_start_matches("./")
            .trim_start_matches("b/")
            .trim_start_matches("a/");
        if actual.contains(cleaned) {
            return cleaned.to_string();
        }
        let base = cleaned.rsplit('/').next().unwrap_or(cleaned);
        if let Some(found) = actual.iter().find(|f| f.rsplit('/').next() == Some(base)) {
            return found.clone();
        }
        path.to_string()
    };
    for group in groups.iter_mut() {
        for file in group.files.iter_mut() {
            *file = resolve(file);
        }
        for partial in group.partial_files.iter_mut() {
            partial.path = resolve(&partial.path);
        }
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
    base_branch: Option<&str>,
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

    execute_commits(plan, file_diffs, staged_used, baseline, base_branch)
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
    base_branch: Option<&str>,
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

    execute_commits(plan, file_diffs, staged_used, baseline, base_branch)
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
    base_branch: Option<&str>,
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
    // What we'll record to the session log once every commit lands.
    let mut recorded: Vec<crate::engine::history::SessionCommit> = Vec::new();

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

                // Capture the commit we just made for the session log.
                let sha = git::head_sha()?;
                let mut files: Vec<String> = group.whole.clone();
                for (path, _) in &group.partial {
                    if !files.contains(path) {
                        files.push(path.clone());
                    }
                }
                recorded.push(crate::engine::history::SessionCommit {
                    sha,
                    message: group.message.clone(),
                    files,
                });
            }
            Err(err) => {
                eprintln!("error: {err:#}");
                return report_partial_failure(&committed, &plan[index..]);
            }
        }
    }

    println!();
    println!("Done — created {total} commit(s).");

    // Record the session only after every commit succeeded, so a partial
    // failure never leaves a half-populated entry in the history log.
    if !recorded.is_empty() {
        record_session(base_branch, recorded)?;
    }

    // Offer to push the result upstream; never fatal to the commit itself.
    if let Err(err) = maybe_push() {
        eprintln!("warning: didn't push — {err:#}");
    }

    Ok(ExitCode::SUCCESS)
}

/// After a successful commit, ask whether to push the current branch.
/// Skips the prompt entirely when there is no remote to push to. A push
/// failure is reported but does not fail the (already successful) commit.
fn maybe_push() -> Result<()> {
    if git::upstream().is_none() && !git::remote_exists("origin") {
        return Ok(());
    }

    if !prompt_confirm("Push these commits to the remote? [y/N] ")? {
        return Ok(());
    }

    println!("Pushing…");
    git::push_current_branch()
}

/// Prompt for a yes/no answer; only `y`/`yes` (case-insensitive) returns
/// true, and the default (empty input) is No.
fn prompt_confirm(question: &str) -> Result<bool> {
    print!("{question}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Persist a successful `commitor commit` run to the per-repo session
/// log. All errors here are non-fatal to the commit itself (the user
/// already has their commits) but are surfaced so a broken history dir
/// doesn't fail silently.
fn record_session(
    base_branch: Option<&str>,
    commits: Vec<crate::engine::history::SessionCommit>,
) -> Result<()> {
    use crate::engine::history;

    let first_sha = match commits.first() {
        Some(c) => c.sha.clone(),
        None => return Ok(()),
    };
    let branch = git::current_branch()?;
    let session = history::Session {
        session_id: history::new_session_id(&first_sha),
        timestamp: history::now_iso(),
        branch,
        base_branch: base_branch.map(str::to_string),
        commits,
        pushed: false,
        reverted: false,
        reverted_at: None,
    };
    history::record_session(&session)?;
    Ok(())
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

/// What the user wants to do when the AI analysis or its proposed split
/// fails: try again, skip straight to the offline plan, or bail out.
enum RetryDecision {
    Retry,
    Offline,
    Cancel,
}

/// Offer to retry after a transient failure, fall back to offline, or
/// cancel. Enter (or `r`) retries; EOF on stdin cancels rather than
/// looping forever.
fn prompt_retry(message: &str) -> Result<RetryDecision> {
    loop {
        print!("{message}\n[r]etry · [o]ffline fallback · [c]ancel (r): ");
        io::stdout().flush()?;

        let mut buf = String::new();
        io::stdin()
            .read_line(&mut buf)
            .context("failed to read your input")?;
        if buf.is_empty() {
            return Ok(RetryDecision::Cancel);
        }

        match buf.trim() {
            "" | "r" | "R" => return Ok(RetryDecision::Retry),
            "o" | "O" => return Ok(RetryDecision::Offline),
            "c" | "C" => return Ok(RetryDecision::Cancel),
            _ => println!("Please answer r, o, or c (Enter = retry)."),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn collected(patch: &str, files: &[&str]) -> analysis::CollectedDiff {
        analysis::CollectedDiff {
            staged_used: true,
            files: files.iter().map(|s| s.to_string()).collect(),
            patch: patch.to_string(),
            untracked: Vec::new(),
        }
    }

    #[test]
    fn new_source_file_is_feat() {
        let patch = "diff --git a/src/auth.rs b/src/auth.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/auth.rs\n@@ -0,0 +1,3 @@\n+fn login() {}\n";
        let msg = offline_commit_message(&collected(patch, &["src/auth.rs"]));
        assert!(msg.starts_with("feat("), "got: {msg}");
        assert!(msg.contains("add auth"), "got: {msg}");
    }

    #[test]
    fn modifying_source_is_fix() {
        let patch = "diff --git a/src/parser.rs b/src/parser.rs\n--- a/src/parser.rs\n+++ b/src/parser.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n";
        let msg = offline_commit_message(&collected(patch, &["src/parser.rs"]));
        assert!(msg.starts_with("fix("), "got: {msg}");
    }

    #[test]
    fn feature_added_to_existing_file_is_feat() {
        // Pure additions (no removed lines) extending an existing source
        // file are a new feature, not a refactor.
        let patch = "diff --git a/src/foo.rs b/src/foo.rs\n--- a/src/foo.rs\n+++ b/src/foo.rs\n@@ -1 +1,3 @@\n fn existing() {}\n+fn new_feature() {}\n+fn another() {}\n";
        let msg = offline_commit_message(&collected(patch, &["src/foo.rs"]));
        assert!(msg.starts_with("feat("), "got: {msg}");
    }

    #[test]
    fn only_tests_is_test() {
        let patch = "diff --git a/tests/auth_test.rs b/tests/auth_test.rs\nnew file mode 100644\n--- /dev/null\n+++ b/tests/auth_test.rs\n@@ -0,0 +1 @@\n+test\n";
        let msg = offline_commit_message(&collected(patch, &["tests/auth_test.rs"]));
        assert!(msg.starts_with("test"), "got: {msg}");
    }

    #[test]
    fn only_docs_is_docs() {
        let patch = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-old\n+new\n";
        let msg = offline_commit_message(&collected(patch, &["README.md"]));
        assert!(msg.starts_with("docs"), "got: {msg}");
    }

    #[test]
    fn splits_multi_file_fixes_into_separate_commits() {
        // Two modified files in *different* directories each become their
        // own `fix` commit (different scope); same-directory fixes group.
        let patch = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,5 +1,2 @@\n-a\n-b\n-c\n-d\n+e\n\
                    diff --git a/lib/b.rs b/lib/b.rs\n--- a/lib/b.rs\n+++ b/lib/b.rs\n@@ -1,5 +1,2 @@\n-f\n-g\n-h\n-i\n+j\n";
        let groups = offline_groups(&collected(&patch, &["src/a.rs", "lib/b.rs"]));
        assert_eq!(groups.len(), 2, "got: {groups:?}");
        assert!(groups.iter().all(|g| g.message.starts_with("fix(")));
    }

    #[test]
    fn empty_changeset_is_chore() {
        let msg = offline_commit_message(&collected("", &[]));
        assert_eq!(msg, "chore: update working changes");
    }

    #[test]
    fn scope_strips_source_root() {
        let patch = "diff --git a/src/auth/login.rs b/src/auth/login.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/auth/login.rs\n@@ -0,0 +1 @@\n+fn x() {}\n";
        let msg = offline_commit_message(&collected(patch, &["src/auth/login.rs"]));
        assert!(msg.starts_with("feat(auth):"), "got: {msg}");
    }

    #[test]
    fn crate_layout_scope_is_crate_name_not_src() {
        // A crate's top-level source file should scope to the crate (`cli`),
        // not the uninformative `src`.
        let patch = "diff --git a/crates/cli/src/admin.rs b/crates/cli/src/admin.rs\nnew file mode 100644\n--- /dev/null\n+++ b/crates/cli/src/admin.rs\n@@ -0,0 +1 @@\n+fn grant() {}\n";
        let msg = offline_commit_message(&collected(patch, &["crates/cli/src/admin.rs"]));
        assert!(msg.starts_with("feat(cli):"), "got: {msg}");

        // A file deeper in the crate keeps the crate + subdir as scope.
        let patch2 = "diff --git a/crates/cli/src/auth/login.rs b/crates/cli/src/auth/login.rs\nnew file mode 100644\n--- /dev/null\n+++ b/crates/cli/src/auth/login.rs\n@@ -0,0 +1 @@\n+fn login() {}\n";
        let msg2 = offline_commit_message(&collected(patch2, &["crates/cli/src/auth/login.rs"]));
        assert!(msg2.starts_with("feat(cli/auth):"), "got: {msg2}");
    }

    #[test]
    fn normalize_repairs_abbreviated_model_paths() {
        use crate::analysis::{ChangeGroup, PartialFile};
        let actual: std::collections::HashSet<String> =
            ["crates/cli/src/analysis.rs", "crates/cli/src/auth/login.rs"]
                .iter()
                .map(|s| s.to_string())
                .collect();

        let mut groups = vec![ChangeGroup {
            files: vec![
                "analysis.rs".to_string(),       // basename
                "./crates/cli/src/auth/login.rs".to_string(), // ./ prefix
            ],
            commit_message: "x".into(),
            rationale: String::new(),
            partial_files: vec![PartialFile {
                path: "b/crates/cli/src/analysis.rs".to_string(), // b/ prefix
                hunks: vec![1],
            }],
        }];

        normalize_group_paths(&mut groups, &actual);

        assert_eq!(groups[0].files[0], "crates/cli/src/analysis.rs");
        assert_eq!(groups[0].files[1], "crates/cli/src/auth/login.rs");
        assert_eq!(groups[0].partial_files[0].path, "crates/cli/src/analysis.rs");
    }

    #[test]
    fn splits_features_fixes_and_remainder_into_separate_commits() {
        let patch = "\
diff --git a/src/auth/login.rs b/src/auth/login.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/auth/login.rs\n@@ -0,0 +1 @@\n+fn login() {}\n\
diff --git a/src/parser/grammar.rs b/src/parser/grammar.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/parser/grammar.rs\n@@ -0,0 +1 @@\n+fn parse() {}\n\
diff --git a/src/cache.rs b/src/cache.rs\n--- a/src/cache.rs\n+++ b/src/cache.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n\
diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-old\n+new\n";
        let groups = offline_groups(&collected(
            patch,
            &["src/auth/login.rs", "src/parser/grammar.rs", "src/cache.rs", "README.md"],
        ));
        // Two distinct features (different scopes) -> 2 feat commits,
        // one fix, one docs.
        let types: Vec<&str> = groups.iter().map(|g| g.message.split(':').next().unwrap()).collect();
        assert_eq!(types.len(), 4, "groups: {groups:?}");
        assert!(types.iter().filter(|t| t.starts_with("feat")).count() == 2);
        assert!(types.iter().any(|t| t.starts_with("fix")));
        assert!(types.iter().any(|t| t.starts_with("docs")));
    }

    #[test]
    fn same_scope_features_merge_into_one_commit() {
        let patch = "\
diff --git a/src/auth/login.rs b/src/auth/login.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/auth/login.rs\n@@ -0,0 +1 @@\n+fn login() {}\n\
diff --git a/src/auth/token.rs b/src/auth/token.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/auth/token.rs\n@@ -0,0 +1 @@\n+fn token() {}\n";
        let groups = offline_groups(&collected(
            patch,
            &["src/auth/login.rs", "src/auth/token.rs"],
        ));
        assert_eq!(groups.len(), 1, "expected one merged feat commit: {groups:?}");
        assert!(groups[0].message.starts_with("feat(auth):"));
    }
}

