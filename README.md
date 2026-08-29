# Commitor

Commitor is a command-line companion for git that catches unrelated changes
bundled into a single commit and helps you split them cleanly — so `git log`
stays readable and every commit tells one story.

> **Status: early preview (0.x).** The core `scan` and `commit` commands are
> live as of 0.2.0. The project stays on 0.x until the command surface
> stabilizes.

## How it works

1. **Stage (or modify) your changes** as usual.
2. Run `commitor scan` for a read-only opinion: is this one logical change,
   or several unrelated ones bundled together?
3. Run `commitor commit` to act on it. Commitor analyzes the diff — including
   untracked files — proposes either a single message or a split plan, and
   creates the commits **only after you approve them**, with editable messages.

Splitting is smart about granularity: whole files are grouped by topic, and
when a single file itself contains two unrelated changes, it is split at the
hunk level. Before anything runs, the proposed plan must account for every
changed line exactly once — a plan that loses or double-assigns changes is
refused rather than committed.

## Current functionality

| Command | Description |
|---|---|
| `commitor login --key <key>` | Validate an API key against the backend and store it locally |
| `commitor logout` | Delete the stored credentials |
| `commitor whoami` | Show the account and plan behind the stored key |
| `commitor scan` | Analyze the working diff for unrelated changes (read-only) |
| `commitor commit` | Split the diff into AI-planned git commits you approve |
| `commitor commit -b` | As above, but first pick (or create) the branch to commit to |
| `commitor update` | Check GitHub for a newer release and install it |
| `commitor version` | Print the installed version (`--version`, `-V` also work) |
| `commitor admin` | Show whether the local admin role is enabled |
| `commitor gimme admin` | Grant the local admin role (unlocks all pro features) |
| `commitor help` | Show usage help |

### Admin and pro features

Admin is a **backend-verified** privilege. The source of truth is the
backend: `GET /auth/me` reports whether your account is an admin, and the
local `~/.commitor/admin.toml` only ever records a verified result — it
can't be self-granted. To activate it:

```sh
commitor login                 # must already be done
commitor gimme admin           # backend verifies your account as admin
```

If the backend does not report your account as an admin, `gimme admin`
refuses with an error. While the (verified) role is active, `commitor
whoami` reports `admin` as the effective plan and every pro feature is
unlocked on that machine, regardless of the account's plan. Revoke it any
time:

```sh
commitor admin revoke
```

`commitor whoami` re-checks the backend, so a cached grant that the
backend has since revoked is flagged instead of silently trusted.

### scan flags

| Flag | Effect |
|---|---|
| `--all` | Scan unstaged changes instead of staged ones |
| `--offline` | Only run local heuristics; never call the backend |
| `--strict` | Exit non-zero when the commit looks mixed (CI / pre-commit hooks) |
| `--json` | Print machine-readable JSON instead of a formatted report |

### commit flags

| Flag | Effect |
|---|---|
| `--all` | Plan commits from unstaged changes instead of staged ones |
| `-b` | Interactively choose the branch to commit to before committing |

#### Choosing a branch with `-b`

Pass `-b` to `commit` and commitor lists every local branch (marking the
current one) plus a **“create a new branch”** option, then switches to your
choice before anything is committed:

```
$ commitor commit -b
Choose a branch to commit to:
  1: master (current)
  2: feature/login-flow
  3: create a new branch

Enter a number [1-3] (or 'n' for a new branch): n
Tab to complete, or type your own; Ctrl-C to skip
New branch name: update-auth-py-update-api-py
Switched to new branch 'update-auth-py-update-api-py'.
```

When creating a branch, commitor suggests a name derived from the diff —
press **Tab** to accept it, type your own name, or **Ctrl-C** to skip the
picker and commit on the current branch.

`scan` and `commit` need a backend account (`commitor login --key <key>` —
get a key at [commitor.dev/dashboard](https://commitor.dev/dashboard)).
`scan` only calls the backend when local heuristics can't confidently call
the changeset one logical change; `commit` always does, because every commit
message comes from the analysis.

When a newer release is available, server-facing commands print a
non-fatal `note:` reminding you to run `commitor update` — but they still
run on older builds, so you're never forced to upgrade. Set
`COMMITOR_ALLOW_OUTDATED=1` to suppress that reminder.

## Install

### With cargo

The crates.io package is `commitor-cli` (the name `commitor` was already
taken); it installs a binary called `commitor`:

```sh
cargo install commitor-cli
```

Requires a Rust toolchain (1.74+ recommended).

### Prebuilt binaries

No Rust toolchain needed — grab a binary from the
[releases page](https://github.com/Commitor-AI/commitor/releases) for your
platform:

| File | Platform |
|---|---|
| `commitor-aarch64-apple-darwin` | macOS (Apple Silicon) |
| `commitor-x86_64-apple-darwin` | macOS (Intel) |
| `commitor-x86_64-unknown-linux-gnu` | Linux (x86_64, glibc) |
| `commitor-x86_64-pc-windows-msvc.exe` | Windows (x86_64) |

Download it, make it executable, and put it on your `PATH`:

```sh
chmod +x commitor-*          # not needed for the .exe
sudo mv commitor-* /usr/local/bin/commitor
```

## Updating

```sh
commitor update
```

Commitor checks GitHub for a new release at most once a day and prompts
before replacing its own binary.

## Roadmap

- Hosted backend generally available (today the backend location defaults to
  a local development server; override with `COMMITOR_API_URL`)
- Hunk-level splitting for exotic paths (quoted/escaped filenames are
  currently refused rather than split)
- PR-review command (`commitor pr`) comparing a branch against its base

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-Apache),
at your option.
