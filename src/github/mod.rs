mod types;
pub use types::*;

use crate::config::Settings;
use crate::error::{AgentError, Result};
use backoff::ExponentialBackoff;
use backoff::backoff::Backoff;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, StatusCode};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::warn;

/// GitHub API base URL.
const GITHUB_API_BASE: &str = "https://api.github.com";

/// Pull request events for reviews.
#[derive(Debug, Clone)]
pub enum ReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

impl ReviewEvent {
    fn as_str(&self) -> &'static str {
        match self {
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Comment => "COMMENT",
        }
    }
}

/// Client for the GitHub API.
#[derive(Clone)]
pub struct GitHub {
    client: Client,
    semaphore: Arc<Semaphore>,
    rate_limiter: Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
    api_base: String,
}

impl GitHub {
    /// Create a new GitHub client from the application settings.
    pub fn new(settings: &Settings) -> Result<Self> {
        let token = settings.github.token.clone();

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("review-agent/0.1.0"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github.v3+json"),
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token.inner()))
                .map_err(|e| AgentError::Config(format!("Invalid auth header: {}", e)))?,
        );

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(settings.github.request_timeout_secs))
            .build()
            .map_err(|e| AgentError::Config(format!("Failed to build HTTP client: {}", e)))?;

        let max_concurrent = settings.github.max_concurrent_requests;
        let semaphore = Arc::new(Semaphore::new(max_concurrent));

        // 100 requests per minute burst
        let quota = Quota::per_second(NonZeroU32::new(2).unwrap());
        let rate_limiter = Arc::new(RateLimiter::direct(quota));

        Ok(Self {
            client,
            semaphore,
            rate_limiter,
            api_base: GITHUB_API_BASE.to_string(),
        })
    }

    // ──────────────────────────────────────
    // Public API
    // ──────────────────────────────────────

    /// Fetch the raw unified diff for a pull request.
    ///
    /// Returns the diff as a plain text string in standard unified diff format.
    pub async fn get_pr_diff(&self, owner: &str, repo: &str, number: u64) -> Result<String> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.api_base, owner, repo, number
        );
        let diff = self
            .get_with_accept(url, "application/vnd.github.v3.diff")
            .await?;
        Ok(diff)
    }

    /// Fetch pull request metadata (title, description, branch, etc.).
    pub async fn get_pr_metadata(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.api_base, owner, repo, number
        );
        self.get_json(&url).await
    }

    /// Fetch the list of files changed in a pull request.
    pub async fn get_pr_files(&self, owner: &str, repo: &str, number: u64) -> Result<Vec<PrFile>> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/files",
            self.api_base, owner, repo, number
        );
        self.get_json(&url).await
    }

    /// Post a review on a pull request.
    ///
    /// `event` controls the review type: Approve, RequestChanges, or Comment.
    /// Pass `None` for a simple comment review.
    pub async fn publish_review(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
        event: Option<ReviewEvent>,
    ) -> Result<ReviewResponse> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/reviews",
            self.api_base, owner, repo, number
        );

        let review_body = ReviewBody {
            body: body.to_string(),
            event: event.as_ref().map(|e| e.as_str().to_string()),
            commit_id: None,
        };

        self.post_json(&url, &review_body).await
    }

    /// Post a comment on a pull request (as an issue comment).
    pub async fn publish_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<Comment> {
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            self.api_base, owner, repo, number
        );

        #[derive(serde::Serialize)]
        struct CommentBody<'a> {
            body: &'a str,
        }

        self.post_json(&url, &CommentBody { body }).await
    }

    /// Fetch the language breakdown for a repository.
    pub async fn get_languages(&self, owner: &str, repo: &str) -> Result<LanguageBreakdown> {
        let url = format!("{}/repos/{}/{}/languages", self.api_base, owner, repo);
        self.get_json(&url).await
    }

    // ──────────────────────────────────────
    // Internal HTTP helpers
    // ──────────────────────────────────────

    /// Perform a GET request and return the response body as a plain string.
    /// Used for fetching raw diffs with a custom Accept header.
    async fn get_with_accept(&self, url: String, accept: &str) -> Result<String> {
        let _permit = self.semaphore.acquire().await.unwrap();
        self.rate_limiter.until_ready().await;

        let response = self
            .retry(move || {
                let client = self.client.clone();
                let url = url.clone();
                let accept = accept.to_string();
                async move {
                    let resp = client
                        .get(&url)
                        .header(ACCEPT, HeaderValue::from_str(&accept).unwrap())
                        .send()
                        .await?;

                    let status = resp.status();
                    if status.is_success() {
                        let text = resp.text().await?;
                        Ok(text)
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        Err(classify_error(status, &text))
                    }
                }
            })
            .await?;

        Ok(response)
    }

    /// Perform a GET request and deserialize the response as JSON.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let _permit = self.semaphore.acquire().await.unwrap();
        self.rate_limiter.until_ready().await;

        let url = url.to_string();
        let response = self
            .retry(move || {
                let client = self.client.clone();
                let url = url.clone();
                async move {
                    let resp = client.get(&url).send().await?;
                    let status = resp.status();
                    if status.is_success() {
                        let json = resp.json().await?;
                        Ok(json)
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        Err(classify_error(status, &text))
                    }
                }
            })
            .await?;

        Ok(response)
    }

    /// Perform a POST request with a JSON body and deserialize the response.
    async fn post_json<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        url: &str,
        body: &B,
    ) -> Result<T> {
        let _permit = self.semaphore.acquire().await.unwrap();
        self.rate_limiter.until_ready().await;

        let url = url.to_string();
        let body_json = serde_json::to_value(body)?;
        let response = self
            .retry(move || {
                let client = self.client.clone();
                let url = url.clone();
                let body = body_json.clone();
                async move {
                    let resp = client.post(&url).json(&body).send().await?;
                    let status = resp.status();
                    if status.is_success() || status == StatusCode::OK {
                        let json = resp.json().await?;
                        Ok(json)
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
    /// Retries only on transient errors (429, 5xx). Permanent errors (4xx
    /// other than 429) are returned immediately without retry.
    async fn retry<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, AgentError>>,
    {
        let mut backoff = ExponentialBackoff {
            initial_interval: Duration::from_secs(1),
            max_interval: Duration::from_secs(30),
            multiplier: 2.0,
            max_elapsed_time: Some(Duration::from_secs(90)),
            ..ExponentialBackoff::default()
        };

        loop {
            match f().await {
                Ok(val) => return Ok(val),
                Err(e) => {
                    if e.is_transient() {
                        match backoff.next_backoff() {
                            Some(duration) => {
                                warn!(error = %e, retry_after_ms = %duration.as_millis(), "Retrying after transient error");
                                tokio::time::sleep(duration).await;
                            }
                            None => {
                                return Err(AgentError::GitHub(format!(
                                    "Max retries exceeded: {}",
                                    e
                                )));
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

/// Classify an HTTP response status code into an AgentError.
fn classify_error(status: StatusCode, body: &str) -> AgentError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            let msg = if body.contains("rate limit") {
                "GitHub API rate limit exceeded".to_string()
            } else {
                format!("GitHub API authentication failed ({}): {}", status, body)
            };
            AgentError::GitHub(msg)
        }
        StatusCode::NOT_FOUND => {
            AgentError::GitHub(format!("GitHub resource not found (404): {}", body))
        }
        s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => {
            AgentError::GitHub(format!("GitHub API transient error ({}): {}", status, body))
        }
        s => AgentError::GitHub(format!("GitHub API error ({}): {}", s, body)),
    }
}

// classify_error is defined above — is_transient now lives in error.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pr_deserialization() {
        let pr_json = serde_json::json!({
            "number": 42,
            "title": "Test PR",
            "body": "Description here",
            "html_url": "https://github.com/owner/repo/pull/42",
            "state": "open",
            "user": { "login": "testuser" },
            "head": { "label": "owner:feature", "ref": "feature", "sha": "abc123", "repo": null },
            "base": { "label": "owner:main", "ref": "main", "sha": "def456", "repo": null }
        });

        let pr = serde_json::from_value::<PullRequest>(pr_json).unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "Test PR");
        assert_eq!(pr.head.r#ref, "feature");
        assert_eq!(pr.base.r#ref, "main");
    }

    #[test]
    fn test_classify_errors() {
        let err = classify_error(StatusCode::UNAUTHORIZED, "bad credentials");
        assert!(err.to_string().contains("authentication failed"));

        let err = classify_error(StatusCode::NOT_FOUND, "not found");
        assert!(err.to_string().contains("not found"));

        let err = classify_error(StatusCode::TOO_MANY_REQUESTS, "");
        assert!(err.to_string().contains("transient"));

        let err = classify_error(StatusCode::FORBIDDEN, "rate limit exceeded");
        assert!(err.to_string().contains("rate limit"));
    }

    #[test]
    fn test_transient_detection() {
        let transient = AgentError::GitHub("GitHub API transient error (503)".into());
        assert!(transient.is_transient());

        let rate = AgentError::GitHub("GitHub API rate limit exceeded".into());
        assert!(rate.is_transient());

        let auth = AgentError::GitHub("GitHub API authentication failed".into());
        assert!(!auth.is_transient());

        let config = AgentError::Config("test".into());
        assert!(!config.is_transient());
    }
}
