# Contributing

We welcome contributions! Here's how to get started.

## Prerequisites
- Rust 1.85+ (MSRV)
- Docker (for building the Action image)

## Development

```bash
# Build
cargo build

# Run tests
cargo test

# Lint
cargo clippy

# Format
cargo fmt --check

# Run a review locally

> **Prerequisite:** Set `GITHUB_TOKEN` and `AI_API_KEY` in your environment first.

```bash
cargo run -- review --pr-url https://github.com/owner/repo/pull/1
```
```

## Project Structure
```
src/
├── main.rs          # CLI + Action dispatch
├── lib.rs           # module tree
├── config.rs        # Settings (TOML + env)
├── error.rs         # AgentError enum
├── sensitive.rs     # Sensitive<T> wrapper
├── github/          # GitHub API client
├── diff.rs          # diff parser
├── tokens.rs        # token budget
├── ai/              # AI client
└── tools/           # review orchestrator
```

## Code Style
- Run `cargo fmt` before committing
- Clippy must pass with no warnings
- Keep `default-features = false` on dependencies where possible to minimize binary size
- Add tests for new functionality — use `wiremock` for HTTP mocking

## Pull Request Process
1. Ensure `cargo test` passes and `cargo clippy` is clean
2. Update documentation (README, AGENTS.md) for any new features
3. For config changes, update `review-agent.toml` and the README config reference

## Security
See `SECURITY.md` for security-related contribution guidelines.
