//! Core review engine: single `ReviewRequest → ReviewResult` contract
//! shared by all four IO surfaces (CLI, Action, webhook, MCP).
//!
//! Callers build a [`ReviewRequest`], call [`ReviewEngine::review()`],
//! and receive a [`ReviewResult`].  No caller touches parsing, prompting,
//! or GitHub internals.

use crate::ai::AiClient;
use crate::config::Settings;
use crate::error::Result;
use crate::services::{DiffService, GithubService, PromptBuilder, PromptContext};
use crate::tokens::estimate_tokens;
use serde::Serialize;
use std::time::Instant;
use tracing::info;

/// Baseline token overhead for prompt template text that is not part of the
/// diff context: headers, metadata fields, markdown formatting, diff fence
/// wrapping, and the file-list header.
const PROMPT_OVERHEAD_BASELINE: usize = 100;

/// Estimated tokens per file in the file-list section (the bullet point with
/// filename, status, and line counts).  Each entry is roughly 50-70 bytes.
const PROMPT_OVERHEAD_PER_FILE: usize = 20;

// ── Resolved source (private) ──────────────────────────────────

/// Strongly-typed result of resolving a `ReviewSource` into concrete data.
struct ResolvedSource {
    pr_number: Option<u64>,
    pr_title: Option<String>,
    description: Option<String>,
    raw_diff: String,
    owner: Option<String>,
    repo: Option<String>,
    author: Option<String>,
    branch: Option<String>,
    base: Option<String>,
    /// Language hint from the caller (e.g. MCP DiffText source).
    /// Used as a fallback when file-extension detection yields "Unknown".
    language_hint: Option<String>,
}

/// What to review and where it comes from.
#[derive(Clone)]
pub enum ReviewSource {
    /// Fetch from GitHub and post back.
    PrUrl {
        owner: String,
        repo: String,
        number: u64,
    },
    /// Review a raw diff string directly, without any GitHub interaction.
    DiffText {
        diff: String,
        title: String,
        language_hint: String,
        description: Option<String>,
    },
}

/// Behaviour flags for a single review invocation.
#[derive(Clone)]
pub struct ReviewOptions {
    /// Whether to post the review to GitHub (only meaningful for `PrUrl` sources).
    pub post_to_github: bool,
    /// If non-empty, only review files whose path starts with one of these prefixes.
    pub paths: Vec<String>,
    /// Extra instructions injected into the user prompt.
    pub extra_instructions: String,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            post_to_github: true,
            paths: Vec::new(),
            extra_instructions: String::new(),
        }
    }
}

/// Everything needed to run a single review.
#[derive(Clone)]
pub struct ReviewRequest {
    pub source: ReviewSource,
    pub options: ReviewOptions,
}

// ── Result types ───────────────────────────────────────────────

/// The complete result of a review invocation.
#[derive(Clone, Serialize)]
pub struct ReviewResult {
    pub review_text: String,
    pub pr_number: Option<u64>,
    pub pr_title: Option<String>,
    pub stats: ReviewStats,
}

/// Statistics collected during a review run.
///
/// Note: `files_changed = files_skipped + files_path_filtered +
/// files_budget_dropped + files_reviewed`.  All four counters
/// are reported so callers can reconstruct total counts.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewStats {
    pub files_changed: usize,
    pub files_reviewed: usize,
    /// Files removed by the built-in skip-list OR the max_diff_files cap.
    /// Both filters are applied together inside `filter_files`, so this
    /// single counter covers both.  Path-filtered and budget-dropped
    /// files are tracked separately in `files_path_filtered` and
    /// `files_budget_dropped`.
    pub files_skipped: usize,
    /// Files removed by the caller-supplied `paths` filter.
    pub files_path_filtered: usize,
    /// Files dropped by token-budget truncation (largest files first).
    pub files_budget_dropped: usize,
    pub input_tokens_estimated: usize,
    /// Estimated system-prompt tokens (heuristic, same 3.5 chars/token).
    pub system_tokens_estimated: usize,
    pub output_tokens_reported: Option<u32>,
    /// Sum of system + input + output token estimates / reported values.
    ///
    /// Formula: `system_tokens_estimated + input_tokens_estimated + output_tokens_reported`.
    ///
    /// **Note for callers migrating from `ReviewOutput`:** the deprecated
    /// `ReviewOutput.total_tokens_used` omits system tokens (input + output
    /// only) to preserve legacy semantics.  `ReviewResult.stats.total_tokens_used`
    /// is the canonical field and includes all three segments.  If you need
    /// the old calculation for continuity, compute
    /// `input_tokens_estimated + output_tokens_reported.unwrap_or(0)`.
    pub total_tokens_used: Option<usize>,
    pub latency_ms: u64,
}

// ── Engine ─────────────────────────────────────────────────────

/// Core review engine.
///
/// Owns the services needed to fetch, parse, prompt, and analyse a PR diff.
/// Every IO surface adapts through this single `ReviewRequest → ReviewResult`
/// contract.
pub struct ReviewEngine {
    github_svc: GithubService,
    diff_svc: DiffService,
    prompt_builder: PromptBuilder,
    ai: AiClient,
    /// Extra instructions from settings (merged into every user prompt).
    config_extra: String,
}

impl ReviewEngine {
    /// Construct a new engine from application settings.
    pub fn new(settings: &Settings) -> Result<Self> {
        Ok(Self {
            github_svc: GithubService::new(settings)?,
            diff_svc: DiffService::new(settings),
            prompt_builder: PromptBuilder::new(settings),
            ai: AiClient::new(settings)?,
            config_extra: settings.review.extra_instructions.clone(),
        })
    }

    /// Run the full review pipeline.
    ///
    /// The flow depends on `ReviewSource`:
    /// - `PrUrl`: fetch metadata + diff from GitHub → parse → filter → path-filter → budget → prompt → AI → optionally post
    /// - `DiffText`: use the provided diff directly → parse → filter → path-filter → budget → prompt → AI
    pub async fn review(&self, request: ReviewRequest) -> Result<ReviewResult> {
        let start = Instant::now();
        let post_to_github = request.options.post_to_github;
        let path_filter = request.options.paths;

        // Merge settings-level extra_instructions with request-level ones.
        let extra = if self.config_extra.is_empty() {
            request.options.extra_instructions.trim().to_string()
        } else if request.options.extra_instructions.is_empty() {
            self.config_extra.trim().to_string()
        } else {
            format!(
                "{}\n\n{}",
                self.config_extra.trim(),
                request.options.extra_instructions.trim()
            )
        };

        // ── 1. Resolve source ───────────────────────────────────
        let resolved = match request.source {
            ReviewSource::PrUrl {
                owner,
                repo,
                number,
            } => {
                let o = owner.clone();
                let r = repo.clone();
                let pr = self.github_svc.get_pr_metadata(&o, &r, number).await?;
                let diff = self.github_svc.get_pr_diff(&o, &r, number).await?;
                ResolvedSource {
                    pr_number: Some(number),
                    pr_title: Some(pr.title),
                    description: pr.body,
                    raw_diff: diff,
                    owner: Some(owner),
                    repo: Some(repo),
                    author: pr.user.as_ref().map(|u| u.login.clone()),
                    branch: Some(pr.head.r#ref),
                    base: Some(pr.base.r#ref),
                    language_hint: None,
                }
            }
            ReviewSource::DiffText {
                diff,
                title,
                language_hint,
                description,
            } => ResolvedSource {
                pr_number: None,
                pr_title: Some(title),
                description,
                raw_diff: diff,
                owner: None,
                repo: None,
                author: None,
                branch: None,
                base: None,
                language_hint: Some(language_hint),
            },
        };

        // ── 2. Parse + filter skippable files ───────────────────
        let (files_changed, filtered) = self.diff_svc.parse_and_filter(&resolved.raw_diff)?;
        // files_skipped includes files removed by the skip-list AND files
        // truncated by the max_diff_files cap (both happen inside
        // filter_files).  Path-filtered and budget-dropped files are
        // tracked separately in files_path_filtered and
        // files_budget_dropped below.
        let files_skipped = files_changed.saturating_sub(filtered.len());

        // ── 3. Apply path filter (before budget truncation) ─────
        // Matches any filename that starts with the given prefix using a
        // simple `starts_with` check.  Empty prefixes are skipped.
        //
        // Note: this is a prefix match, NOT a directory-boundary match.
        // A filter of "src" will match "src/main.rs" AND "src-extra/file.rs"
        // alike.  Callers who want directory-level granularity should include
        // a trailing slash (e.g. "src/") to get exact directory matching.
        // This is more permissive than a dir-boundary check and intentionally
        // so — callers can always narrow by adding separators to their prefix.
        let pre_path = filtered.len();
        let after_path_filter: Vec<_> = if path_filter.is_empty() {
            filtered
        } else {
            filtered
                .into_iter()
                .filter(|f| {
                    path_filter
                        .iter()
                        .any(|p| !p.is_empty() && f.filename.starts_with(p))
                })
                .collect()
        };
        let files_path_filtered = pre_path.saturating_sub(after_path_filter.len());

        // ── 4. Truncate to token budget ─────────────────────────
        // Reserve budget for prompt overhead (template text, file list,
        // extra instructions) so that the effective diff budget is lower.
        // We estimate overhead on the pre-truncation file count, which
        // is conservative — after truncation the overhead will be smaller
        // (fewer files), leaving more room than budgeted.  That's fine:
        // undershooting the budget is safe; overshooting would fail the
        // AI call.
        let mut budgeted = after_path_filter;
        let system_overhead = PromptBuilder::system_prompt_tokens();
        let overhead = PROMPT_OVERHEAD_BASELINE
            + PROMPT_OVERHEAD_PER_FILE * budgeted.len()
            + system_overhead
            + if extra.is_empty() {
                0
            } else {
                estimate_tokens(&extra) + 10
            }; // 10 for the header
        let effective_budget = self.diff_svc.max_tokens().saturating_sub(overhead);
        let files_budget_dropped = self
            .diff_svc
            .truncate_to_budget(&mut budgeted, effective_budget);
        if files_budget_dropped > 0 {
            tracing::warn!(
                files_budget_dropped,
                max_tokens = self.diff_svc.max_tokens(),
                effective_budget,
                "Truncated diff — some files excluded from review"
            );
        }
        let files_reviewed = budgeted.len();

        // Warn if the effective diff budget was too small to keep any
        // reviewable content — the AI will receive a minimal prompt.
        if budgeted.is_empty() {
            tracing::warn!(
                max_tokens = self.diff_svc.max_tokens(),
                effective_budget,
                "Token budget too small for any diff content — AI prompt will be minimal"
            );
        }

        // ── 5. Build prompts ───────────────────────────────────
        let system = PromptBuilder::system_prompt();
        let title = resolved.pr_title.as_deref().unwrap_or("(untitled)");
        let description = resolved.description.as_deref().unwrap_or("");
        let owner = resolved.owner.as_deref().unwrap_or("");
        let repo = resolved.repo.as_deref().unwrap_or("");
        let author = resolved.author.as_deref().unwrap_or("");
        let branch = resolved.branch.as_deref().unwrap_or("");
        let base = resolved.base.as_deref().unwrap_or("");
        let user = self.prompt_builder.user_prompt(
            &PromptContext {
                title,
                description,
                owner,
                repo,
                author,
                branch,
                base,
                language_hint: resolved.language_hint.as_deref(),
            },
            &budgeted,
            &extra,
        );
        let input_tokens_estimated = crate::tokens::estimate_tokens(&user);
        let system_tokens_estimated = crate::tokens::estimate_tokens(&system);

        // ── 6. AI analysis ─────────────────────────────────────
        let chat_output = self.ai.chat(&system, &user).await?;
        let sanitized = crate::services::sanitize_output(&chat_output.content);
        let output_tokens_reported = chat_output.usage.as_ref().and_then(|u| u.completion_tokens);

        // ── 7. Format output ────────────────────────────────────
        // `review_text` always contains the raw sanitised Markdown.
        let review_text = sanitized.clone();

        // ── 8. Post (optional, always Markdown) ─────────────────
        if post_to_github {
            if let (Some(owner), Some(repo)) = (resolved.owner.as_ref(), resolved.repo.as_ref()) {
                if let Some(number) = resolved.pr_number {
                    self.github_svc
                        .post_review(owner, repo, number, &review_text)
                        .await?;
                } else {
                    tracing::warn!(
                        "post_to_github is true but review source lacks a PR number — no review posted"
                    );
                }
            } else {
                tracing::warn!(
                    "post_to_github is true but review source is not a GitHub PR — no review posted"
                );
            }
        }

        let latency_ms = start.elapsed().as_millis() as u64;

        info!(
            pr_number = ?resolved.pr_number,
            files_changed,
            files_reviewed,
            input_tokens_estimated,
            latency_ms,
            "Review complete"
        );

        let total_tokens_used = output_tokens_reported
            .map(|t| system_tokens_estimated + input_tokens_estimated + t as usize);

        Ok(ReviewResult {
            review_text,
            pr_number: resolved.pr_number,
            pr_title: resolved.pr_title,
            stats: ReviewStats {
                files_changed,
                files_reviewed,
                files_skipped,
                files_path_filtered,
                files_budget_dropped,
                input_tokens_estimated,
                system_tokens_estimated,
                output_tokens_reported,
                total_tokens_used,
                latency_ms,
            },
        })
    }
}
