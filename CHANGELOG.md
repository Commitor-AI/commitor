# Changelog

All notable changes to Commitor will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is in 0.x, minor versions may add, change, or remove
functionality freely; 1.0.0 marks a stable command surface.

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

[0.2.0]: https://github.com/Commitor-AI/commitor/releases/tag/v0.2.0
[0.1.0]: https://github.com/Commitor-AI/commitor/releases/tag/v0.1.0
