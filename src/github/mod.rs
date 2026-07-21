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

/// Parse a GitHub PR URL of the form `https://github.com/{owner}/{repo}/pull/{number}`
/// into its components.  Also accepts `www.github.com`.
///
/// Extra trailing path segments (e.g. `/files`) are tolerated.
/// Returns a descriptive error string on failure.
pub fn parse_pr_url(url_str: &str) -> std::result::Result<(String, String, u64), String> {
    let url_str = url_str.trim_end_matches('/');
    let parsed = url::Url::parse(url_str).map_err(|_| {
        format!("Invalid URL: expected https://github.com/owner/repo/pull/N, got '{url_str}'")
    })?;

    if parsed.scheme() != "https" {
        return Err(format!(
            "URL scheme must be https, got '{}'",
            parsed.scheme()
        ));
    }

    let host = parsed.host_str().unwrap_or("");
    if !host.eq_ignore_ascii_case("github.com") && !host.eq_ignore_ascii_case("www.github.com") {
        return Err(format!("URL host must be github.com, got '{host}'"));
    }

    // url::Url::path_segments() returns raw percent-encoded segments.
    // The url crate does NOT automatically decode them — we decode only
    // the owner and repo segments via percent_encoding::percent_decode_str
    // before validation.  This prevents `%2F` from being interpreted as
    // a path separator, which would enable path traversal in downstream
    // API calls.
    //
    // Double-encoded sequences such as `%252E%252E` (which would decode to
    // `..` after two passes) are also blocked: after a single decode they
    // become `%2E%2E`, and `valid_segment` below rejects any segment
    // containing `%` (which is not in the allowed alphanumeric/.-_ set).
    // Both `..` (direct) and `%2E%2E` (after partial decode) are caught
    // by the same check.  No need for iterative decoding or raw-segment
    // scanning.
    let segments: Vec<String> = parsed
        .path_segments()
        .map(|s| {
            s.map(|seg| {
                percent_encoding::percent_decode_str(seg)
                    .decode_utf8()
                    .map(|c| c.into_owned())
                    .map_err(|_| format!("Path segment contains invalid UTF-8: '{seg}'"))
            })
            .collect::<std::result::Result<Vec<String>, String>>()
        })
        .transpose()?
        .unwrap_or_default();

    // Reject URLs with too few segments or a non-"pull" third segment.
    if segments.len() < 4 || !segments[2].eq_ignore_ascii_case("pull") {
        return Err(format!(
            "URL path must be /owner/repo/pull/N, got '/{}'",
            segments.join("/")
        ));
    }

    // Warn when extra trailing segments are present (e.g. /files, /commits).
    if segments.len() > 4 {
        tracing::warn!(
            "PR URL has {} trailing path segment(s) — only the first 4 are used",
            segments.len() - 4
        );
    }

    // Security: The `url` crate does NOT normalise `..` path segments; it
    // returns them verbatim.  Path traversal is prevented by the explicit
    // `valid_segment` check below, which rejects segments containing `..`.

    let number: u64 = segments[3]
        .parse()
        .map_err(|e| format!("Invalid PR number '{}': {e}", segments[3]))?;

    // Validate owner and repo segments are safe identifiers — only
    // alphanumeric, `.`, `_`, `-`.  This prevents path traversal in
    // downstream API calls.  Other segments (e.g. "pull", the PR
    // number) are validated by their own logic (case-insensitive
    // equality for "pull", u64 parse for the number).
    let valid_segment = |s: &str| -> bool {
        !s.is_empty()
            && !s.contains('/')
            && !s.contains("..")
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    };

    if !valid_segment(&segments[0]) {
        return Err(format!("Invalid owner '{}'", segments[0]));
    }
    if !valid_segment(&segments[1]) {
        return Err(format!("Invalid repo '{}'", segments[1]));
    }

    // Security: reject `..` in any path segment to prevent path traversal.
    // Only the owner and repo get the full character whitelist; other
    // segments are checked only for this specific threat.
    for seg in &segments[2..] {
        if seg.contains("..") {
            return Err(format!("Invalid path segment '{seg}'"));
        }
    }

    Ok((segments[0].clone(), segments[1].clone(), number))
}

#[cfg(test)]
mod parse_pr_url_tests {
    use super::*;

    #[test]
    fn parse_valid_pr_url() {
        assert_eq!(
            parse_pr_url("https://github.com/devstroop/review-agent/pull/42").unwrap(),
            ("devstroop".into(), "review-agent".into(), 42)
        );
    }

    #[test]
    fn parse_pr_url_with_trailing_slash() {
        assert_eq!(
            parse_pr_url("https://github.com/o/r/pull/7/").unwrap(),
            ("o".into(), "r".into(), 7)
        );
    }

    #[test]
    fn parse_pr_url_with_query() {
        let (_owner, _repo, number) =
            parse_pr_url("https://github.com/a/b/pull/99?foo=bar").unwrap();
        assert_eq!(number, 99);
    }

    #[test]
    fn parse_invalid_pr_url() {
        assert!(parse_pr_url("https://github.com/devstroop/review-agent").is_err());
        assert!(parse_pr_url("not-a-url").is_err());
        assert!(parse_pr_url("https://github.com/o/r/pull/abc").is_err());
    }

    #[test]
    fn parse_pr_url_with_fragment() {
        let (owner, repo, number) = parse_pr_url("https://github.com/o/r/pull/7#section").unwrap();
        assert_eq!((owner.as_str(), repo.as_str(), number), ("o", "r", 7));
    }

    #[test]
    fn parse_pr_url_extra_trailing_segments() {
        // Extra trailing segments (e.g. /files, /commits) are tolerated.
        let (owner, repo, number) = parse_pr_url("https://github.com/o/r/pull/123/files").unwrap();
        assert_eq!((owner.as_str(), repo.as_str(), number), ("o", "r", 123));

        let (owner, repo, number) =
            parse_pr_url("https://github.com/o/r/pull/42/commits/abc123").unwrap();
        assert_eq!((owner.as_str(), repo.as_str(), number), ("o", "r", 42));
    }

    #[test]
    fn parse_pr_url_wrong_host_rejected() {
        assert!(parse_pr_url("https://gitlab.com/o/r/pull/1").is_err());
        assert!(parse_pr_url("https://malicious.example.com/o/r/pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_http_scheme_rejected() {
        assert!(parse_pr_url("http://github.com/o/r/pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_case_insensitive_host() {
        assert_eq!(parse_pr_url("https://GITHUB.COM/o/r/pull/1").unwrap().2, 1);
        assert_eq!(parse_pr_url("https://Github.com/o/r/pull/1").unwrap().2, 1);
    }

    #[test]
    fn parse_pr_url_www_subdomain() {
        let (owner, repo, number) = parse_pr_url("https://www.github.com/o/r/pull/1").unwrap();
        assert_eq!((owner.as_str(), repo.as_str(), number), ("o", "r", 1));
    }

    #[test]
    fn parse_pr_url_percent_decoded_rejected() {
        // Percent-decoded `/` would enable path traversal — must be rejected.
        assert!(parse_pr_url("https://github.com/user%2Fname/repo%2Btest/pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_case_insensitive_pull() {
        assert_eq!(parse_pr_url("https://github.com/o/r/Pull/1").unwrap().2, 1);
        assert_eq!(parse_pr_url("https://github.com/o/r/PULL/2").unwrap().2, 2);
        assert_eq!(parse_pr_url("https://github.com/o/r/pUlL/3").unwrap().2, 3);
    }

    #[test]
    fn parse_pr_url_invalid_owner_rejected() {
        assert!(parse_pr_url("https://github.com//r/pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_invalid_repo_rejected() {
        assert!(parse_pr_url("https://github.com/o//pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_path_traversal_rejected() {
        assert!(parse_pr_url("https://github.com/o/r/pull/1/../../leak").is_err());
    }
}

/// Pull request review event — currently only Comment is supported.
#[derive(Debug, Clone)]
pub enum ReviewEvent {
    Comment,
}

impl ReviewEvent {
    fn as_str(&self) -> &'static str {
        match self {
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

        // Validate the custom API base URL if one was provided.
        let api_base = if settings.github.base_url.is_empty() {
            GITHUB_API_BASE.to_string()
        } else {
            let trimmed = settings.github.base_url.trim_end_matches('/');
            url::Url::parse(trimmed).map_err(|e| {
                AgentError::Config(format!(
                    "Invalid github.base_url '{}': {}",
                    settings.github.base_url, e
                ))
            })?;
            trimmed.to_string()
        };

        Ok(Self {
            client,
            semaphore,
            rate_limiter,
            api_base,
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

    // ──────────────────────────────────────
    // Internal HTTP helpers
    // ──────────────────────────────────────

    /// Perform a GET request and return the response body as a plain string.
    /// Used for fetching raw diffs with a custom Accept header.
    async fn get_with_accept(&self, url: String, accept: &str) -> Result<String> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| AgentError::GitHub(format!("Semaphore acquire failed: {}", e)))?;
        self.rate_limiter.until_ready().await;

        let accept = accept.to_string();
        let response = self
            .retry(move || {
                let client = self.client.clone();
                let url = url.clone();
                let accept = accept.clone();
                async move {
                    let resp = client
                        .get(&url)
                        .header(
                            ACCEPT,
                            HeaderValue::from_str(&accept).map_err(|e| {
                                AgentError::Config(format!("Invalid Accept header: {}", e))
                            })?,
                        )
                        .send()
                        .await?;

                    let status = resp.status();
                    if status.is_success() {
                        let text = resp.text().await?;
                        Ok(text)
                    } else {
                        let rate_remaining = resp
                            .headers()
                            .get("X-RateLimit-Remaining")
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        let text = resp.text().await.unwrap_or_default();
                        Err(classify_error(status, &text, rate_remaining.as_deref()))
                    }
                }
            })
            .await?;

        Ok(response)
    }

    /// Perform a GET request and deserialize the response as JSON.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| AgentError::GitHub(format!("Semaphore acquire failed: {}", e)))?;
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
                        let rate_remaining = resp
                            .headers()
                            .get("X-RateLimit-Remaining")
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        let text = resp.text().await.unwrap_or_default();
                        Err(classify_error(status, &text, rate_remaining.as_deref()))
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
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| AgentError::GitHub(format!("Semaphore acquire failed: {}", e)))?;
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
                    if status.is_success() {
                        let json = resp.json().await?;
                        Ok(json)
                    } else {
                        let rate_remaining = resp
                            .headers()
                            .get("X-RateLimit-Remaining")
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        let text = resp.text().await.unwrap_or_default();
                        Err(classify_error(status, &text, rate_remaining.as_deref()))
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
///
/// Uses the `X-RateLimit-Remaining` response header (when available) to
/// accurately distinguish 403 rate-limit errors from 403 permission errors,
/// rather than relying on body text matching alone (ADR-008).
fn classify_error(
    status: StatusCode,
    body: &str,
    rate_limit_remaining: Option<&str>,
) -> AgentError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            // Prefer the X-RateLimit-Remaining header over body text for
            // rate-limit detection — the header is authoritative, but when
            // absent we fall back to body text matching as a heuristic.
            let is_rate_limit = rate_limit_remaining == Some("0")
                || (rate_limit_remaining.is_none() && body.contains("rate limit"));
            let msg = if is_rate_limit {
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
        let err = classify_error(StatusCode::UNAUTHORIZED, "bad credentials", None);
        assert!(err.to_string().contains("authentication failed"));

        let err = classify_error(StatusCode::NOT_FOUND, "not found", None);
        assert!(err.to_string().contains("not found"));

        let err = classify_error(StatusCode::TOO_MANY_REQUESTS, "", None);
        assert!(err.to_string().contains("transient"));

        // When X-RateLimit-Remaining: 0, it's a rate limit even with a body
        let err = classify_error(StatusCode::FORBIDDEN, "", Some("0"));
        assert!(err.to_string().contains("rate limit"));

        // When X-RateLimit-Remaining is not 0, it's an auth failure
        let err = classify_error(StatusCode::FORBIDDEN, "bad credentials", Some("5"));
        assert!(err.to_string().contains("authentication failed"));

        // Without the header, fall back to body text (backward compat)
        let err = classify_error(StatusCode::FORBIDDEN, "rate limit exceeded", None);
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
