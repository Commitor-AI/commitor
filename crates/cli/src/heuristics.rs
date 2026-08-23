//! Cheap, local-only heuristics for judging whether a changeset is
//! one logical change.
//!
//! Pure functions on the list of changed paths — no git access, no
//! network — so `commit` can reuse them later.

/// What the heuristics concluded about a changeset.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Confidently one logical change; no backend call needed.
    Clean { summary: String },
    /// Could not confidently call it one change; escalate (unless
    /// running offline).
    Inconclusive { reason: String },
}

const FRONTEND_EXTS: &[&str] = &[
    "css", "scss", "sass", "less", "html", "htm", "js", "jsx", "ts", "tsx",
    "vue", "svelte",
];

const BACKEND_EXTS: &[&str] = &[
    "rs", "py", "go", "java", "kt", "rb", "php", "c", "cc", "cpp", "h",
    "hpp", "cs", "swift",
];

const DOCS_EXTS: &[&str] = &["md", "mdx", "rst", "adoc"];

const DOCS_DIRS: &[&str] = &["docs", "doc", "documentation", "wiki"];

pub fn evaluate(paths: &[String]) -> Verdict {
    if paths.is_empty() {
        return Verdict::Clean {
            summary: "no files changed".to_string(),
        };
    }

    let mut top_dirs: Vec<String> = paths.iter().filter_map(|p| top_segment(p)).map(str::to_string).collect();
    top_dirs.sort();
    top_dirs.dedup();

    let mut exts: Vec<String> = paths.iter().filter_map(|p| extension(p)).map(|e| e.to_ascii_lowercase()).collect();
    exts.sort();
    exts.dedup();

    let has_docs_dir = top_dirs.iter().any(|dir| DOCS_DIRS.contains(&dir.as_str()));
    let frontend = count_in(&exts, FRONTEND_EXTS);
    let backend = count_in(&exts, BACKEND_EXTS);
    let docs_files = count_in(&exts, DOCS_EXTS);
    let code_files = frontend + backend;

    // Documentation-only change.
    if code_files == 0 && docs_files > 0 {
        return Verdict::Clean {
            summary: "documentation-only change".to_string(),
        };
    }

    // Frontend and backend code in one changeset almost always means
    // at least two concerns.
    if frontend > 0 && backend > 0 {
        return Verdict::Inconclusive {
            reason: format!(
                "mixes frontend ({}) with backend ({}) files",
                joined_from(&exts, FRONTEND_EXTS),
                joined_from(&exts, BACKEND_EXTS),
            ),
        };
    }

    // Docs next to code.
    if docs_files > 0 && code_files > 0 {
        if !has_docs_dir && top_dirs.len() == 1 {
            // A module updating its own README alongside its code is
            // usually one concern.
            return Verdict::Clean {
                summary: format!("code and its docs under {}", top_dirs[0]),
            };
        }
        return Verdict::Inconclusive {
            reason: "touches both documentation and code".to_string(),
        };
    }

    match top_dirs.len() {
        0 | 1 => Verdict::Clean {
            summary: format!(
                "all changes under {}",
                top_dirs.first().map(String::as_str).unwrap_or("the repo root")
            ),
        },
        _ => Verdict::Inconclusive {
            reason: format!(
                "spreads across {} areas: {}",
                top_dirs.len(),
                top_dirs.join(", ")
            ),
        },
    }
}

/// First path segment: `Some("src")` for `src/auth/mod.rs`, `None`
/// for repo-root files like `README.md`.
fn top_segment(path: &str) -> Option<&str> {
    match path.split_once('/') {
        Some((first, _)) if !first.is_empty() => Some(first),
        _ => None,
    }
}

/// File extension, ignoring dotfiles: `.gitignore` has none,
/// `main.rs` has `rs`.
fn extension(path: &str) -> Option<&str> {
    let leaf = path.rsplit('/').next().unwrap_or(path);
    let (name, ext) = leaf.rsplit_once('.')?;
    if name.is_empty() || ext.is_empty() {
        return None;
    }
    Some(ext)
}

fn count_in(exts: &[String], set: &[&str]) -> usize {
    exts.iter().filter(|ext| set.contains(&ext.as_str())).count()
}

fn joined_from(exts: &[String], set: &[&str]) -> String {
    let found: Vec<&str> = exts
        .iter()
        .filter(|ext| set.contains(&ext.as_str()))
        .map(String::as_str)
        .collect();
    found.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn empty_changeset_is_clean() {
        assert!(matches!(
            evaluate(&[]),
            Verdict::Clean { .. }
        ));
    }

    #[test]
    fn single_module_is_clean() {
        let verdict = evaluate(&paths(&["src/auth/login.rs", "src/auth/session.rs"]));
        assert_eq!(
            verdict,
            Verdict::Clean {
                summary: "all changes under src".to_string()
            }
        );
    }

    #[test]
    fn root_level_files_are_clean() {
        let verdict = evaluate(&paths(&["Cargo.toml", "README.md"]));
        assert!(matches!(verdict, Verdict::Clean { .. }));
    }

    #[test]
    fn docs_only_change_is_clean() {
        let verdict = evaluate(&paths(&["docs/setup.md", "README.md"]));
        assert_eq!(
            verdict,
            Verdict::Clean {
                summary: "documentation-only change".to_string()
            }
        );
    }

    #[test]
    fn frontend_plus_backend_is_flagged() {
        let verdict = evaluate(&paths(&["web/app.css", "src/auth/login.rs", "server/api.py"]));
        assert!(matches!(verdict, Verdict::Inconclusive { .. }));
    }

    #[test]
    fn css_with_rust_is_flagged_even_without_js() {
        let verdict = evaluate(&paths(&["assets/main.css", "src/main.rs"]));
        assert!(
            matches!(&verdict, Verdict::Inconclusive { reason }
                if reason.contains("frontend") && reason.contains("backend")
            ),
            "unexpected: {verdict:?}"
        );
    }

    #[test]
    fn docs_dir_plus_code_dir_is_flagged() {
        let verdict = evaluate(&paths(&["docs/guide.md", "src/auth/login.rs"]));
        assert!(matches!(verdict, Verdict::Inconclusive { .. }));
    }

    #[test]
    fn readme_alongside_own_module_code_is_clean() {
        let verdict = evaluate(&paths(&["engine/README.md", "engine/src/lib.rs"]));
        assert!(matches!(verdict, Verdict::Clean { .. }));
    }

    #[test]
    fn several_code_dirs_are_flagged() {
        let verdict = evaluate(&paths(&["src/auth/login.rs", "src/billing/invoice.rs", "migrations/0001.sql"]));
        assert!(matches!(&verdict, Verdict::Inconclusive { reason } if reason.contains("areas")));
    }

    #[test]
    fn dotfiles_have_no_extension() {
        // Dotfile-only changesets must not panic or misclassify.
        let verdict = evaluate(&paths(&[".gitignore", ".github/workflows/ci.yml"]));
        assert_eq!(
            verdict,
            Verdict::Clean {
                summary: "all changes under .github".to_string()
            }
        );
    }
}
