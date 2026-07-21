//! Webhook server for processing GitHub PR events in real time.
//!
//! Listens for GitHub webhook `pull_request` events and triggers a review
//! for `opened`, `synchronize`, `reopened`, and `ready_for_review` actions.
//! Draft PRs and bot senders are skipped (matching the Action entrypoint
//! behaviour).

use crate::config::Settings;
use crate::error::Result;
use crate::tools::review::ReviewTool;
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{info, warn};

/// Shared application state for the webhook handler.
struct AppState {
    settings: Settings,
}

/// Helper: return a JSON response with a given status code.
fn json_response(status: StatusCode, value: serde_json::Value) -> impl IntoResponse {
    (status, Json(value))
}

/// GitHub webhook payload for `pull_request` events (minimal subset).
///
/// We only deserialise the fields needed to construct a PR URL and decide
/// whether to skip the event.  All other fields are ignored.
#[derive(Debug, Deserialize)]
struct WebhookPayload {
    action: String,
    #[serde(rename = "pull_request")]
    pull_request: Option<PrObject>,
    sender: Option<Sender>,
    repository: Option<Repository>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PrObject {
    number: u64,
    title: Option<String>,
    draft: Option<bool>,
    html_url: Option<String>,
    head: Option<PrBranch>,
    base: Option<PrBranch>,
    user: Option<Sender>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PrBranch {
    r#ref: String,
    sha: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Sender {
    login: String,
    #[serde(rename = "type")]
    user_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Repository {
    full_name: Option<String>,
    owner: Option<Owner>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Owner {
    login: String,
}

/// Build and return the webhook router.
pub fn router(settings: Settings) -> Router {
    let state = Arc::new(AppState { settings });

    Router::new()
        .route("/webhook", post(handle_webhook))
        .with_state(state)
}

/// Handle an incoming GitHub webhook POST.
async fn handle_webhook(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<WebhookPayload>,
) -> impl IntoResponse {
    info!(action = %payload.action, "Received webhook event");

    // ── Event filter ────────────────────────────────────────────
    let allowed = ["opened", "synchronize", "reopened", "ready_for_review"];
    if !allowed.contains(&payload.action.as_str()) {
        info!(action = %payload.action, "Ignoring non-review event");
        return json_response(StatusCode::OK, serde_json::json!({ "status": "ignored" }));
    }

    // ── Skip bots ───────────────────────────────────────────────
    if let Some(ref sender) = payload.sender {
        if sender.user_type.as_deref() == Some("Bot") {
            info!("Ignoring bot-triggered event");
            return json_response(StatusCode::OK, serde_json::json!({ "status": "ignored" }));
        }
    }

    // ── Skip drafts ─────────────────────────────────────────────
    let pr = match payload.pull_request {
        Some(ref pr) => pr,
        None => {
            warn!("Webhook payload missing pull_request object");
            return json_response(StatusCode::OK, serde_json::json!({ "status": "ignored" }));
        }
    };

    if pr.draft.unwrap_or(false) {
        info!("Ignoring draft PR");
        return json_response(StatusCode::OK, serde_json::json!({ "status": "ignored" }));
    }

    // ── Extract owner/repo ──────────────────────────────────────
    let (owner, repo) = match extract_owner_repo(&payload) {
        Some((o, r)) => (o, r),
        None => {
            warn!("Could not determine owner/repo from payload");
            return json_response(
                StatusCode::BAD_REQUEST,
                serde_json::json!({
                    "status": "error",
                    "error": "could not determine owner/repo"
                }),
            );
        }
    };

    let pr_number = pr.number;

    info!(
        owner = %owner,
        repo = %repo,
        pr_number = %pr_number,
        "Processing review"
    );

    // ── Run the review (fire-and-forget with immediate ACK) ─────
    let settings = state.settings.clone();
    tokio::spawn(async move {
        if let Err(e) = run_review(&settings, &owner, &repo, pr_number).await {
            warn!(
                error = %e,
                owner = %owner,
                repo = %repo,
                pr_number = %pr_number,
                "Review failed"
            );
        }
    });

    json_response(
        StatusCode::ACCEPTED,
        serde_json::json!({
            "status": "accepted",
            "pr_number": pr_number,
        }),
    )
}

/// Run the full review pipeline for a pull request.
#[allow(deprecated)]
async fn run_review(
    settings: &Settings,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> Result<crate::tools::review::ReviewOutput> {
    let tool = ReviewTool::new(settings)?;
    let output = tool.run(owner, repo, pr_number).await?;
    info!(
        pr_number,
        files_reviewed = output.files_reviewed,
        latency_ms = output.latency_ms,
        "Review posted successfully"
    );
    Ok(output)
}

/// Extract owner and repo from either `repository.owner.login` /
/// `repository.name` or from `repository.full_name`.
fn extract_owner_repo(payload: &WebhookPayload) -> Option<(String, String)> {
    if let Some(ref repo) = payload.repository {
        // Try owner.login + name first.
        if let (Some(owner), Some(name)) = (repo.owner.as_ref(), repo.name.as_ref()) {
            return Some((owner.login.clone(), name.clone()));
        }
        // Fall back to full_name (e.g. "owner/repo").
        if let Some(ref full) = repo.full_name {
            if let Some((o, r)) = full.split_once('/') {
                return Some((o.to_string(), r.to_string()));
            }
        }
    }
    // Last resort: extract from pull_request.html_url.
    if let Some(ref pr) = payload.pull_request {
        if let Some(ref url) = pr.html_url {
            // https://github.com/owner/repo/pull/N
            let parts: Vec<&str> = url.split('/').collect();
            if parts.len() >= 5 {
                return Some((parts[3].to_string(), parts[4].to_string()));
            }
        }
    }
    None
}
