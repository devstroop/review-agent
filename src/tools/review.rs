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
    /// Total tokens consumed (input + output), or `None` if the API did not
    /// report output token usage.
    pub total_tokens_used: Option<usize>,
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
        let chat_output = self.ai.chat(SYSTEM_PROMPT, &user_prompt).await?;
        let review_text = sanitize_output(&chat_output.content);
        let output_tokens_reported = chat_output.usage.as_ref().and_then(|u| u.completion_tokens);

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
            total_tokens_used: output_tokens_reported
                .map(|t| input_tokens_estimated + t as usize),
            input_tokens_estimated,
            output_tokens_reported,
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

        let extra = if self.settings.review.extra_instructions.is_empty() {
            String::new()
        } else {
            format!(
                "### Additional Instructions\n\n{}\n",
                self.settings.review.extra_instructions
            )
        };

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
            .replace("{extra_instructions}", &extra)
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

/// Sanitize AI output before posting to GitHub.
///
/// Strips potential prompt-injection vectors from the model response:
/// - GitHub Actions workflow command syntax (`::command params::value`)
/// - ANSI escape sequences
///
/// Maximum number of lines inside a single fenced code block before we
/// force-close it as a safety net against unmatched opening fences.
const MAX_CODE_BLOCK_LINES: usize = 500;

/// Sanitize AI output before posting to GitHub.
///
/// Strips potential prompt-injection vectors from the model response:
/// - GitHub Actions workflow command syntax (`::command params::value`)
/// - ANSI escape sequences
///
/// Fenced code blocks (```` ``` ```` and `~~~`) are preserved verbatim —
/// content inside them is not sanitized, so code examples showing terminal
/// output or workflow commands are left intact.
///
/// As a safety net against unmatched opening fences (which would cause all
/// subsequent content to bypass sanitization), code blocks are force-closed
/// after {MAX_CODE_BLOCK_LINES} lines.  This is not a silver bullet — an
/// attacker could inject multiple short code blocks — but it limits the
/// blast radius.
///
/// This is a defense-in-depth measure — the review is posted as a PR
/// comment via the API, not written to the Actions runner's stdout, so
/// workflow commands are not directly interpreted.  However, sanitizing
/// at the application layer protects against unforeseen data flows.
fn sanitize_output(text: &str) -> String {
    use std::fmt::Write;

    let mut result = String::with_capacity(text.len());

    // Track the active code block fence (None = outside a block).
    // When inside a block, stores the fence delimiter string (e.g. "```")
    // so we can emit a matching closing fence if needed.
    let mut fence: Option<String> = None;
    // Line count inside the current code block (used for forced close).
    let mut code_block_lines: usize = 0;

    for line in text.lines() {
        // ── Fenced code block detection ──────────────────────────
        if let Some((ch, count)) = parse_fence(line) {
            match fence {
                None => {
                    // Start a new code block.
                    let delim: String = (0..count).map(|_| ch).collect();
                    fence = Some(delim);
                    code_block_lines = 0;
                    let _ = writeln!(result, "{line}");
                    continue;
                }
                Some(ref open_delim) => {
                    let expected_count = open_delim.len();
                    let actual_count = count;
                    let is_close = ch == open_delim.chars().next().unwrap()
                        && actual_count >= expected_count;
                    if is_close {
                        fence = None; // close the block
                    } else {
                        // False fence inside a code block (e.g. ~~~~
                        // inside a ``` block) — count toward the
                        // force-close limit so an attacker cannot
                        // inject fake fences to bypass the cap.
                        code_block_lines += 1;
                        if code_block_lines > MAX_CODE_BLOCK_LINES {
                            // This line is the last inside the block —
                            // preserve it verbatim, then emit a closing
                            // fence so the Markdown stays well-formed
                            // and subsequent content is sanitized.
                            let _ = writeln!(result, "{line}");
                            let _ = writeln!(result, "{open_delim}");
                            fence = None;
                            continue;
                        }
                    }
                    let _ = writeln!(result, "{line}");
                    continue;
                }
            }
        }

        // ── Content ─────────────────────────────────────────────
        if let Some(ref delim) = fence {
            code_block_lines += 1;
            if code_block_lines > MAX_CODE_BLOCK_LINES {
                // Safety cap: force-close the code block to avoid leaving
                // all subsequent content unsanitized due to an unmatched
                // opening fence.  This line is the last inside the block —
                // preserve it verbatim, then emit a matching closing fence
                // so the Markdown stays well-formed.
                let _ = writeln!(result, "{line}");
                let _ = writeln!(result, "{delim}");
                fence = None;
                continue;
            } else {
                // Inside a code block: preserve verbatim.
                let _ = writeln!(result, "{line}");
                continue;
            }
        }

        // Outside code blocks (or just force-closed): strip GitHub Actions
        // workflow commands and ANSI escape sequences.
        let cleaned = strip_workflow_commands(line);
        let cleaned = strip_ansi_escapes(&cleaned);
        let _ = writeln!(result, "{cleaned}");
    }

    // If the output ends with an open code block (e.g. truncated by
    // max_completion_tokens), close it so the rendered Markdown is valid.
    if let Some(ref delim) = fence {
        let _ = writeln!(result, "{delim}");
    }

    result
}

/// Check whether a line is a Markdown fenced code block delimiter.
///
/// Returns `Some((char, count))` if the line starts (after optional
/// whitespace) with 3 or more backticks or tildes, where `char` is
/// the delimiter character and `count` is the number of repetitions.
/// Also accepts info strings after the fence (e.g. ` ```rust `).
///
/// This is intentionally permissive — it errs on the side of treating
/// a line as a fence, because treating a non-fence as a fence only
/// toggles code-block state (and content inside is preserved verbatim,
/// which is safe).  Treating a fence as non-fence is the dangerous
/// case (workflow commands would be incorrectly stripped).
fn parse_fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars().peekable();

    // Determine the fence character.
    let (ch, count) = match chars.peek() {
        Some('`') => ('`', trimmed.bytes().take_while(|&b| b == b'`').count()),
        Some('~') => ('~', trimmed.bytes().take_while(|&b| b == b'~').count()),
        _ => return None,
    };

    if count < 3 {
        return None;
    }

    // After the fence delimiters, only whitespace or an info string
    // is allowed.  Both backtick and tilde fences accept info strings
    // with or without leading whitespace (e.g. `` `rust``` and `` ` rust```
    // are both valid).
    let rest = &trimmed[count..];
    let trimmed_rest = rest.trim();
    if trimmed_rest.is_empty() {
        // Pure fence (trailing whitespace only) — valid delimiter.
        Some((ch, count))
    } else if rest.starts_with(' ') || rest.starts_with('\t') {
        // Info string after whitespace — valid opening fence.
        Some((ch, count))
    } else {
        // Info string directly after fence delimiters (e.g. ````rust````) —
        // accepted per common usage, even though the CommonMark spec is
        // stricter for backtick fences.  This matches how most Markdown
        // renderers (including GitHub's) behave.
        Some((ch, count))
    }
}

/// Remove GitHub Actions workflow command syntax from a string.
///
/// Pattern: `::command-name param,param::value` — matches any occurrence
/// in the line.  Once a valid `::...::` construct is found, everything
/// from the opening `::` to end of line is removed, because GitHub Actions
/// interprets the value as extending to end of line.
///
/// **Known limitation:** Heredoc-style commands
/// (`::set-output name=x<<EOF...EOF`) are not detected — they span
/// multiple lines and would require buffering state across calls.
/// This is acceptable because review output is posted via the API as
/// a PR comment, not written to the Actions runner's stdout, so these
/// commands are never interpreted by the runner.
///
/// **Trade-off:** Text after `::...::` on the same line is also removed.
/// This could affect legitimate prose that mentions workflow commands
/// (e.g. `"Consider using ::set-output::x"`).  Since the `::word::`
/// pattern is distinctively GitHub Actions command syntax, the security
/// benefit of full removal outweighs the low probability of false
/// positives in review text.
///
/// See https://docs.github.com/en/actions/using-workflows/workflow-commands-for-github-actions
fn strip_workflow_commands(line: &str) -> String {
    let mut result = String::with_capacity(line.len());

    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ':' && chars.peek() == Some(&':') {
            chars.next(); // consume second ':'
            // Scan ahead for closing ::
            let mut ahead = chars.clone();
            let mut found = false;
            while let Some(c) = ahead.next() {
                if c == ':' && ahead.peek() == Some(&':') {
                    found = true;
                    break;
                }
            }
            if found {
                // Valid ::...:: construct — advance chars past closing ::
                loop {
                    match chars.next() {
                        Some(':') if chars.peek() == Some(&':') => {
                            chars.next(); // consume second ':'
                            break;
                        }
                        Some(_) => continue,
                        None => break,
                    }
                }
                // Skip the command value and everything after it to end of
                // line per the GitHub Actions workflow command spec.
                for c in &mut chars {
                    if c == '\n' {
                        break;
                    }
                }
            } else {
                // Not a valid command — emit the two colons
                result.push_str("::");
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Remove ANSI escape sequences from a string using the `strip-ansi-escapes` crate.
///
/// Handles CSI (`\x1b[`), OSC (`\x1b]`), DCS (`\x1bP`), and other
/// standard ANSI escape sequence introducers.
fn strip_ansi_escapes(s: &str) -> String {
    let stripped = strip_ansi_escapes::strip(s);
    String::from_utf8_lossy(&stripped).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_workflow_command_at_start() {
        let input = "Some review text\n::set-output name=foo::bar\nMore text";
        let result = sanitize_output(input);
        assert!(result.contains("Some review text"));
        assert!(result.contains("More text"));
        assert!(!result.contains("::set-output"));
        assert!(!result.contains("::"));
    }

    #[test]
    fn sanitize_removes_mid_line_workflow_command() {
        // Everything from ::command:: to end of line is stripped.
        let input = r##"echo "::warning::something happened"##;
        let result = sanitize_output(input);
        assert!(!result.contains("::warning::"), "got: {result}");
        assert!(!result.contains("something"), "got: {result}");
        assert!(!result.contains(" happened"), "got: {result}");
        assert!(result.trim() == "echo \"", "got: {result}");
    }

    #[test]
    fn sanitize_preserves_normal_markdown() {
        let input = "## Review\n\nThis is a **normal** review with `inline code`.";
        let result = sanitize_output(input);
        assert_eq!(result.trim(), input);
    }

    #[test]
    fn sanitize_preserves_code_blocks_verbatim() {
        let input = "before\n```\n::set-output name=foo::bar\n\x1b[31mred\x1b[0m\n```\nafter";
        let result = sanitize_output(input);
        assert!(result.contains("before"));
        assert!(result.contains("after"));
        assert!(result.contains("::set-output"));
        assert!(result.contains("\x1b[31mred\x1b[0m"), "ANSI escapes inside code blocks should be preserved");
    }

    #[test]
    fn sanitize_strips_ansi_outside_code_blocks() {
        let input = "normal text\x1b[31mred\x1b[0mend";
        let result = sanitize_output(input);
        assert!(result.contains("normal text"));
        assert!(result.contains("red"));
        assert!(result.contains("end"));
        assert!(!result.contains("\x1b[31m"));
        assert!(!result.contains("\x1b[0m"));
    }

    #[test]
    fn sanitize_preserves_tilde_fenced_blocks() {
        let input = "outer\n~~~\n::warning::beep\n~~~\nouter";
        let result = sanitize_output(input);
        assert!(result.contains("::warning::"));
        assert!(result.contains("~~~"));
    }

    #[test]
    fn sanitize_empty_string() {
        assert_eq!(sanitize_output("").trim(), "");
    }

    #[test]
    fn sanitize_no_false_positive_on_colons() {
        let input = "::this is not a command (no double colons after)";
        let result = sanitize_output(input);
        assert!(result.contains("::this"));
    }

    #[test]
    fn strip_workflow_commands_mid_line() {
        // Everything from :: to EOL is stripped.
        let input = "some text ::warning::message here";
        let result = strip_workflow_commands(input);
        assert_eq!(result, "some text ");
    }

    #[test]
    fn strip_workflow_commands_none() {
        let input = "normal text with colons: like: this";
        let result = strip_workflow_commands(input);
        assert_eq!(result, input);
    }

    #[test]
    fn strip_workflow_commands_multiple() {
        // First ::cmd1:: strips rest of line.
        let input = "a ::cmd1::v1 b ::cmd2::v2 c";
        let result = strip_workflow_commands(input);
        assert_eq!(result, "a ");
    }

    // ── parse_fence ─────────────────────────────────────────────

    #[test]
    fn parse_fence_three_backticks() {
        assert_eq!(parse_fence("```"), Some(('`', 3)));
        assert_eq!(parse_fence("   ```"), Some(('`', 3)));
    }

    #[test]
    fn parse_fence_four_tildes() {
        assert_eq!(parse_fence("~~~~"), Some(('~', 4)));
    }

    #[test]
    fn parse_fence_with_info_string() {
        // Backtick fences accept info strings with or without leading space.
        assert_eq!(parse_fence("``` rust"), Some(('`', 3)));
        assert_eq!(parse_fence("```rust"), Some(('`', 3)));
        assert_eq!(parse_fence("``` ruby"), Some(('`', 3)));
        // Tilde fences accept info string without leading space (CommonMark).
        assert_eq!(parse_fence("~~~bash"), Some(('~', 3)));
        assert_eq!(parse_fence("~~~ python"), Some(('~', 3)));
    }

    #[test]
    fn parse_fence_backtick_info_without_space_accepted() {
        // Backtick fences accept info string without leading space.
        assert_eq!(parse_fence("```rust"), Some(('`', 3)));
    }

    #[test]
    fn parse_fence_less_than_three_rejected() {
        assert_eq!(parse_fence("``"), None);
        assert_eq!(parse_fence("~~"), None);
        assert_eq!(parse_fence("`"), None);
    }

    #[test]
    fn parse_fence_inline_code_not_a_fence() {
        // Single backtick inline code should not be treated as a fence.
        assert_eq!(parse_fence("`code`"), None);
        assert_eq!(parse_fence("``code``"), None);
    }

    #[test]
    fn parse_fence_text_after_no_space_accepted() {
        // Info string directly after fence delimiters is accepted.
        assert_eq!(parse_fence("```x"), Some(('`', 3)));
    }

    // ── Fence matching in sanitize_output ────────────────────────

    #[test]
    fn sanitize_mismatched_fences_rejected() {
        // Open with ```, try to close with ~~~~ — should not close.
        let input = "before\n```\n::warning::x\n~~~~\nafter";
        let result = sanitize_output(input);
        // The block stays open, so ::warning:: is preserved inside.
        assert!(result.contains("::warning::"));
        assert!(result.contains("~~~~"));
    }

    #[test]
    fn sanitize_inline_backticks_not_fences() {
        // Inline code spans should not toggle code block state.
        let input = "text with `inline code` and ::warning::x";
        let result = sanitize_output(input);
        // ::warning::x should be stripped since we're outside a block.
        assert!(!result.contains("::warning::"));
    }

    #[test]
    fn sanitize_force_closes_unmatched_fence() {
        // An unmatched opening fence is force-closed after MAX_CODE_BLOCK_LINES
        // lines, so a subsequent ::warning:: is stripped.
        let mut lines = vec!["```".to_string()];
        for i in 0..MAX_CODE_BLOCK_LINES + 2 {
            lines.push(format!("line {i}"));
        }
        lines.push("::warning::should be stripped".to_string());
        let input = lines.join("\n");
        let result = sanitize_output(&input);
        // The force-close emits a closing ```, so we see two ``` fences.
        assert!(result.matches("```").count() == 2, "got: {result}");
        // The ::warning:: line is force-closed and gets sanitized.
        assert!(!result.contains("::warning::"), "got: {result}");
    }

    #[test]
    fn sanitize_closes_fence_at_end_of_input() {
        // If the input ends inside a code block, a closing fence is emitted.
        let input = "before\n```\nunclosed code block";
        let result = sanitize_output(input);
        assert!(result.matches("```").count() == 2, "got: {result}");
        assert!(result.contains("unclosed code block"));
    }

    #[test]
    fn sanitize_false_fences_count_toward_limit() {
        // Non-matching fence lines inside a code block count toward the
        // MAX_CODE_BLOCK_LINES limit.  After the limit is exceeded and the
        // block is force-closed, a subsequent line that is NOT a fence
        // should be sanitized.
        //
        // We construct a backtick block with 499 normal lines + 2 fence-like
        // lines (~~~~) that don't close the block.  The fence-like lines
        // now count toward the limit, so the block force-closes after the
        // second one (499 + 2 > 500).  After that, a non-fence line with
        // a workflow command should be sanitized.
        let mut lines = vec!["```".to_string()];
        for i in 0..MAX_CODE_BLOCK_LINES - 1 {
            lines.push(format!("normal line {i}"));
        }
        // These two count toward the limit but don't close the block:
        lines.push("~~~~ fake fence 1".to_string());
        lines.push("~~~~ fake fence 2".to_string());
        // This line is outside the block (force-closed) and should be sanitized:
        lines.push("::warning::should be stripped".to_string());
        let input = lines.join("\n");
        let result = sanitize_output(&input);
        // The ::warning:: line should be sanitized because the block was
        // force-closed.
        assert!(!result.contains("::warning::"), "got: {result}");
    }
}
