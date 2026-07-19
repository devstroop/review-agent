//! The review orchestrator: ties together the GitHub client, diff parser,
//! token manager, and AI client to produce and post a PR review.

use crate::ai::AiClient;
use crate::config::Settings;
use crate::diff::{DiffFile, DiffStatus, filter_files, format_diff_context, parse_diff};
use crate::error::Result;
use crate::github::{GitHub, PullRequest, ReviewEvent};
use crate::language::detect_language;
use crate::tokens::{estimate_tokens, truncate_to_budget};
use std::time::Instant;
use tracing::{info, warn};

/// System prompt loaded at compile time.
const SYSTEM_PROMPT: &str = include_str!("../../prompts/review_system.txt");

/// User prompt template loaded at compile time.
const USER_TEMPLATE: &str = include_str!("../../prompts/review_user.txt");

/// Orchestrates the full review pipeline for a single pull request.
pub struct ReviewTool {
    github: GitHub,
    ai: AiClient,
    settings: Settings,
}

/// Summary of a completed review, suitable for step-summary output.
#[derive(Debug, Clone)]
pub struct ReviewOutput {
    pub pr_number: u64,
    pub pr_title: String,
    pub files_changed: usize,
    pub files_reviewed: usize,
    pub files_skipped: usize,
    pub total_tokens_used: usize,
    pub input_tokens_estimated: usize,
    pub output_tokens_reported: Option<u32>,
    pub latency_ms: u64,
}

impl ReviewTool {
    /// Construct a new review tool from settings.
    pub fn new(settings: &Settings) -> Result<Self> {
        let github = GitHub::new(settings)?;
        let ai = AiClient::new(settings)?;
        Ok(Self {
            github,
            ai,
            settings: settings.clone(),
        })
    }

    /// Run the full review for a pull request and post the result.
    pub async fn run(&self, owner: &str, repo: &str, pr_number: u64) -> Result<ReviewOutput> {
        let start = Instant::now();

        // 1. Fetch PR metadata.
        let pr: PullRequest = self.github.get_pr_metadata(owner, repo, pr_number).await?;

        // 2. Fetch the raw unified diff.
        let raw_diff = self.github.get_pr_diff(owner, repo, pr_number).await?;

        // 3. Parse the diff into structured files.
        let parsed = parse_diff(&raw_diff)?;
        let files_changed = parsed.len();

        // 4. Filter out skippable and binary files.
        let filtered = filter_files(parsed, self.settings.review.max_diff_files);
        let files_skipped = files_changed.saturating_sub(filtered.len());

        // 5. Truncate to token budget (largest files dropped first).
        let mut budgeted = filtered;
        let dropped = truncate_to_budget(&mut budgeted, self.settings.review.max_input_tokens);
        let files_reviewed = budgeted.len();
        if dropped > 0 {
            warn!(
                dropped,
                max_tokens = self.settings.review.max_input_tokens,
                "Truncated diff — some files excluded from review"
            );
        }

        // 6. Build the user prompt.
        let user_prompt = self.build_user_prompt(owner, repo, &pr, &budgeted)?;
        let input_tokens_estimated = estimate_tokens(&user_prompt);

        // 7. Call the AI.
        let review_text = self.ai.chat(SYSTEM_PROMPT, &user_prompt).await?;

        // 8. Post the review as a comment.
        let review = self
            .github
            .publish_review(
                owner,
                repo,
                pr_number,
                &review_text,
                Some(ReviewEvent::Comment),
            )
            .await?;
        let _ = review; // id / state available if needed for logging

        let latency_ms = start.elapsed().as_millis() as u64;

        info!(
            pr_number,
            files_changed, files_reviewed, input_tokens_estimated, latency_ms, "Review complete"
        );

        Ok(ReviewOutput {
            pr_number,
            pr_title: pr.title.clone(),
            files_changed,
            files_reviewed,
            files_skipped,
            total_tokens_used: input_tokens_estimated,
            input_tokens_estimated,
            output_tokens_reported: None,
            latency_ms,
        })
    }

    /// Fill the user prompt template with PR metadata and the diff context.
    fn build_user_prompt(
        &self,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
        files: &[DiffFile],
    ) -> Result<String> {
        let language = self.detect_primary_language(files);
        let file_list = self.format_file_list(files);
        let diff_context = format_diff_context(files, usize::MAX);

        let description = pr.body.clone().unwrap_or_default();
        let author = &pr.user.login;
        let branch = &pr.head.r#ref;
        let base = &pr.base.r#ref;

        let prompt = USER_TEMPLATE
            .replace("{title}", &pr.title)
            .replace("{owner}", owner)
            .replace("{repo}", repo)
            .replace("{author}", author)
            .replace("{branch}", branch)
            .replace("{base}", base)
            .replace("{language}", &language)
            .replace("{description}", &description)
            .replace("{total_files}", &files.len().to_string())
            .replace("{file_list}", &file_list)
            .replace("{diff}", &diff_context);

        Ok(prompt)
    }

    /// Determine the primary language from the changed files (most common
    /// detected language, fallback to "Unknown").
    fn detect_primary_language(&self, files: &[DiffFile]) -> String {
        use std::collections::HashMap;

        let mut counts: HashMap<String, usize> = HashMap::new();
        for f in files {
            let lang = detect_language(&f.filename).to_string();
            *counts.entry(lang).or_insert(0) += 1;
        }

        counts
            .into_iter()
            .max_by_key(|(_, c)| *c)
            .map(|(lang, _)| lang)
            .unwrap_or_else(|| "Unknown".to_string())
    }

    /// Format a simple bullet list of changed files with their status.
    fn format_file_list(&self, files: &[DiffFile]) -> String {
        let mut out = String::new();
        for f in files {
            let status = match f.status {
                DiffStatus::Added => "added",
                DiffStatus::Modified => "modified",
                DiffStatus::Deleted => "deleted",
                DiffStatus::Renamed => "renamed",
                DiffStatus::Copied => "copied",
            };
            out.push_str(&format!(
                "- `{}` ({} — +{} −{})\n",
                f.filename, status, f.additions, f.deletions
            ));
        }
        if out.is_empty() {
            out.push_str("- (no reviewable files)");
        }
        out
    }
}
