# review-agent

AI-powered PR review agent — a single-binary Rust CLI that reviews GitHub Pull Requests using any OpenAI-compatible AI endpoint. Deployable as a Docker-based GitHub Action.

**License:** MIT | **MSRV:** 1.85 | **Base image:** `gcr.io/distroless/static` (~20 MB)

---

## Quickstart

### As a GitHub Action

```yaml
# .github/workflows/review-agent.yml
name: Review Agent
on:
  pull_request:
    types: [opened, synchronize, reopened, ready_for_review]
jobs:
  review:
    # Skips draft PRs and bot senders (prevents feedback loops)
    if: ${{ github.event.sender.type != 'Bot' && !github.event.pull_request.draft }}
    runs-on: ubuntu-latest
    permissions:
      contents: read
      pull-requests: write
    steps:
      - name: Run review-agent
        uses: devstroop/review-agent@v0.1.0
        with:
          pr_url: ${{ format('https://github.com/{0}/{1}/pull/{2}', github.repository_owner, github.event.repository.name, github.event.pull_request.number) }}
          ai_api_key: ${{ secrets.AI_API_KEY }}
        env:
          AI_API_BASE: ${{ secrets.AI_API_BASE || 'https://ai.cloudmagic.io/v1' }}
          MODEL: ${{ secrets.MODEL || 'glm-4.6' }}
```

The action only fires on `opened`, `synchronize`, and `reopened` events. Draft PRs and bot senders are skipped by default to save costs and prevent feedback loops. The `pr_url` is automatically constructed from the event context — just pass it through. `GITHUB_TOKEN` defaults to the auto-generated token; no need to pass it explicitly.

The action only fires on `opened`, `synchronize`, and `reopened` events. Draft PRs and bot senders are skipped by default to save costs and prevent feedback loops.

### Local CLI

```bash
# Set required env vars
export GITHUB_TOKEN=ghp_...
export AI_API_KEY=sk-...

# Review a PR
cargo run -- review --pr-url https://github.com/owner/repo/pull/1
```

You can also create a `review-agent.toml` config file instead of env vars — see [Configuration](#configuration).

---

## Configuration

Config is loaded from the first file found in this order:
1. `$GITHUB_WORKSPACE/.github/review-agent.toml` (GitHub Action)
2. `$CWD/review-agent.toml`
3. `$CWD/.review-agent.toml`
4. `~/.config/review-agent/config.toml`
5. Built-in defaults

Environment variables override file values.

### Reference

```toml
[ai]
api_base = "https://ai.cloudmagic.io/v1"   # OpenAI-compatible endpoint
model = "glm-4.6"                           # Model name
# api_key — set via AI_API_KEY env var
request_timeout_secs = 120                   # AI request timeout
temperature = 0.2                            # Model temperature (0.0–1.0)
max_completion_tokens = 4096                 # Max tokens in AI response

[github]
# token — set via GITHUB_TOKEN env var
request_timeout_secs = 30                    # GitHub API timeout
max_concurrent_requests = 10                 # Max concurrent API requests

[review]
max_input_tokens = 16000                     # Max tokens for diff input
max_diff_files = 50                          # Max files to review
extra_instructions = ""                      # Extra prompt instructions
```

### Required Env Vars

| Variable | Description |
|---|---|
| `GITHUB_TOKEN` | GitHub token (`contents:read` + `pull-requests:write`) |
| `AI_API_KEY` | API key for the AI endpoint |

### Optional Env Vars

| Variable | Overrides config field |
|---|---|
| `AI_API_BASE` | `ai.api_base` |
| `MODEL` | `ai.model` |
| `LOG_FORMAT` | Set to `"json"` for structured JSON logging |

---

## GitHub Token Guide

review-agent uses the GitHub token for two things:
1. **Reading PR diffs and metadata** — requires `contents: read`
2. **Posting reviews and comments** — requires `pull-requests: write`

### Default GITHUB_TOKEN (recommended for Actions)

The auto-generated `secrets.GITHUB_TOKEN` is sufficient. Set permissions in your workflow:

```yaml
permissions:
  contents: read
  pull-requests: write
```

### Personal Access Token (for CLI use)

For CLI usage, create a [fine-grained PAT](https://github.com/settings/tokens?type=beta) with:
- Repository access: the repos you want to review
- Permissions: `contents: read`, `pull-requests: write`

**Do not use a token with the `repo` scope** — it grants unnecessary broad access.

---

## Architecture

The `review` command runs through a strict processing pipeline, each phase independently testable and replaceable:

```
┌──────────────────────┐
│  1. Event Filter     │  main.rs — only opened/synchronize/reopened; skip bots & drafts
├──────────────────────┤
│  2. Config Load      │  config.rs — TOML + env overlay (Sensitive<T> for secrets)
├──────────────────────┤
│  3. Diff Fetch       │  github/mod.rs — reqwest, rate-limited (Semaphore + governor), paginated
├──────────────────────┤
│  4. Diff Parse       │  diff.rs — similar crate, file filtering, binary-file skip
├──────────────────────┤
│  5. Token Budget     │  tokens.rs — 3.5 chars/token heuristic, budget trimming
├──────────────────────┤
│  6. AI Review        │  ai/mod.rs — OpenAI-compatible, 90s timeout, backoff retry (3×)
├──────────────────────┤
│  7. Post Comment     │  github/mod.rs — post markdown review to PR
└──────────────────────┘
```

### Skip Logic

PRs are skipped (with a comment) when:
- The event isn't `opened`, `synchronize`, or `reopened`
- The sender is a bot (`github.event.sender.type == 'Bot'`)
- The PR is a draft (`draft: true`) — override with config opt-in
- All changed files match the skip-list: `Cargo.lock`, `package-lock.json`, `yarn.lock`, `*.min.js`, `*.min.css`, `*.pb.go`, `*.pb.rs`, `CHANGELOG.md`, `vendor/`, `node_modules/`
- The file is binary (GitHub returns no `patch` data)
- The diff exceeds `max_diff_files` (default 50)
- The token budget exceeds `max_input_tokens` (default 16,000, hard cap 32,000)

### Observability

Every review run appends a summary to the GitHub Actions step summary (`$GITHUB_STEP_SUMMARY`) with:

| Metric | Description |
|---|---|
| PR size | Files changed + total lines in diff |
| Tokens estimated | Input tokens via 3.5 chars/token heuristic |
| Tokens used | Actual completion tokens from AI response |
| AI latency | Time from request to response |
| Model used | Model name from config |

This gives users and maintainers immediate visibility into cost and performance without digging through raw logs.

---

## Security

See `SECURITY.md` for full details. Key points:
- **Secrets never logged**: `Sensitive<T>` wrapper redacts all keys/tokens in Display/Debug
- **Prompt injection**: AI output capped at 4096 tokens; action-like markdown blocks stripped
- **`GITHUB_TOKEN` never in AI context**: only the PR diff and metadata are sent to the AI
- **Token scope**: minimum `contents: read` + `pull-requests: write` — never use `repo` scope
- **Static linking**: Binary is statically linked with `+crt-static` and runs on `gcr.io/distroless/static` — no dynamic linker attack surface

---

## v2 Roadmap

- Inline code suggestions (line-anchored comments on specific diff hunks)
- Incremental reviews (only new commits since last review)
- Idempotent comment updates (edit existing review comments instead of posting new ones)
- Structured YAML output with "Apply suggestion" buttons
- Multi-provider (GitLab, Bitbucket, Azure DevOps)
- Webhook server mode for real-time processing
- `tiktoken-rs` for accurate token counting (it's pure Rust now)

---

## Development

```bash
cargo build
cargo test
cargo clippy
cargo run -- review --pr-url https://github.com/owner/repo/pull/1
```

See `CONTRIBUTING.md` for detailed development guidelines.

---

## Comparison with PR-Agent

| Feature | review-agent | PR-Agent |
|---|---|---|
| Written in | Rust (single binary) | Python |
| Dependencies | ~15 crates, static-linked | 30+ Python packages |
| Image size | < 50 MB (target) | ~500 MB+ |
| Config | TOML + env vars | Dynaconf (TOML + env + chained loaders) |
| AI providers | Any OpenAI-compatible endpoint | 100+ via LiteLLM |
| Git hosts | GitHub only (v1) | GitHub, GitLab, Bitbucket, Azure, Gitea, Gerrit, etc. |
| Output | Raw markdown | Structured YAML + markdown |
| Tools | `review` only (v1) | `review`, `describe`, `improve`, `ask`, etc. |
| Inline suggestions | v2 | v1 |
| Incremental reviews | v2 | v1 |
