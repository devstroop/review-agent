//! MCP tool definitions and handler functions.

use crate::engine::{ReviewEngine, ReviewOptions, ReviewRequest, ReviewSource};
use crate::github::parse_pr_url;
use serde::Deserialize;
use serde_json::Value;

// ── Input structs for each tool ───────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ReviewPrArgs {
    pub pr_url: String,
    #[serde(default = "default_post")]
    pub post: bool,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub extra_instructions: String,
}

fn default_post() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReviewDiffArgs {
    pub diff: String,
    pub title: String,
    #[serde(default = "default_language")]
    pub language: String,
    pub description: Option<String>,
    #[serde(default)]
    pub extra_instructions: String,
}

fn default_language() -> String {
    "Unknown".into()
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReviewFilesArgs {
    pub pr_url: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub post: bool,
    #[serde(default)]
    pub extra_instructions: String,
}

// ── Tool definitions ──────────────────────────────────────────

/// All tools that this server exposes.
pub(crate) fn tool_definitions() -> Vec<(String, String, Value)> {
    vec![
        (
            "review_pr".into(),
            "Review a GitHub pull request. Fetches the diff, analyzes it with the AI, and optionally posts the review as a comment on the PR.".into(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pr_url": { "type": "string", "description": "Full GitHub PR URL, e.g. https://github.com/owner/repo/pull/42" },
                    "post": { "type": "boolean", "description": "Whether to post the review to GitHub as a PR comment", "default": true },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Only review files matching these path prefixes", "default": [] },
                    "extra_instructions": { "type": "string", "description": "Extra context injected into the review prompt", "default": "" }
                },
                "required": ["pr_url"]
            }),
        ),
        (
            "review_diff".into(),
            "Review a raw unified diff string without fetching from GitHub. Use for ad-hoc code review, pre-fetched diffs, or working-tree changes.".into(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "diff": { "type": "string", "description": "Raw unified diff text (git diff output)" },
                    "title": { "type": "string", "description": "A short title describing the change" },
                    "language": { "type": "string", "description": "Primary language hint (e.g. 'Rust', 'Python')", "default": "Unknown" },
                    "description": { "type": "string", "description": "Optional longer description of the change" },
                    "extra_instructions": { "type": "string", "description": "Extra context injected into the review prompt", "default": "" }
                },
                "required": ["diff", "title"]
            }),
        ),
        (
            "review_files".into(),
            "Review only specific files from a GitHub pull request. Useful when the AI wants to focus on relevant changes after inspecting the PR's file list.".into(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pr_url": { "type": "string", "description": "Full GitHub PR URL, e.g. https://github.com/owner/repo/pull/42" },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Only review files matching these path prefixes" },
                    "post": { "type": "boolean", "description": "Whether to post the review to GitHub", "default": false },
                    "extra_instructions": { "type": "string", "description": "Extra context injected into the review prompt", "default": "" }
                },
                "required": ["pr_url", "paths"]
            }),
        ),
    ]
}

// ── Handler functions ─────────────────────────────────────────

/// Parse a `pr_url` string into owner/repo/number.
///
/// Accepts `https://github.com/owner/repo/pull/42` or `www.github.com` variant.
/// Handle the `review_pr` tool.
pub(crate) async fn handle_review_pr(
    engine: &ReviewEngine,
    params: Value,
) -> std::result::Result<Value, String> {
    let args: ReviewPrArgs =
        serde_json::from_value(params).map_err(|e| format!("Invalid arguments: {e}"))?;

    let (owner, repo, number) = parse_pr_url(&args.pr_url)?;

    let request = ReviewRequest {
        source: ReviewSource::PrUrl {
            owner,
            repo,
            number,
        },
        options: ReviewOptions {
            post_to_github: args.post,
            paths: args.paths,
            extra_instructions: args.extra_instructions,
        },
    };

    let result = engine
        .review(request)
        .await
        .map_err(|e| format!("Review failed: {e}"))?;

    serde_json::to_value(&result).map_err(|e| format!("Failed to serialize result: {e}"))
}

/// Handle the `review_diff` tool.
pub(crate) async fn handle_review_diff(
    engine: &ReviewEngine,
    params: Value,
) -> std::result::Result<Value, String> {
    let args: ReviewDiffArgs =
        serde_json::from_value(params).map_err(|e| format!("Invalid arguments: {e}"))?;

    let request = ReviewRequest {
        source: ReviewSource::DiffText {
            diff: args.diff,
            title: args.title,
            language_hint: args.language,
            description: args.description,
        },
        options: ReviewOptions {
            post_to_github: false,
            paths: Vec::new(),
            extra_instructions: args.extra_instructions,
        },
    };

    let result = engine
        .review(request)
        .await
        .map_err(|e| format!("Review failed: {e}"))?;

    serde_json::to_value(&result).map_err(|e| format!("Failed to serialize result: {e}"))
}

/// Handle the `review_files` tool.
pub(crate) async fn handle_review_files(
    engine: &ReviewEngine,
    params: Value,
) -> std::result::Result<Value, String> {
    let args: ReviewFilesArgs =
        serde_json::from_value(params).map_err(|e| format!("Invalid arguments: {e}"))?;

    if args.paths.is_empty() {
        return Err("'paths' must be a non-empty array of path prefixes".into());
    }

    let (owner, repo, number) = parse_pr_url(&args.pr_url)?;

    let request = ReviewRequest {
        source: ReviewSource::PrUrl {
            owner,
            repo,
            number,
        },
        options: ReviewOptions {
            post_to_github: args.post,
            paths: args.paths,
            extra_instructions: args.extra_instructions,
        },
    };

    let result = engine
        .review(request)
        .await
        .map_err(|e| format!("Review failed: {e}"))?;

    serde_json::to_value(&result).map_err(|e| format!("Failed to serialize result: {e}"))
}
