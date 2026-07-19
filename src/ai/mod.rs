mod types;
pub use types::*;

use crate::config::Settings;
use crate::error::{AgentError, Result};
use crate::sensitive::Sensitive;
use backoff::ExponentialBackoff;
use backoff::backoff::Backoff;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use std::time::Duration;
use tracing::warn;

/// The maximum total wall-clock time for all retry attempts combined.
/// Distinct from the per-request `request_timeout` so short timeouts
/// don't starve the retry budget.
const AI_RETRY_MAX_ELAPSED: Duration = Duration::from_secs(300);

/// Client for any OpenAI-compatible chat completion API.
#[derive(Clone)]
pub struct AiClient {
    client: Client,
    api_base: String,
    model: String,
    api_key: Sensitive<String>,
    temperature: f64,
    max_completion_tokens: u32,
}

impl AiClient {
    /// Create a new AI client from the application settings.
    pub fn new(settings: &Settings) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(settings.ai.request_timeout_secs))
            .build()
            .map_err(|e| AgentError::Config(format!("Failed to build HTTP client: {}", e)))?;

        Ok(Self {
            client,
            api_base: settings.ai.api_base.clone(),
            model: settings.ai.model.clone(),
            api_key: settings.ai.api_key.clone(),
            temperature: settings.ai.temperature,
            max_completion_tokens: settings.ai.max_completion_tokens,
        })
    }

    /// Send a chat completion request and return the response text.
    ///
    /// The system message sets the AI's behavior, and the user message
    /// contains the PR diff + metadata to review.
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let url = format!("{}/chat/completions", self.api_base.trim_end_matches('/'));

        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: system.to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: user.to_string(),
                },
            ],
            temperature: Some(self.temperature),
            max_tokens: Some(self.max_completion_tokens),
        };

        let response = self
            .retry(|| {
                let client = self.client.clone();
                let url = url.clone();
                let request = request.clone();
                let api_key = self.api_key.clone();
                async move {
                    let resp = client
                        .post(&url)
                        .header(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {}", api_key.inner())).map_err(
                                |e| {
                                    AgentError::Config(format!(
                                        "Invalid API key for auth header: {}",
                                        e
                                    ))
                                },
                            )?,
                        )
                        .json(&request)
                        .send()
                        .await?;

                    let status = resp.status();
                    if status.is_success() {
                        let chat_resp: ChatResponse = match resp.json().await {
                            Ok(c) => c,
                            Err(e) => {
                                return Err(AgentError::Ai(format!(
                                    "AI API non-parseable response ({}): {}",
                                    status, e
                                )));
                            }
                        };
                        let content = chat_resp
                            .choices
                            .into_iter()
                            .next()
                            .and_then(|c| c.message.content)
                            .unwrap_or_default();
                        Ok(content)
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        Err(classify_error(status, &text))
                    }
                }
            })
            .await?;

        Ok(response)
    }

    /// Retry a fallible async operation with exponential backoff.
    ///
    /// Retries only on transient errors (429, 5xx, connection timeouts).
    async fn retry<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, AgentError>>,
    {
        let mut backoff = ExponentialBackoff {
            initial_interval: Duration::from_secs(1),
            current_interval: Duration::from_secs(1),
            max_interval: Duration::from_secs(30),
            multiplier: 2.0,
            max_elapsed_time: Some(AI_RETRY_MAX_ELAPSED),
            ..ExponentialBackoff::default()
        };

        loop {
            match f().await {
                Ok(val) => return Ok(val),
                Err(e) => {
                    if e.is_transient() {
                        match backoff.next_backoff() {
                            Some(duration) => {
                                warn!(
                                    error = %e,
                                    retry_after_ms = %duration.as_millis(),
                                    "Retrying AI request after transient error"
                                );
                                tokio::time::sleep(duration).await;
                            }
                            None => {
                                return Err(AgentError::Ai(format!("Max retries exceeded: {}", e)));
                            }
                        }
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// Classify an AI API response status code into an AgentError.
///
/// # Contract
/// The message prefixes produced here are part of the public contract with
/// `AgentError::is_transient()` in `error.rs`. Do NOT change the prefixes
/// (`"AI API rate limit exceeded (429)"`, `"AI API server error (5"`) without
/// updating the corresponding patterns in `is_transient`, or retry behavior
/// will silently break. Transient statuses: 429 (rate limit), 5xx (server).
/// Maximum number of characters from a response body included in an error
/// message. Keeps diagnostics useful without risking leakage of large or
/// sensitive payloads into logs / error output.
const MAX_BODY_SNIPPET: usize = 200;

/// Trim a response body to a safe snippet length for error messages.
fn body_snippet(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= MAX_BODY_SNIPPET {
        trimmed.to_string()
    } else {
        format!("{}…[truncated]", &trimmed[..MAX_BODY_SNIPPET])
    }
}

fn classify_error(status: reqwest::StatusCode, body: &str) -> AgentError {
    let snippet = body_snippet(body);
    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => AgentError::Ai(
            format!("AI API authentication failed ({}): {}", status, snippet),
        ),
        reqwest::StatusCode::NOT_FOUND => {
            AgentError::Ai(format!("AI API endpoint not found (404): {}", snippet))
        }
        reqwest::StatusCode::TOO_MANY_REQUESTS => {
            AgentError::Ai(format!("AI API rate limit exceeded (429): {}", snippet))
        }
        s if s.is_server_error() => {
            AgentError::Ai(format!("AI API server error ({}): {}", s, snippet))
        }
        s => AgentError::Ai(format!("AI API error ({}): {}", s, snippet)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensitive::Sensitive;

    fn test_settings() -> Settings {
        let mut s = Settings::default();
        s.ai.api_key = Sensitive::new("sk-test-key".into());
        s.ai.api_base = "https://ai.cloudmagic.io/v1".into();
        s.ai.model = "glm-4.6".into();
        s.github.token = Sensitive::new("ghp_test".into());
        s
    }

    #[test]
    fn test_chat_request_serialization() {
        let request = ChatRequest {
            model: "glm-4.6".to_string(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: "You are a reviewer".to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: "Review this diff".to_string(),
                },
            ],
            temperature: Some(0.2),
            max_tokens: Some(4096),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "glm-4.6");
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["content"], "Review this diff");
        assert_eq!(json["temperature"], 0.2);
        assert_eq!(json["max_completion_tokens"], 4096);
    }

    #[test]
    fn test_chat_response_deserialization() {
        let json = serde_json::json!({
            "id": "chatcmpl-123",
            "choices": [{
                "message": { "content": "## Review\n\nLooks good." },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            }
        });

        let response: ChatResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.choices.len(), 1);
        assert_eq!(
            response.choices[0].message.content.as_deref(),
            Some("## Review\n\nLooks good.")
        );
        assert_eq!(response.usage.unwrap().total_tokens, Some(150));
    }

    #[test]
    fn test_empty_content_response() {
        let json = serde_json::json!({
            "choices": [{
                "message": { "content": null },
                "finish_reason": "stop"
            }]
        });

        let response: ChatResponse = serde_json::from_value(json).unwrap();
        let content = response
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        assert_eq!(content, "");
    }

    #[test]
    fn test_classify_errors() {
        let err = classify_error(reqwest::StatusCode::UNAUTHORIZED, "invalid api key");
        assert!(err.to_string().contains("authentication failed"));

        let err = classify_error(reqwest::StatusCode::TOO_MANY_REQUESTS, "rate limited");
        assert!(err.to_string().contains("rate limit"));

        let err = classify_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "server error");
        assert!(err.to_string().contains("server error"));
    }

    #[test]
    fn test_classify_error_truncates_long_body() {
        let long_body = "x".repeat(500);
        let err = classify_error(reqwest::StatusCode::BAD_REQUEST, &long_body);
        let msg = err.to_string();
        assert!(msg.contains("[truncated]"));
        // The error message should be much shorter than 500 chars.
        assert!(msg.len() < 300);
    }

    #[test]
    fn test_transient_detection() {
        // These match the actual output format of classify_error() in this module
        let rate = AgentError::Ai("AI API rate limit exceeded (429): ".into());
        assert!(rate.is_transient());

        let server = AgentError::Ai("AI API server error (503): ".into());
        assert!(server.is_transient());

        let auth = AgentError::Ai("AI API authentication failed (401): ".into());
        assert!(!auth.is_transient());

        let not_found = AgentError::Ai("AI API endpoint not found (404): ".into());
        assert!(!not_found.is_transient());
    }

    #[test]
    fn test_client_construction() {
        let client = AiClient::new(&test_settings()).unwrap();
        assert_eq!(client.model, "glm-4.6");
        assert_eq!(client.api_base, "https://ai.cloudmagic.io/v1");
        assert_eq!(client.api_key.inner(), "sk-test-key");
    }
}
