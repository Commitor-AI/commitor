//! Unified-diff parsing, plan validation, and selective-patch building
//! for hunk-level commit splitting.
//!
//! Pure functions: diff text in, structured data out. No git calls, no
//! I/O — `commit` owns the git interaction (`git apply --cached`),
//! this module only makes it safe by guaranteeing a plan accounts for
//! every changed line exactly once before anything executes.
//!
//! KNOWN LIMITATION: quoted/escaped git paths (files containing
//! spaces, quotes, or non-ASCII characters that git renders wrapped
//! in double quotes with backslash escapes) are not unquoted here.
//! Such paths will not match between the parsed diff and the backend's
//! response, and the plan will be REFUSED rather than silently
//! mangled. Fixing this properly needs a real git path-unquoting
//! routine; out of scope for now.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};

/// One `@@ -a,b +c,d @@` block plus its content lines (stored without
/// trailing newlines).
#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub body_lines: Vec<String>,
}

/// Everything the splitter needs to know about one changed file.
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Repo-relative path (the `b/` side, except deletions, which use
    /// the `a/` side).
    pub path: String,
    /// Lines from `diff --git …` up to (not including) the first hunk
    /// header: index/mode/---/+++/rename/binary markers.
    pub header: Vec<String>,
    /// 0-based here; referenced externally as 1-based indices.
    pub hunks: Vec<Hunk>,
    /// True when the file must be committed whole: binaries, new and
    /// deleted files, renames/copies, and mode-change-only diffs.
    pub atomic: bool,
}

/// One commit's share of the diff: whole files and/or hunk ranges of
/// partially-split files.
#[derive(Debug, Clone)]
pub struct PlanGroup {
    pub message: String,
    pub whole: Vec<String>,
    /// `(path, 1-based hunk indices)` pairs, in claim order.
    pub partial: Vec<(String, Vec<usize>)>,
}

// ── parsing ─────────────────────────────────────────────────────────

/// Parse a git unified diff into per-file records.
///
/// Lines before the first `diff --git` are ignored. A file record ends
/// at the next `diff --git` or EOF; hunk bodies run from their `@@ `
/// line to the next `@@ `, the next file, or EOF. `\ No newline at end
/// of file` markers stay inside the hunk body they follow.
pub fn parse(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut in_hunk = false;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if let Some(finished) = current.take() {
                files.push(finalize(finished));
            }
            in_hunk = false;
            current = Some(FileDiff {
                path: String::new(),
                header: vec![line.to_string()],
                hunks: Vec::new(),
                atomic: false,
            });
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue; // preamble before any file section
        };

        if line.starts_with("@@ ") {
            file.hunks.push(Hunk {
                header: line.to_string(),
                body_lines: Vec::new(),
            });
            in_hunk = true;
        } else if in_hunk {
            if let Some(hunk) = file.hunks.last_mut() {
                hunk.body_lines.push(line.to_string());
            }
        } else {
            file.header.push(line.to_string());
        }
    }
    if let Some(finished) = current.take() {
        files.push(finalize(finished));
    }
    files
}

/// Fill in derived fields once a file's lines are all collected.
fn finalize(mut file: FileDiff) -> FileDiff {
    file.path = extract_path(&file.header);
    file.atomic = file.hunks.is_empty()
        || file.header.iter().any(|line| {
            line.contains("GIT binary patch")
                || line.contains("Binary files")
                || line.starts_with("new file mode")
                || line.starts_with("deleted file mode")
                || line.starts_with("rename ")
                || line.starts_with("copy ")
        });
    file
}

/// Repo-relative path of a file section.
///
/// Prefers `+++ b/<path>`; on deletions (`+++ /dev/null`) falls back
/// to `--- a/<path>`; mode-only diffs have neither, so the path is
/// taken from the `diff --git a/X b/Y` line instead. Quoted paths are
/// NOT unquoted — see the module-level limitation note.
fn extract_path(header: &[String]) -> String {
    let mut plus: Option<&str> = None;
    let mut minus: Option<&str> = None;

    for line in header {
        if let Some(rest) = line.strip_prefix("+++ ") {
            plus = Some(rest);
        } else if let Some(rest) = line.strip_prefix("--- ") {
            minus = Some(rest);
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            // Fallback seed so even mode-only diffs have something.
            minus = minus.or(Some(rest));
        }
    }

    for candidate in [plus, minus].into_iter().flatten() {
        let token = candidate.split_whitespace().next().unwrap_or(candidate);
        if token == "/dev/null" {
            continue;
        }
        return strip_a_or_b_prefix(token).to_string();
    }

    // Neither --- nor +++ existed: derive from the diff --git line by
    // taking everything after the last " b/" separator.
    let git_line = header.first().map(String::as_str).unwrap_or_default();
    match git_line.rsplit_once(" b/") {
        Some((_, b_side)) => strip_a_or_b_prefix(b_side.trim()).to_string(),
        None => String::new(),
    }
}

fn strip_a_or_b_prefix(token: &str) -> &str {
    token.strip_prefix("b/").or_else(|| token.strip_prefix("a/")).unwrap_or(token)
}

// ── validation ──────────────────────────────────────────────────────

/// Check that `groups` partition the ENTIRE diff exactly once.
///
/// Every rule failure is a hard error — the caller refuses the plan
/// and asks the user to re-run rather than committing anything.
pub fn validate<S: AsRef<str>>(
    files: &[FileDiff],
    groups: &[PlanGroup],
    expected_paths: &[S],
) -> Result<()> {
    let by_path: HashMap<&str, &FileDiff> =
        files.iter().map(|f| (f.path.as_str(), f)).collect();

    let mut whole_claimed: HashSet<&str> = HashSet::new();
    let mut partial_claimed: HashMap<&str, HashSet<usize>> = HashMap::new();

    for (index, group) in groups.iter().enumerate() {
        let label = group_number(index);

        if group.whole.is_empty() && group.partial.is_empty() {
            bail!("group {label} ({}) has no files or hunks assigned", group.message);
        }

        for path in &group.whole {
            let file = lookup(&by_path, path)?;
            if !whole_claimed.insert(path) {
                bail!(
                    "'{path}' is assigned whole to more than one group \
                     (already claimed before group {label})"
                );
            }
            if partial_claimed.contains_key(path.as_str()) {
                bail!(
                    "'{path}' is claimed whole AND partially — ambiguous, refusing"
                );
            }
            let _ = file; // existence checked by lookup
        }

        for (path, hunk_ids) in &group.partial {
            let file = lookup(&by_path, path)?;
            if file.atomic {
                bail!(
                    "'{path}' cannot be split at hunk level (binary/new/deleted/renamed/mode-only) — assign it whole"
                );
            }
            if whole_claimed.contains(path.as_str()) {
                bail!(
                    "'{path}' is claimed whole AND partially — ambiguous, refusing"
                );
            }
            let claimed = partial_claimed.entry(path).or_default();
            for &hunk_id in hunk_ids {
                if hunk_id == 0 || hunk_id > file.hunks.len() {
                    bail!(
                        "'{path}' has {} hunk(s), but group {label} references hunk {hunk_id}",
                        file.hunks.len()
                    );
                }
                if !claimed.insert(hunk_id) {
                    bail!("'{path}' hunk {hunk_id} is assigned more than once");
                }
            }
        }
    }

    // Coverage: every parsed file must be fully accounted for — either
    // claimed whole once, or its partial claims must cover every hunk.
    for file in files {
        let fully_partial = partial_claimed
            .get(file.path.as_str())
            .is_some_and(|claimed| claimed.len() == file.hunks.len());
        if whole_claimed.contains(file.path.as_str()) || fully_partial {
            continue;
        }
        let claimed = partial_claimed.get(file.path.as_str());
        let missing: Vec<usize> = (1..=file.hunks.len())
            .filter(|id| !claimed.is_some_and(|set| set.contains(id)))
            .collect();
        bail!(
            "'{}' is not fully accounted for — unclaimed hunk(s): {}; \
             every changed line must belong to exactly one group",
            file.path,
            join_ids(&missing)
        );
    }

    // No silently-dropped files: everything the collection step saw as
    // changed (tracked + untracked) must appear somewhere in the plan.
    for path in expected_paths {
        let path = path.as_ref();
        let claimed = whole_claimed.contains(path) || partial_claimed.contains_key(path);
        if !claimed {
            bail!("changed file '{path}' was not included in any group");
        }
    }

    Ok(())
}

fn lookup<'a>(by_path: &'a HashMap<&'a str, &'a FileDiff>, path: &str) -> Result<&'a FileDiff> {
    by_path.get(path).copied().ok_or_else(|| {
        anyhow::anyhow!(
            "'{path}' appears in the suggested split but is not part of the analyzed diff"
        )
    })
}

fn group_number(index: usize) -> usize {
    index + 1
}

fn join_ids(ids: &[usize]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

// ── patch building ──────────────────────────────────────────────────

/// Build a standalone unified-diff patch applying only the claimed
/// hunks of `claim.0`, suitable for `git apply --cached`.
///
/// Unknown paths or out-of-range hunk ids yield an empty string —
/// callers validate plans beforehand, so this only guards internal
/// misuse, never user input.
pub fn build_patch(files: &[FileDiff], claim: &(String, Vec<usize>)) -> String {
    let (path, hunk_ids) = claim;
    let Some(file) = files.iter().find(|f| &f.path == path) else {
        return String::new();
    };

    let mut out = String::new();
    for line in &file.header {
        out.push_str(line);
        out.push('\n');
    }
    for &hunk_id in hunk_ids {
        let Some(hunk) = file.hunks.get(hunk_id.wrapping_sub(1)) else {
            continue;
        };
        out.push_str(&hunk.header);
        out.push('\n');
        for line in &hunk.body_lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_FILES: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@ imports
 use one;
+use two;
 use three;
@@ -10,3 +11,4 @@ body
 fn a() {}
+fn b() {}
 }
diff --git a/src/b.rs b/src/b.rs
index 3333333..4444444 100644
--- a/src/b.rs
+++ b/src/b.rs
@@ -1,2 +1,3 @@
 x
+y
 z";

    #[test]
    fn parses_multiple_files_and_hunks() {
        let files = parse(TWO_FILES);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[1].path, "src/b.rs");
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[0].hunks[0].header, "@@ -1,3 +1,4 @@ imports");
        assert!(files[0]
            .hunks[0]
            .body_lines
            .iter()
            .any(|l| l == "+use two;"));
        // The second hunk's body runs until the next diff --git line.
        assert!(files[0].hunks[1].body_lines.iter().any(|l| l == " }"));
        assert!(!files[0].atomic && !files[1].atomic);
    }

    #[test]
    fn no_newline_marker_stays_in_body() {
        let diff = "\
diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -1 +1 @@
-old
\\ No newline at end of file
+new";
        let files = parse(diff);
        let body = &files[0].hunks[0].body_lines;
        assert!(body.iter().any(|l| l.starts_with('\\')));
    }

    #[test]
    fn deleted_file_is_atomic_with_a_side_path() {
        let diff = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 1111111..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye";
        let files = parse(diff);
        assert_eq!(files[0].path, "gone.txt");
        assert!(files[0].atomic);
    }

    #[test]
    fn binary_file_is_atomic_without_hunks() {
        let diff = "\
diff --git a/img.png b/img.png
index 1111111..2222222 100644
Binary files a/img.png and b/img.png differ";
        let files = parse(diff);
        assert_eq!(files[0].path, "img.png");
        assert!(files[0].atomic);
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn rename_with_edits_is_atomic_despite_hunks() {
        let diff = "\
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
--- a/old.rs
+++ b/new.rs
@@ -1 +1 @@
-a
+b";
        let files = parse(diff);
        assert!(files[0].atomic);
        assert_eq!(files[0].path, "new.rs");
    }

    #[test]
    fn mode_only_diff_has_path_from_git_line() {
        let diff = "\
diff --git a/script.sh b/script.sh
old mode 100644
new mode 100755";
        let files = parse(diff);
        assert_eq!(files[0].path, "script.sh");
        assert!(files[0].atomic); // zero hunks
    }

    #[test]
    fn synthetic_new_file_section_parses() {
        let diff = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world";
        let files = parse(diff);
        assert_eq!(files[0].path, "new.txt");
        assert!(files[0].atomic);
        assert_eq!(files[0].hunks.len(), 1);
    }

    fn two_files_fixture() -> Vec<FileDiff> {
        parse(TWO_FILES)
    }

    #[test]
    fn valid_whole_file_plan_passes() {
        let files = two_files_fixture();
        let groups = vec![
            PlanGroup {
                message: "one".into(),
                whole: vec!["src/a.rs".into()],
                partial: vec![],
            },
            PlanGroup {
                message: "two".into(),
                whole: vec!["src/b.rs".into()],
                partial: vec![],
            },
        ];
        assert!(validate(&files, &groups, &["src/a.rs", "src/b.rs"]).is_ok());
    }

    #[test]
    fn valid_hunk_split_of_one_file_passes() {
        let files = two_files_fixture();
        let groups = vec![
            PlanGroup {
                message: "one".into(),
                whole: vec![],
                partial: vec![("src/a.rs".into(), vec![1])],
            },
            PlanGroup {
                message: "two".into(),
                whole: vec!["src/b.rs".into()],
                partial: vec![("src/a.rs".into(), vec![2])],
            },
        ];
        assert!(validate(&files, &groups, &["src/a.rs", "src/b.rs"]).is_ok());
    }

    #[test]
    fn unclaimed_hunk_is_refused() {
        let files = two_files_fixture();
        let groups = vec![
            PlanGroup {
                message: "one".into(),
                whole: vec!["src/b.rs".into()],
                partial: vec![("src/a.rs".into(), vec![1])], // hunk 2 missing
            },
        ];
        let err = validate(&files, &groups, &["src/a.rs", "src/b.rs"]).unwrap_err();
        assert!(err.to_string().contains("unclaimed"), "{err}");
    }

    #[test]
    fn duplicate_hunk_assignment_is_refused() {
        let files = two_files_fixture();
        let groups = vec![
            PlanGroup {
                message: "one".into(),
                whole: vec![],
                partial: vec![("src/a.rs".into(), vec![1])],
            },
            PlanGroup {
                message: "two".into(),
                whole: vec!["src/b.rs".into()],
                partial: vec![("src/a.rs".into(), vec![1, 2])], // hunk 1 twice
            },
        ];
        let err = validate(&files, &groups, &["src/a.rs", "src/b.rs"]).unwrap_err();
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    #[test]
    fn out_of_range_hunk_is_refused() {
        let files = two_files_fixture();
        let groups = vec![PlanGroup {
            message: "one".into(),
            whole: vec!["src/b.rs".into()],
            partial: vec![("src/a.rs".into(), vec![1, 2, 3])], // no hunk 3
        }];
        let err = validate(&files, &groups, &["src/a.rs", "src/b.rs"]).unwrap_err();
        assert!(err.to_string().contains("references hunk 3"), "{err}");
    }

    #[test]
    fn unknown_path_is_refused() {
        let files = two_files_fixture();
        let groups = vec![PlanGroup {
            message: "one".into(),
            whole: vec!["src/a.rs".into(), "phantom.rs".into()],
            partial: vec![],
        }];
        assert!(validate::<&str>(&files, &groups, &[]).is_err());
    }

    #[test]
    fn splitting_an_atomic_file_is_refused() {
        let binary = parse(
            "diff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ",
        );
        let groups = vec![PlanGroup {
            message: "one".into(),
            whole: vec![],
            partial: vec![("img.png".into(), vec![1])],
        }];
        let err = validate(&binary, &groups, &["img.png"]).unwrap_err();
        assert!(err.to_string().contains("cannot be split"), "{err}");
    }

    #[test]
    fn mixed_whole_and_partial_claim_is_refused() {
        let files = two_files_fixture();
        let groups = vec![
            PlanGroup {
                message: "one".into(),
                whole: vec!["src/a.rs".into()],
                partial: vec![],
            },
            PlanGroup {
                message: "two".into(),
                whole: vec![],
                partial: vec![("src/a.rs".into(), vec![2])], // also partially
            },
        ];
        let err = validate(&files, &groups, &["src/a.rs"]).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn omitted_changed_file_is_refused() {
        // Only src/a.rs is part of the parsed diff, but the collection
        // step reported src/b.rs as changed too — the plan must
        // include it somewhere.
        let files = &two_files_fixture()[..1];
        let groups = vec![PlanGroup {
            message: "one".into(),
            whole: vec!["src/a.rs".into()],
            partial: vec![],
        }];
        let err = validate(files, &groups, &["src/a.rs", "src/b.rs"]).unwrap_err();
        assert!(err.to_string().contains("not included in any group"), "{err}");
    }

    #[test]
    fn empty_group_is_refused() {
        let files = two_files_fixture();
        let groups = vec![
            PlanGroup {
                message: "one".into(),
                whole: vec!["src/a.rs".into(), "src/b.rs".into()],
                partial: vec![],
            },
            PlanGroup {
                message: "empty".into(),
                whole: vec![],
                partial: vec![],
            },
        ];
        let err = validate::<&str>(&files, &groups, &[]).unwrap_err();
        assert!(err.to_string().contains("no files or hunks"), "{err}");
    }

    #[test]
    fn build_patch_emits_only_selected_hunks() {
        let files = two_files_fixture();
        let patch = build_patch(&files, &("src/a.rs".into(), vec![2]));
        assert!(patch.starts_with("diff --git a/src/a.rs b/src/a.rs"));
        assert!(!patch.contains("@@ -1,3 +1,4 @@")); // hunk 1 excluded
        assert!(patch.contains("@@ -10,3 +11,4 @@ body"));
        assert!(patch.contains("+fn b() {}"));
        assert!(patch.ends_with('\n'));
        assert!(patch.lines().all(|l| !l.starts_with("+use two;")));
    }
}
