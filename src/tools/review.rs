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

        // Number of files after filtering (before token-budget truncation).
        let filtered_count = filtered.len();

        // 5. First pass: reduce the diff to roughly fit the token budget.
        //    We use `max_input_tokens` as a generous cap (no overhead
        //    subtracted yet); exact overhead is computed below.
        let mut budgeted = filtered;
        let dropped = truncate_to_budget(&mut budgeted, self.settings.review.max_input_tokens);
        if dropped > 0 {
            warn!(
                dropped,
                max_tokens = self.settings.review.max_input_tokens,
                "Truncated diff — some files excluded from review"
            );
        }

        // 6. Build the user prompt and compute accurate totals.
        let mut user_prompt = self.build_user_prompt(owner, repo, &pr, &budgeted)?;
        let mut input_tokens_estimated = estimate_tokens(&user_prompt);
        let mut total_estimated = input_tokens_estimated + estimate_tokens(SYSTEM_PROMPT);

        // If still over budget, drop the largest file iteratively until
        // the prompt fits (or only one file remains).
        while total_estimated > self.settings.review.max_input_tokens && budgeted.len() > 1 {
            // Find the file contributing the most diff tokens.
            let largest_idx = budgeted
                .iter()
                .map(|f| estimate_tokens(&format_diff_context(std::slice::from_ref(f), usize::MAX)))
                .enumerate()
                .max_by_key(|(_, t)| *t)
                .map(|(i, _)| i)
                .unwrap();
            budgeted.remove(largest_idx);

            user_prompt = self.build_user_prompt(owner, repo, &pr, &budgeted)?;
            input_tokens_estimated = estimate_tokens(&user_prompt);
            total_estimated = input_tokens_estimated + estimate_tokens(SYSTEM_PROMPT);
        }

        if total_estimated > self.settings.review.max_input_tokens {
            warn!(
                total_estimated,
                budget = self.settings.review.max_input_tokens,
                "Final prompt still exceeds token budget — review may be incomplete"
            );
        }

        let files_reviewed = budgeted.len();
        let files_skipped =
            files_changed.saturating_sub(filtered_count) + (filtered_count - budgeted.len());

        // 7. Call the AI.
        let ai_output = self.ai.chat(SYSTEM_PROMPT, &user_prompt).await?;
        let review_text = ai_output.content;
        let output_tokens_reported = ai_output.usage.and_then(|u| u.completion_tokens);

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
        info!(
            review_id = review.id,
            review_url = ?review.html_url,
            "Review posted"
        );

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
            input_tokens_estimated,
            output_tokens_reported,
            latency_ms,
        })
    }

    /// Fill the user prompt template with PR metadata and the diff context.
    ///
    /// Uses single-pass replacement so substituted values are never re-scanned
    /// for further placeholders.
    fn build_user_prompt(
        &self,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
        files: &[DiffFile],
    ) -> Result<String> {
        let diff_context = format_diff_context(files, usize::MAX);
        self.build_user_prompt_with_diff(owner, repo, pr, files, &diff_context)
    }

    /// Like `build_user_prompt` but accepts an explicit diff context string
    /// instead of computing it from `files`.  Used internally to compute the
    /// prompt overhead with an empty diff before the final prompt is built.
    fn build_user_prompt_with_diff(
        &self,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
        files: &[DiffFile],
        diff_context: &str,
    ) -> Result<String> {
        let language = self.detect_primary_language(files);
        let file_list = self.format_file_list(files);
        let description = pr.body.clone().unwrap_or_default();
        let total_files = files.len().to_string();

        let prompt = render_template(
            USER_TEMPLATE,
            &[
                ("title", &pr.title),
                ("owner", owner),
                ("repo", repo),
                ("author", &pr.user.login),
                ("branch", &pr.head.r#ref),
                ("base", &pr.base.r#ref),
                ("language", &language),
                ("description", &description),
                ("total_files", &total_files),
                ("file_list", &file_list),
                ("diff", diff_context),
            ],
        );
        Ok(prompt)
    }
}

/// Single-pass template replacement that never re-scans substituted values.
/// Each `{key}` in `template` is replaced with the corresponding value from
/// `replacements`. Unknown keys are kept as-is in the output.
fn render_template(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(end) = rest.find('}') {
            let key = &rest[1..end];
            if let Some((_, value)) = replacements.iter().find(|(k, _)| *k == key) {
                out.push_str(value);
            } else {
                // Unknown key — keep literal text including braces
                out.push_str(&rest[..=end]);
            }
            rest = &rest[end + 1..];
        } else {
            // No closing brace — keep rest as-is
            out.push_str(rest);
            break;
        }
    }
    if !rest.is_empty() {
        out.push_str(rest);
    }
    out
}

impl ReviewTool {
    /// Determine the primary language from the changed files (most common
    /// detected language, fallback to "Unknown").
    fn detect_primary_language(&self, files: &[DiffFile]) -> String {
        use std::collections::HashMap;

        let mut counts: HashMap<String, usize> = HashMap::new();
        for f in files {
            let lang = detect_language(&f.filename).to_string();
            if !lang.trim().is_empty() {
                *counts.entry(lang).or_insert(0) += 1;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    fn test_settings() -> Settings {
        let mut s = Settings::default();
        s.github.token = crate::sensitive::Sensitive::new("test-token".to_string());
        s.ai.api_key = crate::sensitive::Sensitive::new("test-key".to_string());
        s
    }

    #[test]
    fn primary_language_detection() {
        let tool = ReviewTool::new(&test_settings()).unwrap();
        let files = vec![
            DiffFile {
                filename: "src/a.rs".into(),
                old_filename: None,
                mode_change: None,
                status: DiffStatus::Added,
                hunks: vec![],
                additions: 1,
                deletions: 0,
            },
            DiffFile {
                filename: "src/b.rs".into(),
                old_filename: None,
                mode_change: None,
                status: DiffStatus::Modified,
                hunks: vec![],
                additions: 1,
                deletions: 0,
            },
            DiffFile {
                filename: "README.md".into(),
                old_filename: None,
                mode_change: None,
                status: DiffStatus::Modified,
                hunks: vec![],
                additions: 1,
                deletions: 0,
            },
        ];
        assert_eq!(tool.detect_primary_language(&files), "Rust");
    }

    #[test]
    fn file_list_formatting() {
        let tool = ReviewTool::new(&test_settings()).unwrap();
        let files = vec![DiffFile {
            filename: "src/main.rs".into(),
            old_filename: None,
            mode_change: None,
            status: DiffStatus::Added,
            hunks: vec![],
            additions: 10,
            deletions: 2,
        }];
        let list = tool.format_file_list(&files);
        assert!(list.contains("`src/main.rs`"));
        assert!(list.contains("added"));
        assert!(list.contains("+10"));
        assert!(list.contains("−2"));
    }

    #[test]
    fn file_list_empty() {
        let tool = ReviewTool::new(&test_settings()).unwrap();
        let list = tool.format_file_list(&[]);
        assert!(list.contains("no reviewable files"));
    }

    #[test]
    fn render_template_basic() {
        let result = render_template(
            "Hello {name}! Age: {age}",
            &[("name", "Alice"), ("age", "30")],
        );
        assert_eq!(result, "Hello Alice! Age: 30");
    }

    #[test]
    fn render_template_unknown_key_preserved() {
        let result = render_template("X:{x} Y:{y}", &[("x", "1")]);
        assert_eq!(result, "X:1 Y:{y}");
    }

    #[test]
    fn render_template_no_placeholder() {
        let result = render_template("static text", &[("k", "v")]);
        assert_eq!(result, "static text");
    }

    #[test]
    fn render_template_value_contains_placeholder() {
        // Single-pass ensures a value containing "{language}" is never
        // re-scanned for further substitutions.
        let result = render_template("Lang: {lang}", &[("lang", "{language}")]);
        assert_eq!(result, "Lang: {language}");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::config::Settings;
    use crate::sensitive::Sensitive;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn integration_settings(github_base: String, ai_base: String) -> Settings {
        let mut s = Settings::default();
        s.github.token = Sensitive::new("test-token".to_string());
        s.github.base_url = github_base;
        s.ai.api_key = Sensitive::new("test-key".to_string());
        s.ai.api_base = ai_base;
        s.review.max_input_tokens = 100_000; // don't truncate in tests
        s
    }

    const PR_METADATA: &str = r#"{
        "number": 42,
        "title": "Add new feature",
        "body": "This PR adds a cool feature",
        "html_url": "https://github.com/devstroop/review-agent/pull/42",
        "state": "open",
        "user": {"login": "contributor", "type": "User"},
        "head": {"label": "devstroop:feature", "ref": "feature", "sha": "abc123"},
        "base": {"label": "devstroop:master", "ref": "master", "sha": "def456"}
    }"#;

    const RAW_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
-    println!(\"old\");
+    let x = 1;
+    println!(\"new {}\", x);
 }";

    const AI_RESPONSE: &str = "{\n  \"id\": \"chatcmpl-test\",\n  \"choices\": [{\"message\": {\"content\": \"## Review Summary\\n\\nLooks good overall.\"}, \"finish_reason\": \"stop\"}],\n  \"usage\": {\"prompt_tokens\": 100, \"completion_tokens\": 20, \"total_tokens\": 120}\n}";

    const REVIEW_POST_RESPONSE: &str = r#"{
        "id": 999,
        "state": "COMMENTED",
        "html_url": "https://github.com/devstroop/review-agent/pull/42#pullrequestreview-999"
    }"#;

    #[tokio::test]
    async fn full_review_flow_posts_comment() {
        let gh_server = MockServer::start().await;
        let ai_server = MockServer::start().await;

        // GitHub: PR metadata
        Mock::given(method("GET"))
            .and(path("/repos/devstroop/review-agent/pulls/42"))
            .and(header("Accept", "application/vnd.github.v3+json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PR_METADATA))
            .mount(&gh_server)
            .await;

        // GitHub: raw diff (Accept: v3.diff)
        Mock::given(method("GET"))
            .and(path("/repos/devstroop/review-agent/pulls/42"))
            .and(header("Accept", "application/vnd.github.v3.diff"))
            .respond_with(ResponseTemplate::new(200).set_body_string(RAW_DIFF))
            .mount(&gh_server)
            .await;

        // GitHub: post review
        Mock::given(method("POST"))
            .and(path("/repos/devstroop/review-agent/pulls/42/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_string(REVIEW_POST_RESPONSE))
            .mount(&gh_server)
            .await;

        // AI: chat completion
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(AI_RESPONSE))
            .mount(&ai_server)
            .await;

        let settings = integration_settings(gh_server.uri(), ai_server.uri());
        let tool = ReviewTool::new(&settings).unwrap();

        let output = tool.run("devstroop", "review-agent", 42).await.unwrap();

        assert_eq!(output.pr_number, 42);
        assert_eq!(output.pr_title, "Add new feature");
        assert_eq!(output.files_changed, 1);
        assert_eq!(output.files_reviewed, 1);
        assert_eq!(output.files_skipped, 0);
        assert!(output.input_tokens_estimated > 0);
        assert!(output.latency_ms > 0); // latency recorded
    }

    #[tokio::test]
    async fn empty_diff_produces_valid_output() {
        let gh_server = MockServer::start().await;
        let ai_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/devstroop/review-agent/pulls/7"))
            .and(header("Accept", "application/vnd.github.v3+json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PR_METADATA))
            .mount(&gh_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/devstroop/review-agent/pulls/7"))
            .and(header("Accept", "application/vnd.github.v3.diff"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&gh_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/repos/devstroop/review-agent/pulls/7/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_string(REVIEW_POST_RESPONSE))
            .mount(&gh_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(AI_RESPONSE))
            .mount(&ai_server)
            .await;

        let settings = integration_settings(gh_server.uri(), ai_server.uri());
        let tool = ReviewTool::new(&settings).unwrap();

        let output = tool.run("devstroop", "review-agent", 7).await.unwrap();
        // The diff mock returns "" → parse_diff("") → vec![] → files_changed=0
        assert_eq!(output.files_changed, 0);
        assert_eq!(output.files_reviewed, 0);
        assert!(output.input_tokens_estimated > 0); // prompt still has template text
    }
}
