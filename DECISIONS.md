# Architecture Decision Records

This file captures every significant decision made during review-agent's design and implementation. If an AI agent or contributor loses session context, this file should be enough to reconstruct the full rationale.

---

## ADR-001: Rust over Python

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent is Python (requires venv, 30+ packages, ~500 MB Docker image). For a CI tool that runs on every PR, startup time and image size matter.

**Decision:** Build in Rust. Single statically-linked binary, no runtime deps, <50 MB target image size.

**Consequences:**
- + Binary distribution: `docker pull` is fast, no venv/poetry setup
- + Cross-platform: static binary works on any Linux x64 kernel
- + Performance: no Python interpreter overhead for diff parsing or token estimation
- − Rust learning curve for contributors unfamiliar with the language
- − Fewer AI/LLM libraries in the Rust ecosystem vs Python

---

## ADR-002: Scope Limitation — One Tool, One Host, One Provider

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent has 12 git providers, 10+ tools, and 100+ AI providers via LiteLLM. This is appropriate for a mature product but adds enormous complexity for v1.

**Decision:** v1 ships only:
- **One tool:** `review` (no `describe`, `improve`, `ask`, etc.)
- **One host:** GitHub (no GitLab, Bitbucket, Azure DevOps, etc.)
- **One provider:** Any OpenAI-compatible endpoint via configurable `api_base`

**Consequences:**
- + Dramatically simpler codebase — no provider abstraction layer needed in v1
- + Faster to ship — weeks instead of months
- + Easy to extend later — adding providers/tools follows the same patterns
- − Users on other git hosts can't use v1
- − Users who want inline code suggestions (PR-Agent's killer feature) need v2

---

## ADR-003: Raw Markdown Output over Structured YAML

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent uses structured YAML output from the AI, parsed into typed code suggestions. This enables "Apply suggestion" buttons but is brittle — YAML parsing failures block the entire review.

**Decision:** v1 returns raw unstructured markdown from the AI and posts it directly as a review comment. The system prompt is crafted to produce naturally structured output.

**Consequences:**
- + No parsing errors can block a review
- + AI has more freedom to express nuanced feedback
- + Simpler client code — just pass-through the response
- − No "Apply suggestion" buttons on code blocks
- − Can't do line-anchored comments (inline suggestions deferred to v2)

---

## ADR-004: Config via TOML + Env Vars (No Dynaconf)

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent uses Dynaconf, which has complex chained-loading semantics, env var prefix configuration, and surprising nested-key behavior. It caused real debugging pain during testing.

**Decision:** Use `toml` crate for file deserialization + manual `std::env::var` overlay. Config search: `$GITHUB_WORKSPACE/.github/review-agent.toml` → `$CWD/review-agent.toml` → `$CWD/.review-agent.toml` → `~/.config/review-agent/config.toml` → built-in defaults.

**Consequences:**
- + No external config framework dependency
- + Predictable override order: env vars > file > defaults
- + Type-safe via serde Deserialize
- − ~30 lines of boilerplate for env overlay and validation
- − No built-in support for `.env` files (intentional — see SECURITY.md)

---

## ADR-005: Diff Parsing via `similar` Crate (Not Regex)

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent uses regex for unified diff parsing. This is known to be fragile — edge cases include `rename from`/`rename to`, `new file mode`, `Binary files differ`, `\ No newline at end of file`, merge diffs (`@@@`), and `Index:` headers. By fetching the diff via GitHub's `Accept: application/vnd.github.v3.diff` header, we guarantee a standard unified diff format that `similar` can parse reliably.

**Decision:** Use the `similar` crate for diff parsing. It's a pure-Rust, well-tested implementation that handles all standard unified diff edge cases.

**Consequences:**
- + Correct by construction — no regex footguns
- + Handles renames, binaries, and merge diffs gracefully
- + Parse into structured `DiffFile`/`Hunk`/`DiffLine` types
- − One additional crate dependency
- − Slightly less control over the parsing pipeline (but the loss is acceptable)

---

## ADR-006: Token Estimation via Heuristic (3.5 chars/token)

**Status:** Accepted | **Date:** 2026-07-19

**Context:** Accurate token counting requires `tiktoken-rs` or equivalent. While `tiktoken-rs` is now pure Rust (no C dependency as of ~0.5), it still adds maintenance burden and must be kept in sync with model tokenizers.

**Decision:** v1 uses a heuristic: 3.5 characters per token for code (bumped from the naive 4.0 to account for code being denser than prose). Configurable via `max_input_tokens` defaulting to 16,000 with a hard cap at 32,000.

**Consequences:**
- + No `tiktoken-rs` dependency in v1
- + Simpler code — a one-line calculation vs API call to a tokenizer
- + **Safe margin:** Underestimating chars/token (i.e., overestimating token count) ensures we never accidentally exceed the model's context window, acting as a conservative budget cap
- − Imprecise — may over- or under-count for some languages
- − Upgrade to `tiktoken-rs` planned for v2 (it's pure Rust now, so the original "avoid C dep" rationale no longer applies)

---

## ADR-007: `Sensitive<T>` Wrapper for Secrets

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent leaked the OpenAI key into the AI prompt if a certain config chain was triggered. This is unacceptable for a tool that handles private repository code.

**Decision:** Create a `Sensitive<T>` wrapper struct with a `Display` impl that renders as `"***"` and a `Debug` impl that renders as `Sensitive(***)`. All API keys and tokens use this wrapper. `Serialize` also outputs `"***"` so config dumps never leak secrets.

**Consequences:**
- + Secrets can't appear in logs even if a developer forgets to redact — the type system enforces it
- + Clear audit trail — grep for `Sensitive<` to find all secret-holding fields
- − Thin wrapper needs `inner()` / `into_inner()` accessors, adding a tiny amount of boilerplate at use sites
- − `Sensitive<String>` can't directly be passed to APIs expecting `&str` — requires `.inner()` call

---

## ADR-008: Rate Limiting via Semaphore + Governor

**Status:** Accepted | **Date:** 2026-07-19

**Context:** GitHub's API has both concurrency limits (don't flood with simultaneous requests) and rate limits (requests per minute). A `Semaphore` alone handles concurrency but does nothing for token-bucket rate limiting. GitHub's secondary rate limits trigger on burst patterns.

**Decision:** Use `tokio::sync::Semaphore(10)` to cap concurrent in-flight requests + `governor` token bucket (100 req/min burst) for rate limiting. Distinguish 403 (permission) vs rate limit via `X-RateLimit-Remaining: 0` header. Retry 429s with `Retry-After` header parsing.

**Consequences:**
- + Both concurrency AND rate limiting covered — GitHub needs both
- + `governor` is lightweight and purpose-built
- − Two mechanisms instead of one — slightly more code
- − Rate limit tuning may need adjustment based on real usage

---

## ADR-009: AI Timeout at 90s with Exponential Backoff

**Status:** Accepted | **Date:** 2026-07-19

**Context:** AI providers can hang indefinitely on overloaded models. Without an explicit timeout, a GitHub Action workflow could run for minutes.

**Decision:** Wrap every AI call in `tokio::time::timeout(90s)`. Use the `backoff` crate with exponential strategy + jitter, maximum 3 retries, on 429 and 5xx responses only. Log a heartbeat every 10s while waiting.

**Consequences:**
- + Workflows never hang longer than ~7 minutes (4 attempts × 90s + exponential backoff overhead)
- + Jitter prevents thundering herd on retry
- − Timeout may need adjustment for slower models — configurable via `ai.request_timeout_secs`

---

## ADR-010: Bot Detection via Sender Type Check

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent caused a feedback loop when its own comments were parsed as commands, re-triggering the workflow. This was fixed via `github.event.sender.type != 'Bot'` check.

**Decision:** The example workflow includes `if: ${{ github.event.sender.type != 'Bot' }}` to prevent feedback loops. The action entrypoint also ignores events from bot senders.

**Consequences:**
- + No feedback loop with PR-Agent or other bots
- + Users don't need to add the guard themselves — it's built in
- − Legitimate bot PRs (e.g., Dependabot) won't be reviewed — users who want this can remove the guard

---

## ADR-011: Full PR Diff (Not Incremental) for v1

**Status:** Accepted | **Date:** 2026-07-19

**Context:** Incremental reviews (only new commits since last review) require tracking state (commit SHA) and deduplication logic. PR-Agent supports this but it adds significant complexity.

**Decision:** v1 reviews the full cumulative diff between base and head. The noise problem is addressed via prompt design (focus on issues, don't restate obvious changes) and file filtering (skip generated files, lockfiles, vendored code).

**Consequences:**
- + No state management — stateless and simple
- + Full context — AI sees the complete change history
- − Every push re-reviews the entire diff, not just new commits
- − Incremental reviews deferred to v2

---

## ADR-012: File Skip-List and Hard Cost Caps

**Status:** Accepted | **Date:** 2026-07-19

**Context:** Sending large generated diffs to an AI provider costs real money. A 50k-line generated diff could burn through the token budget and produce a poor review.

**Decision:** Built-in skip-list for trivial/generated files: `*lock*`, `CHANGELOG.md`, `*.min.*`, `vendor/*`, `node_modules/*`, `*.pb.*`. Binary files (where GitHub returns no `patch` data) are automatically skipped to prevent parsing errors. Hard caps: `max_diff_files = 50`, `max_total_input_tokens = 32000`. If exceeded, post a warning comment and skip.

**Consequences:**
- + No surprise costs from massive generated diffs
- + Skip-list prevents wasting AI context on uninteresting files
- + Configurable — users can extend the skip-list
- − Some legitimate large PRs will be skipped — users can adjust caps

---

## ADR-013: MSRV 1.85 and Edition 2024

**Status:** Accepted | **Date:** 2026-07-19

**Context:** Edition 2024 is the latest Rust edition with improvements to `impl Trait` handling, borrow checker, and `unsafe` semantics. It requires Rust 1.85+.

**Decision:** Set `edition = "2024"` and `rust-version = "1.85"` in Cargo.toml. Document as MSRV.

**Consequences:**
- + Access to latest language features
- + Better `impl Trait` ergonomics for async code
- − Users on older toolchains need to upgrade
- − Some CI environments may not have 1.85 pre-installed (solved by `dtolnay/rust-toolchain`)

---

## ADR-014: MIT License

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent is Apache 2.0. We may port prompt templates or configuration conventions. A permissive license compatible with Apache 2.0 is needed.

**Decision:** MIT License — permissive, compatible with Apache 2.0, and the most common choice for Rust open-source projects.

**Consequences:**
- + Compatible with PR-Agent's Apache 2.0
- + No restrictions on commercial use
- + Standard for Rust ecosystem

---

## ADR-015: Retry Strategy — Exponential Backoff with Jitter

**Status:** Accepted | **Date:** 2026-07-19

**Context:** Both the GitHub API and the AI API (CloudMagic) can fail transiently — 429 rate limits, 503 service unavailable, connection timeouts. Without retry logic, a PR review workflow fails on the first hiccup, which is unacceptable for an automated CI step.

**Decision:** Use the `backoff` crate with `ExponentialBackoff` strategy + jitter for all HTTP calls. Retry on: `429 Too Many Requests`, `5xx` server errors, and connection timeouts. Do NOT retry on `4xx` client errors other than 429 (those are permanent). Maximum 3 retries per operation. Initial interval 1s, max interval 30s, multiplier 2x, jitter 0.1x.

```rust
// backoff::ExponentialBackoffBuilder
ExponentialBackoffBuilder::new()
    .with_initial_interval(Duration::from_secs(1))
    .with_max_interval(Duration::from_secs(30))
    .with_multiplier(2.0)
    .with_jitter(0.1)
    .with_max_elapsed_time(Some(Duration::from_secs(90)))
    .build()
```

**Consequences:**
- + Transient failures are handled transparently — users see fewer false negatives
- + Jitter prevents thundering herd when multiple instances retry simultaneously
- + Max elapsed time (90s) bounds worst-case delay for the workflow
- − Adds `backoff` crate dependency
- − Masked permanent failures delay error detection — mitigated by logging retry attempts

**Status:** Accepted | **Date:** 2026-07-19

**Context:** If a workflow re-runs (e.g., `synchronize` fires while a previous run is in-flight), PR-Agent will post a duplicate review comment. PR-Agent handles this by finding and updating its own prior comment.

**Decision:** v1 does not track prior reviews. Duplicate comments are possible. This is documented as a known issue.

**Consequences:**
- + Simpler — no state or comment-tracking logic
- − Users may see duplicate reviews on rapid re-triggers
- − Idempotent updates deferred to v2

---

## ADR-016: No Deduplication / Idempotency in v1

**Status:** Accepted | **Date:** 2026-07-19

**Context:** If a workflow re-runs (e.g., `synchronize` fires while a previous run is in-flight), PR-Agent will post a duplicate review comment. PR-Agent handles this by finding and updating its own prior comment.

**Decision:** v1 does not track prior reviews. Duplicate comments are possible. This is documented as a known issue.

**Consequences:**
- + Simpler — no state or comment-tracking logic
- − Users may see duplicate reviews on rapid re-triggers
- − Idempotent updates deferred to v2

---

## ADR-017: Full Git Provider Abstraction Deferred

**Status:** Accepted | **Date:** 2026-07-19

**Context:** PR-Agent has a `GitProvider` abstract base with 12 implementations. This is great for extensibility but adds abstraction overhead.

**Decision:** v1 has a concrete `GitHub` client struct, not a trait-based provider abstraction. The GitHub-specific logic lives in `src/github/mod.rs`. If/when GitLab support is added, a trait can be extracted at that point.

**Consequences:**
- + Simpler — no trait, no dynamic dispatch, no capability probing
- + Faster to implement — write for GitHub, ship, iterate
- − Adding a second provider later will require refactoring (extracting a trait from the concrete struct)

---

## ADR-018: Static Linking for Distroless Docker

**Status:** Accepted | **Date:** 2026-07-19

**Context:** `distroless/cc` contains glibc, but dynamic linking across different base images can cause cryptic runtime crashes if the dynamic linker path differs.

**Decision:** Compile with `RUSTFLAGS='-C target-feature=+crt-static'` and use `gcr.io/distroless/static` as the runtime base image.

**Consequences:**
- + Smaller Docker image size (~20MB vs ~100MB for a full distroless/cc image)
- + Zero dynamic linker issues across different base images
- + Faster startup — no dynamic linker resolution at container start
- − Slightly larger binary on disk due to static linking

---

## ADR-019: GitHub Event Filtering at Entrypoint

**Status:** Accepted | **Date:** 2026-07-19

**Context:** GitHub Actions fires on many PR events (`labeled`, `edited`, `closed`, etc.). Running an AI review on non-code events wastes money and time.

**Decision:** The action entrypoint will explicitly no-op (exit 0) unless the event is `opened`, `synchronize`, or `reopened`. Draft PRs (`draft: true`) are also skipped unless explicitly configured otherwise.

**Consequences:**
- + Reduced API costs, faster CI runs, clear user expectations
- − Users who want draft PR reviews must opt in explicitly

---

## ADR-020: Observability via GitHub Step Summary

**Status:** Accepted | **Date:** 2026-07-19

**Context:** Digging through GitHub Action logs to find AI token usage or latency is poor UX.

**Decision:** The action will append a markdown table to the `$GITHUB_STEP_SUMMARY` environment file detailing: PR size (files/lines), tokens estimated, tokens used, AI latency, and model used.

**Consequences:**
- + Excellent visibility for users, trivial debugging for maintainers
- + No external metrics backend required
- − Step summary has a size limit; very large PRs may truncate the table

---

## ADR-021: Diff Parsing via `diffy` Crate (Not `similar`)

**Status:** Accepted | **Date:** 2026-07-19

**Context:** GitHub's raw diff endpoint (`Accept: application/vnd.github.v3.diff`) returns a standard multi-file unified diff string. We need to parse this into structured `DiffFile`/`Hunk`/`DiffLine` types. A naive first design proposed `similar::ChangeTag`, but `similar` *computes* diffs between two strings — it does **not** parse unified diff text. Feeding a unified diff string into `similar` treats it as plain text.

**Decision:** Use the `diffy` crate (`diffy::Patch::from_str`) for parsing. It is purpose-built for unified diff parsing and handles all standard edge cases natively: `---`/`+++` headers, `@@` hunk ranges, `Binary files differ`, `rename from`/`rename to`, `new file mode`, `deleted file mode`, and `\ No newline at end of file`. For multi-file GitHub diffs, we split on `diff --git a/x b/y` boundaries and parse each section independently.

**Consequences:**
- + Correct by construction — `diffy` is a battle-tested unified-diff parser
- + Handles rename/new/deleted/binary edge cases without custom logic
- + One small dependency (`diffy = "0.4"`)
- − `diffy`'s `Patch` API is single-file; we must split multi-file diffs manually before parsing
- − `diffy` does not preserve GitHub's `diff --git` metadata (we re-derive filename from `modified()`/`original()` and status from mode lines)
- − `similar` crate removed from `Cargo.toml` — confirmed unused after switching to `diffy`

---

## ADR-022: Language Detection via Sorted Slice (Not `phf`)

**Status:** Accepted | **Date:** 2026-07-19

**Context:** The AI prompt benefits from knowing the programming language of each changed file. A lookup table mapping file extensions to language names is needed.

**Decision:** Use a static sorted slice of `(extension, language)` pairs with `binary_search_by_key` for `O(log n)` lookups. Extension extraction uses `Path::extension()` (handles `.spec.ts` → "ts" naturally). Extensionless files like `Dockerfile` are matched by basename first.

**Consequences:**
- + Zero dependencies — no `phf` crate needed for ~40 entries
- + `binary_search_by_key` is fast (≤6 comparisons for 40 entries)
- + Simple to extend — add a tuple, keep the slice sorted
- − Not compile-time perfect hashing (irrelevant at this scale)
- − No file-content sniffing (v1 simplicity over accuracy)

---

## ADR-023: Truncation Drops Files (Not Hunks), Largest First

**Status:** Accepted | **Date:** 2026-07-19

**Context:** When a PR's diff exceeds the token budget, we must truncate. The question is at what granularity: file, hunk, or line?

**Decision:** Truncate at the **file level**, dropping the **largest files first** (by estimated tokens) until the total fits `max_input_tokens`. Never split a hunk or a file mid-way.

**Consequences:**
- + A partial file review provides misleading signal — reviewing fewer complete files is better
- + Dropping largest-first preserves many small-but-impactful changes while shedding massive generated diffs
- − **Caveat:** The largest file may be the core PR change. On very large PRs, the AI might miss the most important file. Accepted v1 trade-off in favor of breadth.
- − Users can raise `max_input_tokens` (default 16,000) if they need deeper coverage
