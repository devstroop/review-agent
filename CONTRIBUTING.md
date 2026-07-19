# Contributing

We welcome contributions! Here's how to get started.

## Prerequisites
- Rust 1.85+ (MSRV)
- Docker (for building the Action image)

## Development

```bash
# Build
cargo build

# Run all tests
cargo test

# Run specific test
cargo test test_name

# Lint (must pass with no warnings)
cargo clippy -- -D warnings

# Check formatting
cargo fmt --check

# Generate documentation
cargo doc --no-deps --open

# Run a review locally
# Prerequisite: Set GITHUB_TOKEN and AI_API_KEY in your environment first.
cargo run -- review --pr-url https://github.com/owner/repo/pull/1
```

## Project Structure
```
src/
├── main.rs           # CLI + Action dispatch
├── lib.rs            # Module tree + re-exports
├── config.rs         # Settings (TOML + env overlay, Sensitive<T> secrets)
├── error.rs          # AgentError enum (11 variants, is_transient)
├── sensitive.rs      # Sensitive<T> wrapper (redacts Display/Debug/Serialize)
├── logging.rs        # tracing subscriber (env-filter, JSON)
├── github/           # GitHub API client (reqwest, rate-limit, retry)
│   ├── mod.rs
│   └── types.rs
├── diff.rs           # Unified diff parser via diffy
├── tokens.rs         # Token estimation (3.5 chars/token) + budget enforcement
├── language.rs       # Extension → language lookup
├── ai/               # OpenAI-compatible chat client
│   ├── mod.rs
│   └── types.rs
└── tools/            # Review tool orchestrator
    ├── mod.rs
    └── review.rs
```

## Code Style
- Run `cargo fmt` before committing
- Clippy must pass with `-- -D warnings` (warnings are errors)
- Keep `default-features = false` on dependencies where possible to minimize binary size
- Add tests for new functionality — use `wiremock` for HTTP mocking (no network in CI)
- All public types and functions must have `///` doc comments

## Pull Request Process
1. Create a feature branch from `master`
2. Ensure `cargo test` passes and `cargo clippy -- -D warnings` is clean
3. Ensure `cargo fmt --check` passes
4. Update documentation (README, AGENTS.md) for any new features
5. For config changes, update `review-agent.toml` and the README config reference
6. Open a PR against `master` with a clear description of the change

## Security
See `SECURITY.md` for security-related contribution guidelines.
