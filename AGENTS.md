# AGENTS.md — review-agent

Single-binary Rust CLI that reviews GitHub PRs via any OpenAI-compatible endpoint. Runs as a Docker-based GitHub Action on `gcr.io/distroless/static` (~20 MB image).

## Architecture

```
src/main.rs      — clap CLI (review + serve subcommands); event filter (skip bots/drafts)
src/config.rs    — Settings::load() — TOML + env overlay (Sensitive<T> for secrets)
src/error.rs     — AgentError enum (thiserror)
src/sensitive.rs — Sensitive<T> — Display/Debug redacted as "***"
src/github/      — reqwest client for GitHub API (done)
src/diff.rs      — diff parser via diffy crate; binary files auto-skipped (done)
src/tokens.rs    — (len*2)/7 token heuristic; safe margin overestimates tokens (done)
src/language.rs  — extension→language lookup via sorted slice + Path::extension() (done)
src/ai/          — OpenAI-compatible chat client; 90s timeout × 4 attempts ~7 min max (done)
src/tools/       — review orchestrator (in progress — feat/review-tool)
prompts/         — system/user prompt templates
Dockerfile       — multi-stage Docker build (musl static → distroless/static) (done)
action.yml       — GitHub Action metadata (Docker strategy) (done)
```

## Conventions

- **Errors**: `AgentError` enum with `?` propagation. Add variants as needed.
- **Secrets**: `Sensitive<T>` wrapper — Display/Debug show `"***"`. All keys/tokens use it.
- **Config**: `$GITHUB_WORKSPACE/.github/review-agent.toml` → CWD → `~/.config/` → defaults. Env vars override.
- **Event filtering**: Action no-ops unless event is `opened`, `synchronize`, or `reopened`. Draft PRs and bot senders are skipped (ADR-019).
- **Diff source**: Fetched via `Accept: application/vnd.github.v3.diff` header, guaranteeing standard unified diff format (ADR-005).
- **GitHub rate limiting**: Semaphore(10) + `governor` (100 req/min). AI retry via `backoff` (exp+jitter, 3 retries, 429/5xx only).
- **Token budget**: 3.5 chars/token heuristic overestimates tokens, acting as a conservative safety cap (ADR-006).
- **HTTP**: single `reqwest::Client` with rustls-tls. Headers: `User-Agent: review-agent`, `Accept: application/vnd.github.v3.diff`.
- **Logging**: `tracing` — JSON when `LOG_FORMAT=json`. Secrets redacted at type level.
- **Tests**: `wiremock` for HTTP mocking. No network in CI.
- **Docker**: Static link via `x86_64-unknown-linux-musl` target, `gcr.io/distroless/static` base image (ADR-018).
- **Action**: `action.yml` with Docker strategy — auto-detects PR URL from `github.event.pull_request`.
- **Release**: GHCR publish + GitHub Release on `v*` tags, SBOM generation.
- **Observability**: Step summary table via `$GITHUB_STEP_SUMMARY` — PR size, tokens, latency, model (ADR-020).
- **MSRV**: 1.85. Edition 2024.
- **Style**: `cargo fmt`, `cargo clippy` clean, `default-features=false` on deps.

## Key Docs

| File | What it's for |
|---|---|
| `DECISIONS.md` | All architecture decisions (why Rust, why no YAML, why Sensitive<T>, static linking, event filtering, step summary, etc.) |
| `SECURITY.md` | Threat model: prompt injection, secret leakage, token scopes |

## Current State

| Phase | What | Status |
|---|---|---|
| 1–4 | Scaffold, config, error, sensitive, logging, CLI, CI, GitHub client, diff parser, token manager, language detection, AI client | ✅ Done |
| 5 | Review tool orchestrator (`src/tools/review.rs`) | 🏗️ `feat/review-tool` worktree |
| 6 | CLI wiring (parse_pr_url, ReviewTool integration) | 🏗️ `feat/review-tool` worktree |
| **7** | **Docker & Action (Dockerfile, action.yml, .dockerignore, release workflow)** | ✅ **Done** (this branch) |
| 8 | CI polish (badges, release automation) | ⬜ Next
