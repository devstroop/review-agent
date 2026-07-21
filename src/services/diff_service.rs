//! Service for diff parsing, file filtering, and token-budget enforcement.

use crate::config::Settings;
use crate::diff::{DiffFile, filter_files, parse_diff};
use crate::error::Result;
use crate::tokens::truncate_to_budget;

/// Service that chains diff parsing → file filtering → token budgeting.
pub(crate) struct DiffService {
    max_files: usize,
    max_tokens: usize,
}

impl DiffService {
    pub(crate) fn new(settings: &Settings) -> Self {
        Self {
            max_files: settings.review.max_diff_files,
            max_tokens: settings.review.max_input_tokens,
        }
    }

    /// The configured `max_input_tokens` value (for diagnostics/logging).
    pub(crate) fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Parse raw diff text and filter skippable files.
    ///
    /// Returns `(total_file_count, reviewable_files)`.
    /// Does **not** apply token-budget truncation — callers that need
    /// truncation should apply a path filter first, then call
    /// [`truncate_to_budget`](Self::truncate_to_budget).
    pub(crate) fn parse_and_filter(&self, raw_diff: &str) -> Result<(usize, Vec<DiffFile>)> {
        let parsed = parse_diff(raw_diff)?;
        let files_changed = parsed.len();
        let filtered = filter_files(parsed, self.max_files);
        Ok((files_changed, filtered))
    }

    /// Truncate a file list to fit a token budget (drops largest first).
    ///
    /// `budget` is the maximum number of tokens the **diff content alone**
    /// may consume.  Callers are responsible for reserving room for prompt
    /// template overhead (file lists, metadata, instructions) before calling
    /// this method.
    ///
    /// Returns the number of files dropped.
    pub(crate) fn truncate_to_budget(&self, files: &mut Vec<DiffFile>, budget: usize) -> usize {
        truncate_to_budget(files, budget)
    }
}
