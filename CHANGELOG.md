# Changelog

All notable changes to Commitor will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is in 0.x, minor versions may add, change, or remove
functionality freely; 1.0.0 marks a stable command surface.

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

[0.1.0]: https://github.com/Commitor-AI/commitor/releases/tag/v0.1.0
