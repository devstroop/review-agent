//! Backward-compatible `ReviewTool` that delegates to `ReviewEngine`.
//!
//! New surfaces (MCP) should use `ReviewEngine` directly.  This module
//! exists so that CLI (`main.rs`), Docker/Action, and the webhook server
//! continue to work without changes.

use crate::config::Settings;
use crate::engine::{ReviewEngine, ReviewOptions, ReviewRequest, ReviewSource};
use crate::error::Result;
use std::time::Instant;

/// Summary of a completed review, suitable for step-summary output.
///
/// ⚠️ **Deprecated** — use [`ReviewResult`](crate::engine::ReviewResult) instead.
/// This type exists only for backward compatibility with existing callers
/// (CLI, webhook server, integration tests). New code should use
/// `review_agent::engine::ReviewResult` directly.
///
/// **Token-counting note:** `total_tokens_used` here uses the original
/// formula (`input_tokens_estimated + output_tokens_reported`) to preserve
/// continuity.  `ReviewResult.stats.total_tokens_used` additionally includes
/// the system-prompt estimate, so the two values will differ by roughly
/// the system-prompt token count.  See [`ReviewStats`] for the canonical
/// definition.
#[derive(Debug, Clone)]
#[deprecated(since = "0.1.0", note = "use engine::ReviewResult instead")]
pub struct ReviewOutput {
    pub pr_number: u64,
    pub pr_title: String,
    pub files_changed: usize,
    pub files_reviewed: usize,
    pub files_skipped: usize,
    /// Estimated total tokens used (input + output only).
    ///
    /// ⚠️ This omits system-prompt tokens (unlike
    /// [`ReviewStats::total_tokens_used`]), preserving the original
    /// pre-refactor formula for continuity.  See the struct-level doc
    /// for details.
    pub total_tokens_used: Option<usize>,
    pub input_tokens_estimated: usize,
    pub output_tokens_reported: Option<u32>,
    pub latency_ms: u64,
}

/// Backward-compatible orchestrator — delegates to `ReviewEngine`.
pub struct ReviewTool {
    engine: ReviewEngine,
}

impl ReviewTool {
    pub fn new(settings: &Settings) -> Result<Self> {
        let engine = ReviewEngine::new(settings)?;
        Ok(Self { engine })
    }

    /// Run the full review for a pull request and post the result.
    #[allow(deprecated)]
    pub async fn run(&self, owner: &str, repo: &str, pr_number: u64) -> Result<ReviewOutput> {
        let start = Instant::now();

        let request = ReviewRequest {
            source: ReviewSource::PrUrl {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number: pr_number,
            },
            options: ReviewOptions {
                post_to_github: true,
                // Engine already reads extra_instructions from config_extra.
                // Passing it again would cause duplication in the merge.
                extra_instructions: String::new(),
                ..Default::default()
            },
        };

        let result = self.engine.review(request).await?;
        let latency_ms = start.elapsed().as_millis() as u64;

        Ok(ReviewOutput {
            pr_number,
            pr_title: result.pr_title.unwrap_or_default(),
            files_changed: result.stats.files_changed,
            files_reviewed: result.stats.files_reviewed,
            files_skipped: result.stats.files_skipped,
            // Preserve original total_tokens_used semantics (input + output
            // only, no system prompt estimate) for backward compatibility.
            total_tokens_used: result
                .stats
                .output_tokens_reported
                .map(|t| result.stats.input_tokens_estimated + t as usize),
            input_tokens_estimated: result.stats.input_tokens_estimated,
            output_tokens_reported: result.stats.output_tokens_reported,
            latency_ms,
        })
    }
}
