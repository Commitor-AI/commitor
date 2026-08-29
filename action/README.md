# commitor-action

A GitHub Action that runs [Commitor](https://github.com/Commitor-AI/commitor)
against a pull request's diff and posts the result as a PR comment —
updating the previous comment instead of posting a new one on every push.

Commitor catches unrelated changes bundled into a single commit/PR and
tells you how to split them. This action points the existing `scan`
engine at the PR range (`origin/<base>...HEAD`) and renders the verdict
as compact GitHub-flavored Markdown.

## Quick start

Add this to `.github/workflows/commitor.yml`:

```yaml
on: pull_request

jobs:
  commitor:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          # Full history is required: a shallow clone can't diff against
          # the base branch, so `origin/<base>...HEAD` would resolve to
          # nothing (or the wrong thing). fetch-depth: 0 prevents that.
          fetch-depth: 0

      - uses: Commitor-AI/commitor-action@v1
        with:
          github-token: ${{ secrets.GITHUB_TOKEN }}
          commitor-api-key: ${{ secrets.COMMITOR_API_KEY }}
```

> If you keep this action as a subdirectory of the Commitor repo, point
> `uses` at it directly instead: `uses: ./action` (or
> `uses: Commitor-AI/commitor/action@v1`). The published form above
> assumes the action lives in its own repo named `commitor-action`.

## Setup

1. **Add the API key secret.** Get a key from your Commitor dashboard,
   then add it as a repository secret named `COMMITOR_API_KEY`
   (`Settings → Secrets and variables → Actions`). The action passes it
   to `commitor login --key`.

2. **Don't forget `fetch-depth: 0`.** The action diffs `HEAD` against
   `origin/<base_ref>` using a symmetric range. With the default
   shallow checkout (`fetch-depth: 1`), the base branch's history isn't
   present, so the diff range comes back empty and Commitor reports
   "No changes to analyze." `fetch-depth: 0` (or an explicit
   `git fetch` of the base branch) avoids that.

## Inputs

| Input               | Required | Default           | Description |
| ------------------- | -------- | ----------------- | ----------- |
| `github-token`      | no       | `${{ github.token }}` | Token used to read/update PR comments. |
| `commitor-api-key`  | **yes**  | —                 | Commitor API key (from the dashboard), as a repo secret. |
| `strict`            | no       | `false`           | If `true`, fail the check when Commitor reports a mixed PR. |
| `api-url`           | no       | *(production)*    | Override for the Commitor backend URL. |
| `version`           | no       | `v0.4.0`          | **Pinned** release tag for the binary. Review bumps deliberately. |

The action forwards the PR's **title and description** to `commitor scan
--context`, so the model can weigh the stated intent of the PR against the
files it actually touches (and never trusts a "clean" local heuristic at
PR scale — `--diff-range` scans always escalate to the backend).

### Strict mode

When `strict: true`, the action fails (non-zero exit) if Commitor finds a
mixed PR, so it shows up as a failing status check — useful as a CI gate.
Note `commitor scan --strict` only fails on a *mixed* verdict; a clean or
inconclusive result still passes.

## Output

The action posts a Markdown comment containing a hidden
`<!-- commitor-analysis -->` marker. On subsequent runs it finds that
marker and **updates** the same comment rather than adding a new one, so
a busy PR doesn't accumulate a wall of Commitor comments.

## Scope (and what's deliberately *not* here — v2)

<!-- TODO(v2): intentionally out of scope for the initial action. -->
This action is intentionally minimal: it shells out to the existing
`commitor scan` CLI and posts the result via the REST API. The following
are **deliberate v2 scope decisions**, not oversights:

- **No GitHub App.** It uses the built-in `GITHUB_TOKEN` plus a user
  supplied API key, not a first-party App with its own identity/permissions.
- **No webhook listener.** It runs on the `pull_request` event, not a
  standalone webhook server.
- **No Marketplace submission.** It's published as a composite action in
  this repo, not submitted to the GitHub Marketplace.

Promoting any of these to v2 should be a conscious, separately-reviewed
change.
