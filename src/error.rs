use thiserror::Error;

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("Config error: {0}")]
    Config(String),

    #[error("GitHub API error: {0}")]
    GitHub(String),

    #[error("AI API error: {0}")]
    Ai(String),

    #[error("Diff parse error: {0}")]
    Diff(String),

    #[error("Token budget exceeded: {used} > {limit}")]
    TokenBudget { used: usize, limit: usize },

    #[error("Operation timed out after {0}s")]
    Timeout(u64),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
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
    fn invalid_url_message() {
        let err = AgentError::InvalidUrl("bad url".into());
        assert_eq!(err.to_string(), "Invalid URL: bad url");
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
