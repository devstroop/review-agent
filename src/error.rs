//! Unified error types for all review-agent operations.
//!
//! [`AgentError`] is the single error enum used across every module. Each
//! variant carries a human-readable message via `thiserror`. The
//! `AgentError::is_transient()` distinguishes retryable errors (rate
//! limits, server errors, timeouts) from permanent failures (config errors,
//! parse errors, bad URLs).
//!
//! [`Result<T>`] is a convenience alias over `std::result::Result<T, AgentError>`.

use thiserror::Error;

/// All errors that can occur during PR review processing.
///
/// Each variant wraps either a static message string (via `thiserror`'s
/// `#[error("...")]` attribute) or transparently delegates to an underlying
/// error type via `#[from]`.
///
/// Use `is_transient()` to determine whether an
/// error is safe to retry.
#[derive(Error, Debug)]
pub enum AgentError {
    /// A configuration error — missing required env var, invalid value, etc.
    ///
    /// These are **permanent** — retrying will not fix them.
    #[error("Config error: {0}")]
    Config(String),

    /// A GitHub API error — non-transient 4xx response (e.g. 404, 403).
    ///
    /// Transient GitHub errors (429, 5xx) are identified by message prefix
    /// in `is_transient()`.
    #[error("GitHub API error: {0}")]
    GitHub(String),

    /// An AI API error — non-transient error from the chat endpoint.
    ///
    /// Transient AI errors (429, 5xx) are identified by message prefix
    /// in `is_transient()`.
    #[error("AI API error: {0}")]
    Ai(String),

    /// A diff parsing error — the raw diff text is structurally invalid.
    #[error("Diff parse error: {0}")]
    Diff(String),

    /// The token budget was exceeded by the diff content.
    ///
    /// Applies when `used` tokens exceed the `limit` configured in
    /// `review.max_input_tokens` (default 16,000, hard cap 32,000).
    #[error("Token budget exceeded: {used} > {limit}")]
    TokenBudget { used: usize, limit: usize },

    /// An operation timed out — includes the configured timeout in seconds.
    ///
    /// These are **transient** — a retry may succeed if the server has
    /// recovered or load has decreased.
    #[error("Operation timed out after {0}s")]
    Timeout(u64),

    /// An I/O error from the standard library (file read/write, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// An HTTP error from `reqwest` — connection failure, DNS resolution,
    /// TLS handshake failure, etc.
    ///
    /// These are **transient** — the underlying connection issue may resolve.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// A JSON serialization/deserialization error from `serde_json`.
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A TOML parse error from the `toml` crate.
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),

    // (reserved for URL parse errors — currently handled via anyhow in parse_pr_url)
}

impl AgentError {
    /// Returns `true` if the error is transient and retryable.
    ///
    /// Transient errors include:
    /// - HTTP 429 (rate limit)
    /// - HTTP 5xx (server errors)
    /// - Connection timeouts / `reqwest::Error`
    ///
    /// Detection strategy:
    /// - GitHub errors: checked via known prefix strings from the GitHub API error formatter
    ///   (e.g. "GitHub API transient error (5", "GitHub API error (429)")
    /// - AI errors: checked via known prefix strings from the AI API error formatter
    ///   (e.g. "AI API rate limit exceeded (429)", "AI API server error (5")
    /// - `Timeout` and `Http` variants are always transient.
    ///
    /// # Contract
    /// The AI/GitHub string prefixes below are coupled to the `classify_error`
    /// formatters in `src/ai/mod.rs` and `src/github/mod.rs`. If those formatters
    /// change their prefixes, this method must be updated in lockstep.
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::GitHub(msg) => {
                msg.starts_with("GitHub API transient error")
                    || msg.starts_with("GitHub API rate limit exceeded")
                    || msg.starts_with("GitHub API error (5")
                    || msg.starts_with("GitHub API error (429)")
            }
            Self::Ai(msg) => {
                msg.starts_with("AI API rate limit exceeded (429)")
                    || msg.starts_with("AI API server error (5")
            }
            Self::Timeout(_) | Self::Http(_) => true,
            _ => false,
        }
    }
}

/// Convenience alias for `Result<T, AgentError>` used across the entire crate.
pub type Result<T> = std::result::Result<T, AgentError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_message() {
        let err = AgentError::Config("missing GITHUB_TOKEN".into());
        assert_eq!(err.to_string(), "Config error: missing GITHUB_TOKEN");
    }

    #[test]
    fn github_error_message() {
        let err = AgentError::GitHub("404 Not Found".into());
        assert_eq!(err.to_string(), "GitHub API error: 404 Not Found");
    }

    #[test]
    fn ai_error_message() {
        let err = AgentError::Ai("rate limited".into());
        assert_eq!(err.to_string(), "AI API error: rate limited");
    }

    #[test]
    fn token_budget_message() {
        let err = AgentError::TokenBudget {
            used: 50000,
            limit: 32000,
        };
        assert!(err.to_string().contains("50000"));
        assert!(err.to_string().contains("32000"));
    }

    #[test]
    fn timeout_message() {
        let err = AgentError::Timeout(90);
        assert_eq!(err.to_string(), "Operation timed out after 90s");
    }

    #[test]
    fn io_error_conversion() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: AgentError = io.into();
        assert!(matches!(err, AgentError::Io(_)));
    }

    #[test]
    fn serde_error_conversion() {
        let serde = serde_json::from_str::<()>("invalid").unwrap_err();
        let err: AgentError = serde.into();
        assert!(matches!(err, AgentError::Serde(_)));
    }

    #[test]
    fn result_type_alias_works() {
        fn returns_result() -> Result<String> {
            Ok("hello".into())
        }
        assert_eq!(returns_result().unwrap(), "hello");
    }

    #[test]
    fn result_type_alias_err() {
        fn returns_err() -> Result<String> {
            Err(AgentError::Config("fail".into()))
        }
        assert!(returns_err().is_err());
    }

    #[test]
    fn debug_format() {
        let err = AgentError::Config("test".into());
        let debug = format!("{:?}", err);
        assert!(debug.contains("Config"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn display_includes_variant_hint() {
        let err = AgentError::GitHub("forbidden".into());
        let s = err.to_string();
        assert!(s.contains("GitHub"));
        assert!(s.contains("forbidden"));
    }
}
