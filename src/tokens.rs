//! Token estimation and budget enforcement for diff context.
//!
//! v1 uses a heuristic: 3.5 characters per token (ADR-006). We implement this
//! as pure integer math `(len * 2) / 7` to avoid float casting and precision
//! quirks. Because multi-byte UTF-8 characters are 2–4 bytes, this slightly
//! overestimates token counts — a conservative safety margin that prevents us
//! from exceeding the model's context window.

use crate::diff::DiffFile;
use tracing::warn;

/// Estimate the number of tokens in a text string.
///
/// Uses the heuristic of 3.5 chars/token expressed as integer math:
/// `(byte_len * 2) / 7`. Multi-byte characters count as multiple bytes, so
/// they're correctly treated as multiple tokens.
pub fn estimate_tokens(text: &str) -> usize {
    (text.len() * 2) / 7
}

/// Drop files from `files` (largest first) until the total estimated token count
/// fits within `max_tokens`. Returns the number of files dropped.
///
/// Truncation works at the file level, never mid-hunk — a partial file review
/// provides misleading signal, so we review fewer complete files instead.
///
/// **Caveat (ADR-023):** The largest file may be the core PR change. Dropping it
/// means the AI might miss the most important part of a very large PR. This is
/// an accepted v1 trade-off in favor of keeping many small files reviewable.
pub fn truncate_to_budget(files: &mut Vec<DiffFile>, max_tokens: usize) -> usize {
    if files.is_empty() {
        return 0;
    }

    // Compute per-file token counts.
    let mut indexed: Vec<(usize, usize)> = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let formatted = crate::diff::format_diff_context(std::slice::from_ref(f), usize::MAX);
            (i, estimate_tokens(&formatted))
        })
        .collect();

    let total: usize = indexed.iter().map(|(_, t)| *t).sum();
    if total <= max_tokens {
        return 0;
    }

    // Drop largest files first until under budget. Always keep at least one
    // file so the prompt is never empty — a single oversized file is kept
    // rather than leaving the AI with no context.
    indexed.sort_by_key(|(_, t)| std::cmp::Reverse(*t));

    let mut to_drop: Vec<usize> = Vec::new();
    let mut running_total = total;

    for (i, (idx, tokens)) in indexed.into_iter().enumerate() {
        // Stop once we're under budget.
        if running_total <= max_tokens {
            break;
        }
        // Never drop the last remaining file; keep it so the prompt isn't empty.
        if i == files.len() - 1 {
            break;
        }
        to_drop.push(idx);
        running_total -= tokens;
        warn!(
            tokens = tokens,
            "Dropping file — exceeded token budget ({})", max_tokens
        );
    }

    // Remove in descending index order to keep earlier indices valid.
    to_drop.sort_unstable_by(|a, b| b.cmp(a));
    let dropped_count = to_drop.len();
    for idx in to_drop {
        files.remove(idx);
    }

    dropped_count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffFile, DiffLine, DiffLineKind, Hunk};

    fn make_file(name: &str, additions: u64, content_size: usize) -> DiffFile {
        let content = "x".repeat(content_size);
        DiffFile {
            filename: name.to_string(),
            old_filename: None,
            status: crate::diff::DiffStatus::Modified,
            hunks: vec![Hunk {
                header: "@@ -1 +1 @@".to_string(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    content,
                    old_lineno: None,
                    new_lineno: Some(1),
                }],
            }],
            additions,
            deletions: 0,
            mode_change: None,
        }
    }

    #[test]
    fn estimate_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_short_string() {
        // "hello" = 5 bytes → (5*2)/7 = 1
        assert_eq!(estimate_tokens("hello"), 1);
        // 7 bytes → (7*2)/7 = 2
        assert_eq!(estimate_tokens("1234567"), 2);
    }

    #[test]
    fn estimate_multibyte() {
        // 4 CJK chars = 12 bytes → (12*2)/7 = 3
        assert_eq!(estimate_tokens("你好世界"), 3);
    }

    #[test]
    fn truncate_single_over_budget_keeps_one() {
        // Even when the only file exceeds the budget, we keep it so the prompt
        // is never empty.
        let mut files = vec![make_file("big.rs", 100, 10_000)];
        let dropped = truncate_to_budget(&mut files, 100);
        assert_eq!(dropped, 0);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn truncate_keeps_one_when_all_over_budget() {
        let mut files = vec![
            make_file("a.rs", 100, 10_000),
            make_file("b.rs", 100, 10_000),
            make_file("c.rs", 100, 10_000),
        ];
        let dropped = truncate_to_budget(&mut files, 100);
        assert_eq!(dropped, 2);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn truncate_largest_first() {
        let mut files = vec![
            make_file("small.rs", 10, 100),
            make_file("huge.rs", 500, 50_000),
            make_file("medium.rs", 50, 5_000),
        ];
        // Budget fits small + medium but not huge.
        let dropped = truncate_to_budget(&mut files, 2_000);
        assert_eq!(dropped, 1);
        assert_eq!(files.len(), 2);
        // The huge file (largest) should be the one dropped.
        assert!(!files.iter().any(|f| f.filename == "huge.rs"));
    }

    #[test]
    fn truncate_none_when_budget_fits() {
        let mut files = vec![make_file("a.rs", 10, 100), make_file("b.rs", 10, 100)];
        let dropped = truncate_to_budget(&mut files, 1_000_000);
        assert_eq!(dropped, 0);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn truncate_removes_correct_indices() {
        // Files ordered so that the two to-drop are at indices 0 and 2 (not
        // contiguous). A naive removal in to_drop order (0 then 2) would shift
        // index 2 → 1 after removing 0, deleting the wrong file. Descending
        // index sort (2 then 0) avoids this.
        let mut files = vec![
            make_file("drop_first.rs", 100, 20_000), // large, index 0
            make_file("keep.rs", 10, 100),           // small, index 1
            make_file("drop_third.rs", 100, 20_000), // large, index 2
        ];
        // Budget so small that both large files must drop.
        let dropped = truncate_to_budget(&mut files, 500);
        assert_eq!(dropped, 2);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "keep.rs");
    }
}
