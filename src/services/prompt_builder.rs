//! Fills the system and user prompt templates with context about
//! the PR and diff.  Only `ReviewEngine` calls this service.

use crate::config::Settings;
use crate::diff::{DiffFile, DiffStatus, format_diff_context};
use crate::language::detect_language;
use crate::tokens::estimate_tokens;

/// System prompt loaded at compile time.
const SYSTEM_PROMPT: &str = include_str!("../../prompts/review_system.txt");

/// User prompt template loaded at compile time.
const USER_TEMPLATE: &str = include_str!("../../prompts/review_user.txt");

/// All PR metadata fields used to fill the user prompt template.
pub(crate) struct PromptContext<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub author: &'a str,
    pub branch: &'a str,
    pub base: &'a str,
    /// Optional language hint from the caller (DiffText source).
    /// Used as a fallback when file-extension detection yields "Unknown".
    pub language_hint: Option<&'a str>,
}

/// Builds the system and user prompts for the AI.
pub(crate) struct PromptBuilder;

impl PromptBuilder {
    pub(crate) fn new(_settings: &Settings) -> Self {
        Self
    }

    /// Return the compiled system prompt (constant, no dynamic content).
    pub(crate) fn system_prompt() -> String {
        SYSTEM_PROMPT.to_string()
    }

    /// Return the estimated token count of the (constant) system prompt.
    /// Used by the engine to reserve budget before truncating the diff.
    pub(crate) fn system_prompt_tokens() -> usize {
        estimate_tokens(SYSTEM_PROMPT)
    }

    /// Fill the user prompt template with PR metadata and diff context.
    pub(crate) fn user_prompt(
        &self,
        ctx: &PromptContext<'_>,
        files: &[DiffFile],
        extra: &str,
    ) -> String {
        let detected = self.detect_primary_language(files);
        let language = if detected != "Unknown" {
            detected
        } else if let Some(hint) = ctx.language_hint {
            hint.to_string()
        } else {
            "Unknown".to_string()
        };
        let file_list = self.format_file_list(files);
        let diff_context = format_diff_context(files, usize::MAX);
        let total_files = files.len();

        let extra_section = if extra.is_empty() {
            String::new()
        } else {
            format!("### Additional Instructions\n\n{}\n", extra)
        };

        USER_TEMPLATE
            .replace("{title}", ctx.title)
            .replace("{owner}", ctx.owner)
            .replace("{repo}", ctx.repo)
            .replace("{author}", ctx.author)
            .replace("{branch}", ctx.branch)
            .replace("{base}", ctx.base)
            .replace("{language}", &language)
            .replace("{description}", ctx.description)
            .replace("{total_files}", &total_files.to_string())
            .replace("{file_list}", &file_list)
            .replace("{extra_instructions}", &extra_section)
            .replace("{diff}", &diff_context)
    }

    /// Determine the primary language from the changed files.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffFile, DiffStatus, Hunk};

    fn make_file(name: &str, status: DiffStatus) -> DiffFile {
        DiffFile {
            filename: name.to_string(),
            old_filename: None,
            status,
            hunks: vec![Hunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![],
            }],
            additions: 5,
            deletions: 3,
            mode_change: None,
        }
    }

    #[test]
    fn detect_primary_language_finds_most_common() {
        let builder = PromptBuilder;
        let files = vec![
            make_file("a.rs", DiffStatus::Modified),
            make_file("b.py", DiffStatus::Added),
            make_file("c.rs", DiffStatus::Modified),
            make_file("d.js", DiffStatus::Added),
        ];
        let result = builder.detect_primary_language(&files);
        assert_eq!(result, "Rust");
    }

    #[test]
    fn detect_primary_language_empty_returns_unknown() {
        let builder = PromptBuilder;
        assert_eq!(builder.detect_primary_language(&[]), "Unknown");
    }

    #[test]
    fn detect_primary_language_tie_returns_first_max() {
        let builder = PromptBuilder;
        let files = vec![
            make_file("a.rs", DiffStatus::Modified),
            make_file("b.py", DiffStatus::Modified),
        ];
        // Both have count 1; HashMap iteration order is arbitrary,
        // so we just check it returns one of them.
        let result = builder.detect_primary_language(&files);
        assert!(result == "Rust" || result == "Python", "got {result}");
    }

    #[test]
    fn format_file_list_basic() {
        let builder = PromptBuilder;
        let files = vec![
            make_file("src/main.rs", DiffStatus::Modified),
            make_file("src/lib.rs", DiffStatus::Added),
        ];
        let result = builder.format_file_list(&files);
        assert!(result.contains("src/main.rs"));
        assert!(result.contains("src/lib.rs"));
        assert!(result.contains("+5"));
        assert!(result.contains("−3"));
        assert!(result.contains("modified"));
        assert!(result.contains("added"));
    }

    #[test]
    fn format_file_list_empty() {
        let builder = PromptBuilder;
        assert_eq!(builder.format_file_list(&[]), "- (no reviewable files)");
    }

    #[test]
    fn user_prompt_language_fallback_from_hint() {
        let builder = PromptBuilder;
        let ctx = PromptContext {
            title: "test",
            description: "",
            owner: "",
            repo: "",
            author: "",
            branch: "",
            base: "",
            language_hint: Some("Zig"),
        };
        // Empty file list → file-extension detection returns "Unknown",
        // so the hint should activate.
        let result = builder.user_prompt(&ctx, &[], "");
        assert!(result.contains("Zig"));
    }

    #[test]
    fn user_prompt_language_hint_ignored_when_detected() {
        let builder = PromptBuilder;
        let ctx = PromptContext {
            title: "test",
            description: "",
            owner: "",
            repo: "",
            author: "",
            branch: "",
            base: "",
            language_hint: Some("Python"),
        };
        let files = vec![make_file("a.rs", DiffStatus::Modified)];
        let result = builder.user_prompt(&ctx, &files, "");
        // Extension detection should find Rust, overriding the hint.
        assert!(result.contains("Rust"));
        assert!(!result.contains("Python"));
    }
}
