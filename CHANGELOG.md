# Changelog

All notable changes to Commitor will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project is in 0.x, minor versions may add, change, or remove
functionality freely; 1.0.0 marks a stable command surface.

## [0.5.1] — 2026-08-30

### Fixed
- Running `commitor scan` outside a git repository now stops with a clear
  "not a git repository" message instead of git's raw diff usage dump.

## [0.5.0] — 2026-08-29

### Added
- `commitor revert` — undo commits produced by `commitor commit`. Unpushed
  commits are rolled back with a hard reset; already-pushed commits are
  reverted with `git revert`, respecting the safety of shared history.
- `commitor scan --diff-range <RANGE>` — analyze a fixed git range
  (e.g. `origin/main...HEAD`) instead of the working tree, plus `--markdown`
  output suited to a PR comment.
- `commitor scan --context "<title/description>"` — forward a PR's stated
  intent to the model so it can weigh it against the files actually touched.
- Post-commit push in `commitor commit` — after committing, offers to push
  the new branch, auto-detecting the upstream.
- Per-repo session history of `commitor commit` runs, stored at
  `~/.commitor/history/<repo-id>.jsonl`.
- GitHub Action (`./action`) that runs `commitor scan --diff-range` on pull
  requests and posts an updatable Markdown comment.

### Changed
- `commitor commit` gains interactive branch selection, base-branch tracking,
  and an offline mode.

### Improved
- PR-scale analysis: `--diff-range` scans now always consult the backend
  (the local heuristic is advisory only at PR scale), so unrelated changes
  bundled in a large diff are no longer missed.

### Fixed
- Release workflow (`publish.yml`) is now idempotent — re-running a release
  no longer fails on an already-published crates.io / npm version.

## [0.4.0] — 2026-08-29

### Added

- **Backend-verified admin role.** `commitor gimme admin` asks the
  backend to confirm your account (against its admin allowlist) before
  caching the grant locally — it can never be self-granted by editing a
  file. `commitor admin` shows status, `commitor admin revoke` removes
  it, and `whoami` reports the verified state. A verified admin unlocks
  every pro feature (`effective_plan` returns `admin`).
- **Retry prompts on failure.** When the AI analysis is temporarily
  unavailable, or the model returns an inconsistent split, `commitor
  commit` now offers `[r]etry · [o]ffline · [c]ancel` instead of
  silently degrading — up to a few attempts before falling back.

### Improved

- **Conventional Commits are now split at hunk granularity.** A modified
  file that mixes an added feature with an edit is split so the additions
  commit as `feat` and the edits as `fix`, via `partial_files`.
- **Crate-aware scopes.** A crate's source root collapses to the crate
  name — `crates/cli/src/...` scopes to `cli` (and `crates/cli/src/auth`
  to `cli/auth`) instead of the uninformative `src`.
- **Model path-repair.** Abbreviated paths the model returns (a basename,
  or `./`/`b/` prefixes) are matched back to the real changed files, so a
  salvageable AI split is used rather than rejected and forced into the
  coarse offline fallback.
- The offline splitter now mirrors the backend's local tier (same scopes,
  same hunk-level splitting), so `--offline` and `scan` read consistently.

## [0.3.3] — 2026-08-28

### Added

- **One-click login from the terminal.** `commitor login` (with no
  `--key`) opens your browser to sign in, then connects automatically —
  you no longer create or copy an API key. The web app issues a CLI key
  for you and redirects back to a local callback
  (`http://127.0.0.1:18745/callback?key=…`). If that flow is
  interrupted, pasting a key from the dashboard still works as a fallback.

### Improved

- The browser "connected" page after login is redesigned to match the
  Commitor site: a branded, animated success state (no emoji) instead of
  a plain message.

### Changed

- CLI now defaults to the **production live** backend instead of a local
  dev server: `DEFAULT_API_URL` points at `https://commitor-api.vercel.app`.
- Dashboard links in help/error output now point at the live frontend
  (`https://commitor-web.vercel.app/dashboard`) instead of the placeholder
  `commitor.dev`.
- `COMMITOR_API_URL` still overrides the default for pointing at a local or
  staging backend.

## [0.3.2] — 2026-08-27

### Changed

- `commitor commit -b` now asks the AI to read the diff and recommend a
  branch name for "create a new branch", pre-filling the prompt with the
  suggestion. Falls back to the local diff-derived heuristic when offline,
  unauthenticated, or if the backend can't suggest one (backend gains a
  `mode="branch"` that returns `branch_name`).

## [0.3.1] — 2026-08-27

### Changed

- Previous versions are no longer forced to update: `login`, `scan`,
  `commit`, and the other server-facing commands now run on older builds.
  A newer release is only surfaced as a non-fatal `note:` reminder
  (suppressible with `COMMITOR_ALLOW_OUTDATED=1`). `commitor update`
  still upgrades on demand.

## [0.3.0] — 2026-08-27

### Added

- `commitor commit` improvements:
  - interactive branch selection with diff-derived suggestions,
  - `suggest_branch_name` / `slugify` helpers for tidy default branch names,
  - offline commit mode (`--offline`) and offline planning when AI analysis is
    unavailable.
- `commitor scan --json` now prints results only when the flag is set.
- npm wrapper package (`commitor-cli`): install prebuilt binaries via
  `npm install -g commitor-cli` — no Rust toolchain required. npm version tracks
  the crate version 1:1.

### Changed

- Increased the `/analyze` timeout to 180s and added rate-limiting status
  tracking with clearer error handling.

## [0.2.0] — 2026-08-23

The core commands land: Commitor can now analyze your working diff and,
with your approval, turn it into clean commits.

### Added

- `commitor login --key <key>` / `logout` / `whoami` — API-key
  authentication against the Commitor backend.
- `commitor scan` — read-only analysis of the working diff (staged by
  default, `--all` for unstaged). Local heuristics run first; only
  inconclusive changesets are sent to the backend's `/analyze`
  endpoint. Supports `--offline`, `--strict` (non-zero exit when mixed,
  for pre-commit hooks/CI) and `--json`.
- `commitor commit` — turns the analyzed diff into real git commits
  after you approve every message:
  - whole-file splitting into per-topic commits,
  - **hunk-level splitting** when a single file contains unrelated
    changes (`git apply --cached` staging),
  - untracked files are analyzed and planned too (synthesized as
    new-file diffs; the index is never touched during analysis),
  - **coverage guarantee**: before anything runs, the plan must
    account for every changed line exactly once — plans that lose or
    double-assign changes are refused instead of committed,
  - all-or-nothing approval with editable messages; partial failures
    stop immediately and report exactly what landed and what didn't
    (no automatic rollback).
- Server-facing commands now verify the CLI is current before running
  (same once-a-day update gate as before).

### Changed

- Crate description updated to reflect actual functionality.

## [0.1.0] — 2026-08-23

Initial public release — an early preview of the distribution pipeline,
not of the full tool.

### Added

- `commitor update` — check GitHub Releases for a newer version and
  self-update (platform-matched asset download, atomic binary swap,
  interactive confirmation).
- Automatic once-a-day update check; once an update is discovered, all
  other commands are blocked until `commitor update` runs
  (`COMMITOR_ALLOW_OUTDATED=1` bypasses the gate).
- `commitor version` (`--version`, `-V`) and `commitor help`
  (`-h`, `--help`).

### Not yet available

- `commitor scan` and `commitor commit` — in active development,
  coming in future 0.x releases.

[0.5.1]: https://github.com/Commitor-AI/commitor/releases/tag/v0.5.1
[0.5.0]: https://github.com/Commitor-AI/commitor/releases/tag/v0.5.0
[0.4.0]: https://github.com/Commitor-AI/commitor/releases/tag/v0.4.0
[0.3.3]: https://github.com/Commitor-AI/commitor/releases/tag/v0.3.3
[0.3.2]: https://github.com/Commitor-AI/commitor/releases/tag/v0.3.2
[0.3.1]: https://github.com/Commitor-AI/commitor/releases/tag/v0.3.1
[0.3.0]: https://github.com/Commitor-AI/commitor/releases/tag/v0.3.0
[0.2.0]: https://github.com/Commitor-AI/commitor/releases/tag/v0.2.0
[0.1.0]: https://github.com/Commitor-AI/commitor/releases/tag/v0.1.0
