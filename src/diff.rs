//! Unified diff parsing, file filtering, and prompt context formatting.
//!
//! GitHub's raw diff endpoint (`Accept: application/vnd.github.v3.diff`) returns
//! a standard multi-file unified diff. We split it into per-file sections and
//! parse each with the `diffy` crate, which handles `---`/`+++` headers, `@@`
//! hunk ranges, `Binary files differ`, and `\ No newline at end of file` natively.

use crate::error::Result;
use diffy::{Line, Patch};
use tracing::warn;

/// Status of a file within a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

/// A single line within a diff hunk.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
    /// 1-based line number in the old (pre-change) file. `None` for added lines.
    pub old_lineno: Option<u32>,
    /// 1-based line number in the new (post-change) file. `None` for removed lines.
    pub new_lineno: Option<u32>,
}

/// Classification of a diff line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLineKind {
    Added,
    Removed,
    Context,
}

/// A contiguous region of changed lines in a file.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// Raw hunk header, e.g. `@@ -10,7 +10,8 @@ fn main()`.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// A file changed in a pull request, with its parsed hunks.
#[derive(Debug, Clone)]
pub struct DiffFile {
    pub filename: String,
    /// Original name when the file was renamed/copied; `None` for other changes.
    pub old_filename: Option<String>,
    pub status: DiffStatus,
    pub hunks: Vec<Hunk>,
    pub additions: u64,
    pub deletions: u64,
    /// Mode-only change metadata (e.g. `old mode 100644` / `new mode 100755`),
    /// present when a file has no `@@` hunks but carries permission/mode changes.
    pub mode_change: Option<String>,
}

/// Parse a raw unified diff string into a list of changed files.
///
/// Returns an error only if the input is non-empty but structurally invalid in a
/// way `diffy` cannot recover from. Binary files (no `patch` data) and empty
/// diffs yield an empty `Vec`.
pub fn parse_diff(diff_text: &str) -> Result<Vec<DiffFile>> {
    let trimmed = diff_text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();

    // GitHub emits `diff --git a/x b/y` between files. Split there.
    for section in split_on_diff_git(trimmed) {
        if let Some(f) = parse_single_file(section)? {
            files.push(f);
        }
    }

    Ok(files)
}

/// Split a multi-file diff on `diff --git ` boundaries, keeping the boundary
/// line as the start of each subsequent section.
///
/// Byte offsets are computed in a single O(N) pass over the input rather than
/// re-scanning from the start for every boundary (which would be O(N²) on large
/// multi-file diffs).
fn split_on_diff_git(text: &str) -> Vec<&str> {
    let mut sections = Vec::new();
    let mut indices: Vec<usize> = vec![0];

    // Collect the byte offset at which each line starts. A line starts right
    // after the previous line's terminating '\n', so we only need to scan the
    // newlines once.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            line_starts.push(i + 1);
        }
    }

    // Walk the line-start offsets; when a line begins with `diff --git ` it is
    // a section boundary (except the very first line, which is always the
    // start of the first section).
    for (li, &start) in line_starts.iter().enumerate() {
        if li == 0 {
            continue;
        }
        if text[start..].starts_with("diff --git ") {
            indices.push(start);
        }
    }

    for window in indices.windows(2) {
        sections.push(&text[window[0]..window[1]]);
    }
    if let Some(&last) = indices.last() {
        sections.push(&text[last..]);
    }

    sections
}

/// Parse a single-file diff section into a `DiffFile`, or `None` if the section
/// is a binary file marker with no parseable patch.
fn parse_single_file(section: &str) -> Result<Option<DiffFile>> {
    // Only the metadata header (before the first hunk/`---` line) carries status
    // markers. Scanning just this region prevents hunk-content substrings
    // (e.g. a comment `// rename from foo`) from misclassifying the file.
    let header = diff_header(section);
    let has_hunks = section.contains("@@ ");

    // ── Early returns for files without content hunks ───────────
    // Binary files, mode-only changes, and pure renames/copies all lack
    // `@@` hunk headers and must be handled before the diffy parser.
    //
    // Previously each path had its own inline construction of a DiffFile,
    // and when the filename could not be extracted they fell through to
    // diffy (which would also fail).  Now all three share build_simple_file,
    // which returns Ok(None) when the filename is missing — a more honest
    // outcome than silently passing garbage to diffy.

    // 1. Binary files.
    if section.contains("Binary files") && section.contains("differ") {
        return build_simple_file(section, header);
    }

    // 2. Mode-only changes (e.g. chmod +x) — no hunks.
    if !has_hunks
        && header.lines().any(|l| {
            l.starts_with("old mode")
                || l.starts_with("new file mode")
                || l.starts_with("deleted file mode")
        })
    {
        return build_simple_file(section, header);
    }

    // 3. Pure renames/copies with no content hunks.
    if !has_hunks
        && (header.contains("rename from")
            || header.contains("copy from")
            || header.contains("rename to")
            || header.contains("copy to"))
    {
        return build_simple_file(section, header);
    }

    // Remove the trailing blank line from the inter-file gap before passing to
    // diffy. Multi-file diffs separate each file with a blank line, and diffy
    // treats that extra line as an orphan hunk line, causing a "Hunk header does
    // not match hunk" error. Use strip_suffix("\n") to remove at most one
    // newline — trimming all trailing newlines would break the last hunk line.
    let section = section.strip_suffix("\n").unwrap_or(section);

    let patch = match Patch::from_str(section) {
        Ok(p) => p,
        Err(e) => {
            // A section we couldn't parse (e.g. pure rename with no hunk) is skipped.
            warn!(error = %e, "Failed to parse diff section; skipping");
            return Ok(None);
        }
    };

    // Derive the real filename. For deleted files, `modified()` is `/dev/null`,
    // so fall back to `original()`. For added files, `original()` is `/dev/null`
    // but `modified()` holds the real path — prefer `modified()` first, then
    // `original()`, and skip the `/dev/null` sentinel if it leaks through.
    let filename = patch
        .modified()
        .filter(|m| *m != "/dev/null")
        .or_else(|| patch.original().filter(|o| *o != "/dev/null"))
        .map(clean_filename)
        .unwrap_or_default();

    if filename.is_empty() {
        return Ok(None);
    }

    let status = detect_status(header, patch.original(), patch.modified());

    // For renames/copies capture the original name so the formatted output can
    // show the full `a/old b/new` path.
    let old_filename = if status == DiffStatus::Renamed || status == DiffStatus::Copied {
        extract_old_filename(header).or_else(|| {
            patch
                .original()
                .filter(|o| *o != "/dev/null")
                .map(clean_filename)
        })
    } else {
        None
    };

    let mut hunks = Vec::new();
    let mut additions = 0u64;
    let mut deletions = 0u64;

    for hunk in patch.hunks() {
        let header = format!(
            "@@ -{},{} +{},{} @@{}",
            hunk.old_range().start(),
            hunk.old_range().len(),
            hunk.new_range().start(),
            hunk.new_range().len(),
            hunk.function_context()
                .map(|c| format!(" {}", c))
                .unwrap_or_default()
        );

        let mut old_lineno = hunk.old_range().start() as u32;
        let mut new_lineno = hunk.new_range().start() as u32;
        let mut lines = Vec::new();

        for line in hunk.lines() {
            match line {
                Line::Context(s) => {
                    lines.push(DiffLine {
                        kind: DiffLineKind::Context,
                        content: s.to_string(),
                        old_lineno: Some(old_lineno),
                        new_lineno: Some(new_lineno),
                    });
                    old_lineno += 1;
                    new_lineno += 1;
                }
                Line::Delete(s) => {
                    lines.push(DiffLine {
                        kind: DiffLineKind::Removed,
                        content: s.to_string(),
                        old_lineno: Some(old_lineno),
                        new_lineno: None,
                    });
                    old_lineno += 1;
                    deletions += 1;
                }
                Line::Insert(s) => {
                    lines.push(DiffLine {
                        kind: DiffLineKind::Added,
                        content: s.to_string(),
                        old_lineno: None,
                        new_lineno: Some(new_lineno),
                    });
                    new_lineno += 1;
                    additions += 1;
                }
            }
        }

        hunks.push(Hunk { header, lines });
    }

    // Preserve mode metadata (e.g. chmod) even when the file also has content
    // hunks, so permission changes are never hidden from the AI.
    let mode_change = extract_mode_lines(header);

    Ok(Some(DiffFile {
        filename,
        old_filename,
        status,
        hunks,
        additions,
        deletions,
        mode_change,
    }))
}

/// Return the metadata header portion of a diff section — everything before the
/// first hunk (`@@`) or `--- ` line. Status markers such as `rename from`,
/// `new file mode`, etc. live in this region. Scanning only the header avoids
/// misclassifying a file whose hunk content happens to contain one of those
/// substrings (e.g. a comment reading `// rename from foo`).
fn diff_header(section: &str) -> &str {
    let mut cut = section.len();
    for (i, ch) in section.char_indices() {
        if (ch == '@' && section[i..].starts_with("@@"))
            || (ch == '-' && section[i..].starts_with("--- "))
        {
            cut = i;
            break;
        }
    }
    &section[..cut]
}

/// Determine the file change status from the diff *header* and parsed filenames.
///
/// Rename/copy are checked before mode lines so a rename (or copy) that also
/// carries a mode change is not misclassified as Added/Deleted.
fn detect_status(header: &str, original: Option<&str>, modified: Option<&str>) -> DiffStatus {
    if header.contains("rename from") {
        DiffStatus::Renamed
    } else if header.contains("copy from") {
        DiffStatus::Copied
    } else if header.contains("new file mode") {
        DiffStatus::Added
    } else if header.contains("deleted file mode") {
        DiffStatus::Deleted
    } else if original == Some("/dev/null") {
        DiffStatus::Added
    } else if modified == Some("/dev/null") {
        DiffStatus::Deleted
    } else {
        DiffStatus::Modified
    }
}

/// Extract the original name for a rename/copy from the diff section's
/// `rename from` / `copy from` lines. Returns `None` if not present.
fn extract_old_filename(header: &str) -> Option<String> {
    for line in header.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("rename from ") {
            return Some(clean_filename(rest));
        }
        if let Some(rest) = trimmed.strip_prefix("copy from ") {
            return Some(clean_filename(rest));
        }
    }
    None
}

/// Strip the `a/` or `b/` prefix GitHub adds to filenames in diff headers.
fn clean_filename(name: &str) -> String {
    let name = name.trim();
    if let Some(stripped) = name.strip_prefix("a/").or_else(|| name.strip_prefix("b/")) {
        stripped.to_string()
    } else {
        name.to_string()
    }
}

/// Extract the filename from a `diff --git a/x b/y` line.
///
/// Handles Git's quoting of filenames containing spaces:
/// `diff --git "a/my file.rs" "b/my file.rs"`.
fn extract_filename_from_diff_git(section: &str) -> Option<String> {
    section
        .lines()
        .find(|l| l.starts_with("diff --git "))
        .and_then(|l| {
            let rest = l.strip_prefix("diff --git ")?;
            // Tokenize respecting double-quotes so paths with spaces stay intact.
            let parts = tokenize_quoted(rest);
            // GitHub emits `a/path` and `b/path`; prefer `b/` (post-change).
            let b = parts.get(1).or_else(|| parts.first())?;
            Some(clean_filename(b))
        })
}

/// Extract mode metadata lines (old mode / new mode / new file mode / deleted file mode)
/// from the diff header region. Returns `None` if no mode lines are present.
fn extract_mode_lines(header: &str) -> Option<String> {
    // Use starts_with because git mode lines always begin at column 0.
    // contains would false-positive on filenames or other header lines.
    let lines: Vec<&str> = header
        .lines()
        .filter(|l| {
            l.starts_with("old mode")
                || l.starts_with("new mode")
                || l.starts_with("new file mode")
                || l.starts_with("deleted file mode")
        })
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Build a `DiffFile` for a section with no content hunks (binary, mode-only,
/// or pure rename/copy). Returns `Ok(None)` if the filename cannot be extracted.
fn build_simple_file(section: &str, header: &str) -> Result<Option<DiffFile>> {
    let filename = match extract_filename_from_diff_git(section) {
        Some(f) => f,
        None => {
            warn!(
                header_len = header.len(),
                "Could not extract filename from diff section — skipping"
            );
            return Ok(None);
        }
    };
    let status = detect_status(header, None, None);
    Ok(Some(DiffFile {
        filename,
        old_filename: extract_old_filename(header),
        status,
        hunks: Vec::new(),
        additions: 0,
        deletions: 0,
        mode_change: extract_mode_lines(header),
    }))
}

/// Split a string on spaces, treating double-quoted segments as single tokens.
fn tokenize_quoted(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in s.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ' ' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Built-in skip-list for trivial or generated files.
///
/// Patterns are matched with simple substring/prefix checks (no glob engine):
/// - Well-known lockfiles by exact basename (avoids matching `block.rs`, `flock.c`, etc.)
/// - `vendor/*`   → filename starts with "vendor/"
/// - `*.min.*`    → filename contains ".min."
/// - `node_modules/*` → filename starts with "node_modules/"
/// - `*.pb.*`     → filename contains ".pb."
/// - `CHANGELOG.md` → exact match
pub fn is_skippable(filename: &str) -> bool {
    let f = filename;

    // Match well-known lockfiles by basename. Avoids the false positives that a
    // naive "contains lock" check would produce (e.g. `block.rs`, `flock.c`)
    // while still catching lockfiles in subdirectories (e.g. `vendor/Cargo.lock`).
    const LOCKFILES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Gemfile.lock",
        "poetry.lock",
        "composer.lock",
        "go.sum",
        "mix.lock",
        "flutter_pub_get_lock.yaml",
    ];
    let basename = std::path::Path::new(f)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(f);
    if LOCKFILES.contains(&basename) {
        return true;
    }

    f.starts_with("vendor/")
        || f.contains(".min.")
        || f.starts_with("node_modules/")
        || f.contains(".pb.")
        || f == "CHANGELOG.md"
}

/// Filter a list of diff files by removing skippable files, binary files (no
/// hunks), and capping at `max_files`. Returns the filtered list.
///
/// If the number of reviewable files exceeds `max_files`, the excess is dropped
/// and a warning is logged. Files are kept in their original order.
pub fn filter_files(files: Vec<DiffFile>, max_files: usize) -> Vec<DiffFile> {
    let mut kept: Vec<DiffFile> = files
        .into_iter()
        .filter(|f| !is_skippable(&f.filename))
        // Keep files that have hunks, that carry mode-only metadata (e.g.
        // `chmod +x`), or that are renames/copies. All of these are meaningful
        // and must not be silently dropped. Plain empty sections (no hunks,
        // no mode change, no rename/copy) are still excluded.
        .filter(|f| {
            !f.hunks.is_empty()
                || f.mode_change.is_some()
                || f.old_filename.is_some()
                || matches!(f.status, DiffStatus::Renamed | DiffStatus::Copied)
        })
        .collect();

    if kept.len() > max_files {
        let dropped = kept.split_off(max_files);
        for f in &dropped {
            warn!(
                file = %f.filename,
                "Dropping file — exceeded max_diff_files ({})",
                max_files
            );
        }
    }

    kept
}

/// Format filtered diff files back into a compact unified-diff string for the
/// AI prompt. Respects a `max_chars` budget by dropping trailing hunks; never
/// splits a hunk mid-way.
///
/// **Soft limit:** The first file (and its first hunk) is always emitted even
/// if it alone exceeds `max_chars`. This guarantees the AI receives at least
/// one file of context. Hard budget enforcement happens earlier in
/// `truncate_to_budget`, which drops entire files to fit `max_input_tokens`.
pub fn format_diff_context(files: &[DiffFile], max_chars: usize) -> String {
    let mut out = String::new();
    let mut total = 0usize;
    // The first hunk of the first file that fits is emitted unconditionally
    // (soft limit) so the AI always gets at least one file of context. This
    // guarantee applies exactly once, globally — not per file — to avoid
    // overshooting the budget when several files are present.
    let mut soft_used = false;

    for file in files {
        // Build paths for the header. Renames/copies need the original name on
        // the `a/` side; added/deleted files use `/dev/null` for the absent side.
        let a_path = match &file.old_filename {
            Some(old) => format!("a/{}", old),
            None => format!("a/{}", file.filename),
        };
        let git_old = file.old_filename.as_deref().unwrap_or(&file.filename);
        let (diff_git, old_side, new_side) = match file.status {
            DiffStatus::Added => (
                format!("a/dev/null b/{}", file.filename),
                "/dev/null".to_string(),
                format!("b/{}", file.filename),
            ),
            DiffStatus::Deleted => (
                format!("a/{} b/dev/null", file.filename),
                format!("a/{}", file.filename),
                "/dev/null".to_string(),
            ),
            _ => (
                format!("a/{} b/{}", git_old, file.filename),
                a_path.clone(),
                format!("b/{}", file.filename),
            ),
        };
        let file_header = format!(
            "diff --git {}\n--- {}\n+++ {}\n",
            diff_git, old_side, new_side
        );

        if total + file_header.len() > max_chars && !out.is_empty() {
            break;
        }

        let mut file_body = String::new();
        let mut file_total = total + file_header.len();
        let mut emitted_any = false;

        for hunk in &file.hunks {
            let mut hunk_text = format!("{}\n", hunk.header);
            for line in &hunk.lines {
                let prefix = match line.kind {
                    DiffLineKind::Added => '+',
                    DiffLineKind::Removed => '-',
                    DiffLineKind::Context => ' ',
                };
                hunk_text.push_str(&format!("{}{}\n", prefix, line.content));
            }

            // Soft limit: emit the first hunk even if it alone exceeds the
            // budget, but only once globally — i.e. only while no hunk has been
            // emitted anywhere AND the soft allowance is still available. Any
            // later hunk (whether in the same file or a subsequent one) that
            // exceeds the budget is dropped to respect the hard limit.
            let soft_ok = !soft_used;
            if file_total + hunk_text.len() > max_chars && (emitted_any || !soft_ok) {
                // Stop at hunk boundary — don't split a hunk.
                break;
            }

            file_body.push_str(&hunk_text);
            file_total += hunk_text.len();
            emitted_any = true;
            soft_used = true;
        }

        // Mode-change metadata (e.g. chmod) is always emitted when present,
        // whether or not the file also has content hunks — permission changes
        // must never be hidden from the AI.
        let mode_text = file
            .mode_change
            .as_ref()
            .map(|m| format!("Mode change:\n{}\n", m));

        if emitted_any {
            // At least one hunk was emitted: write header + hunks, and append
            // any mode metadata.
            let mut body = std::mem::take(&mut file_body);
            if let Some(mode) = &mode_text {
                body.push_str(mode);
            }
            out.push_str(&file_header);
            out.push_str(&body);
            total = file_total + mode_text.as_ref().map_or(0, |m| m.len());
            soft_used = true;
        } else if let Some(mode) = mode_text {
            // No hunks emitted, but mode metadata exists (mode-only file, or a
            // file whose hunks all exceeded the budget). Emit header + mode text
            // so the change is still visible. This also avoids writing a
            // header-only (empty-body) diff, which would be malformed.
            if file_total + mode.len() > max_chars && !out.is_empty() {
                break;
            }
            out.push_str(&file_header);
            out.push_str(&mode);
            file_total += mode.len();
            total = file_total;
            // This counts as the soft-limit emission: the first file's content
            // has been unconditionally emitted, so subsequent files must respect
            // the hard budget.
            soft_used = true;
        }
        // Otherwise (no hunks emitted AND no mode metadata) the file is skipped
        // entirely — we never write a header with an empty body.
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_diff() {
        assert!(parse_diff("").unwrap().is_empty());
        assert!(parse_diff("   \n  ").unwrap().is_empty());
    }

    #[test]
    fn parse_single_file_with_hunks() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
-    println!(\"old\");
+    let x = 1;
+    println!(\"new {}\", x);
 }";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.filename, "src/main.rs");
        assert_eq!(f.status, DiffStatus::Modified);
        assert_eq!(f.additions, 2);
        assert_eq!(f.deletions, 1);
        assert_eq!(f.hunks.len(), 1);
        assert_eq!(f.hunks[0].lines.len(), 5);
    }

    // Regression test for status detection being fooled by rename/copy
    // markers that appear only inside hunk content (e.g. a code comment that
    // literally says `// rename from foo`). The status must be derived from the
    // diff *header* only, so this file is correctly classified as Modified.
    #[test]
    fn status_not_fooled_by_substring_in_hunks() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
-    // rename from foo
+    // rename from foo
+    let y = 2;
 }";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.filename, "src/main.rs");
        assert_eq!(
            f.status,
            DiffStatus::Modified,
            "comment containing 'rename from' must not classify as Renamed"
        );
        assert_eq!(f.old_filename, None);
    }

    #[test]
    fn parse_new_file() {
        let diff = "\
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,3 @@
+pub fn hello() {
+    println!(\"hi\");
+}";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, DiffStatus::Added);
        assert_eq!(files[0].additions, 3);
        assert_eq!(files[0].deletions, 0);
    }

    #[test]
    fn parse_deleted_file() {
        let diff = "\
diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
--- a/src/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-pub fn goodbye() {
-    println!(\"bye\");
-}";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files[0].status, DiffStatus::Deleted);
        assert_eq!(files[0].deletions, 3);
        // Filename must be the real path, NOT /dev/null.
        assert_eq!(files[0].filename, "src/old.rs");
    }

    #[test]
    fn parse_rename() {
        let diff = "\
diff --git a/old_name.rs b/new_name.rs
similarity index 100%
rename from old_name.rs
rename to new_name.rs
--- a/old_name.rs
+++ b/new_name.rs
@@ -1,1 +1,1 @@
-// old
+// new";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files[0].status, DiffStatus::Renamed);
        assert_eq!(files[0].filename, "new_name.rs");
    }

    #[test]
    fn parse_binary_file() {
        let diff = "\
diff --git a/logo.png b/logo.png
new file mode 100644
Binary files /dev/null and b/logo.png differ";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].hunks.is_empty());
        assert_eq!(files[0].filename, "logo.png");
    }

    #[test]
    fn parse_no_newline_at_eof() {
        let diff = "\
diff --git a/x.txt b/x.txt
--- a/x.txt
+++ b/x.txt
@@ -1,1 +1,1 @@
-old line
\\ No newline at end of file
+new line
\\ No newline at end of file";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
    }

    #[test]
    fn filter_skips_lockfiles_and_binaries() {
        let files = vec![
            DiffFile {
                filename: "package-lock.json".into(),
                old_filename: None,
                status: DiffStatus::Modified,
                hunks: vec![Hunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![],
                }],
                additions: 1,
                deletions: 1,
                mode_change: None,
            },
            DiffFile {
                filename: "src/app.rs".into(),
                old_filename: None,
                status: DiffStatus::Modified,
                hunks: vec![Hunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![],
                }],
                additions: 1,
                deletions: 1,
                mode_change: None,
            },
            DiffFile {
                filename: "image.png".into(),
                old_filename: None,
                status: DiffStatus::Added,
                hunks: vec![], // binary, no hunks
                additions: 0,
                deletions: 0,
                mode_change: None,
            },
        ];
        let filtered = filter_files(files, 50);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].filename, "src/app.rs");
    }

    #[test]
    fn filter_respects_max_files() {
        let files: Vec<DiffFile> = (0..10)
            .map(|i| DiffFile {
                filename: format!("src/file{}.rs", i),
                old_filename: None,
                status: DiffStatus::Modified,
                hunks: vec![Hunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![],
                }],
                additions: 1,
                deletions: 0,
                mode_change: None,
            })
            .collect();
        let filtered = filter_files(files, 3);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn format_diff_context_respects_char_budget() {
        let files = vec![DiffFile {
            filename: "src/big.rs".into(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![
                Hunk {
                    header: "@@ -1,2 +1,2 @@".into(),
                    lines: vec![
                        DiffLine {
                            kind: DiffLineKind::Context,
                            content: "line1".into(),
                            old_lineno: Some(1),
                            new_lineno: Some(1),
                        },
                        DiffLine {
                            kind: DiffLineKind::Added,
                            content: "line2".into(),
                            old_lineno: None,
                            new_lineno: Some(2),
                        },
                    ],
                },
                Hunk {
                    header: "@@ -10,1 +10,1 @@".into(),
                    lines: vec![DiffLine {
                        kind: DiffLineKind::Removed,
                        content: "line10".into(),
                        old_lineno: Some(10),
                        new_lineno: None,
                    }],
                },
            ],
            additions: 1,
            deletions: 1,
            mode_change: None,
        }];

        // Tight budget: only the first hunk should fit.
        let formatted = format_diff_context(&files, 120);
        assert!(formatted.contains("@@ -1,2 +1,2 @@"));
        assert!(!formatted.contains("@@ -10,1 +10,1 @@"));
    }

    #[test]
    fn diffy_parses_binary_file() {
        let bin = "\
diff --git a/logo.png b/logo.png
new file mode 100644
Binary files /dev/null and b/logo.png differ";
        let patch = diffy::Patch::from_str(bin).unwrap();
        assert_eq!(patch.hunks().len(), 0);
    }

    #[test]
    fn parse_binary_file_with_spaces_in_name() {
        let diff = "\
diff --git \"a/my image.png\" \"b/my image.png\"
new file mode 100644
Binary files /dev/null and b/my image.png differ";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "my image.png");
    }

    #[test]
    fn parse_binary_new_file_has_added_status() {
        let diff = "\
diff --git a/logo.png b/logo.png
new file mode 100644
Binary files /dev/null and b/logo.png differ";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files[0].status, DiffStatus::Added);
    }

    #[test]
    fn parse_binary_deleted_file_has_deleted_status() {
        let diff = "\
diff --git a/old.bin b/old.bin
deleted file mode 100644
Binary files a/old.bin and /dev/null differ";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files[0].status, DiffStatus::Deleted);
    }

    #[test]
    fn parse_binary_rename_keeps_old_name() {
        let diff = "\
diff --git a/old.png b/new.png
similarity index 100%
rename from old.png
rename to new.png
Binary files a/old.png and b/new.png differ";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.status, DiffStatus::Renamed);
        assert_eq!(f.filename, "new.png");
        assert_eq!(f.old_filename.as_deref(), Some("old.png"));
    }

    #[test]
    fn parse_binary_copy_keeps_old_name() {
        let diff = "\
diff --git a/tpl.png b/inst.png
similarity index 100%
copy from tpl.png
copy to inst.png
Binary files a/tpl.png and b/inst.png differ";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.status, DiffStatus::Copied);
        assert_eq!(f.filename, "inst.png");
        assert_eq!(f.old_filename.as_deref(), Some("tpl.png"));
    }

    #[test]
    fn parse_mode_only_change_is_retained() {
        // chmod +x: old mode 100644 → new mode 100755, no hunks.
        let diff = "\
diff --git a/build.sh b/build.sh
old mode 100644
new mode 100755";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.filename, "build.sh");
        assert_eq!(f.status, DiffStatus::Modified);
        assert!(f.hunks.is_empty());
        assert!(f.mode_change.is_some());
        let mode = f.mode_change.as_deref().unwrap();
        assert!(mode.contains("old mode 100644"));
        assert!(mode.contains("new mode 100755"));
    }

    #[test]
    fn filter_keeps_mode_only_changes() {
        let files = vec![
            DiffFile {
                filename: "build.sh".into(),
                old_filename: None,
                status: DiffStatus::Modified,
                hunks: vec![],
                additions: 0,
                deletions: 0,
                mode_change: Some("old mode 100644\nnew mode 100755".into()),
            },
            DiffFile {
                filename: "empty.rs".into(),
                old_filename: None,
                status: DiffStatus::Modified,
                hunks: vec![],
                additions: 0,
                deletions: 0,
                mode_change: None,
            },
        ];
        let filtered = filter_files(files, 50);
        // Mode-only file kept; truly empty file dropped.
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].filename, "build.sh");
    }

    #[test]
    fn filter_keeps_pure_renames() {
        // A pure rename has no hunks and no mode_change, but must survive
        // filtering so the AI learns about the rename event.
        let files = vec![
            DiffFile {
                filename: "new_path.rs".into(),
                old_filename: Some("old_path.rs".into()),
                status: DiffStatus::Renamed,
                hunks: vec![],
                additions: 0,
                deletions: 0,
                mode_change: None,
            },
            DiffFile {
                filename: "empty.rs".into(),
                old_filename: None,
                status: DiffStatus::Modified,
                hunks: vec![],
                additions: 0,
                deletions: 0,
                mode_change: None,
            },
        ];
        let filtered = filter_files(files, 50);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].filename, "new_path.rs");
    }

    #[test]
    fn format_context_emits_mode_change() {
        let file = DiffFile {
            filename: "build.sh".into(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![],
            additions: 0,
            deletions: 0,
            mode_change: Some("old mode 100644\nnew mode 100755".into()),
        };
        let out = format_diff_context(&[file], usize::MAX);
        assert!(out.contains("Mode change:"));
        assert!(out.contains("old mode 100644"));
        assert!(out.contains("new mode 100755"));
    }

    #[test]
    fn format_context_emits_mode_change_with_hunks() {
        // A file that has both content hunks AND a mode change must emit the
        // mode metadata alongside the hunks (not hide it).
        let file = DiffFile {
            filename: "build.sh".into(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,2 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Context,
                        content: "line1".into(),
                        old_lineno: Some(1),
                        new_lineno: Some(1),
                    },
                    DiffLine {
                        kind: DiffLineKind::Added,
                        content: "line2".into(),
                        old_lineno: None,
                        new_lineno: Some(2),
                    },
                ],
            }],
            additions: 1,
            deletions: 0,
            mode_change: Some("old mode 100644\nnew mode 100755".into()),
        };
        let out = format_diff_context(&[file], usize::MAX);
        // Both the hunk content and the mode metadata are present.
        assert!(out.contains("+line2"));
        assert!(out.contains("Mode change:"));
        assert!(out.contains("old mode 100644"));
        assert!(out.contains("new mode 100755"));
    }

    #[test]
    fn format_context_skips_header_only_file() {
        // A file with hunks that all exceed the budget (after the soft limit is
        // consumed) and no mode metadata must be skipped entirely — never emit a
        // header with an empty body.
        let mk_big = |name: &str| DiffFile {
            filename: name.to_string(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "x".repeat(500).as_str().into(),
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                }],
            }],
            additions: 0,
            deletions: 0,
            mode_change: None,
        };
        // First file consumes the soft limit (force-emitted). Second file's hunk
        // exceeds budget and has no mode_change → must be skipped, no header-only.
        let files = vec![mk_big("a.rs"), mk_big("b.rs")];
        let out = format_diff_context(&files, 50);
        assert!(out.contains("a.rs"));
        assert!(!out.contains("b.rs"));
        // No malformed header-only entry: the output should not contain a
        // "diff --git" for b.rs at all.
        assert!(!out.contains("diff --git a/b.rs"));
    }

    #[test]
    fn is_skippable_patterns() {
        // Well-known lockfiles are skipped.
        assert!(is_skippable("package-lock.json"));
        assert!(is_skippable("Cargo.lock"));
        assert!(is_skippable("yarn.lock"));
        assert!(is_skippable("go.sum"));
        // Lockfiles in subdirectories are also skipped (basename match).
        assert!(is_skippable("vendor/Cargo.lock"));
        assert!(is_skippable("subdir/package-lock.json"));
        // Other skip patterns.
        assert!(is_skippable("vendor/lib/foo.rs"));
        assert!(is_skippable("app.min.js"));
        assert!(is_skippable("node_modules/dep/index.js"));
        assert!(is_skippable("proto/foo.pb.rs"));
        assert!(is_skippable("CHANGELOG.md"));
        // False positives from the old "contains lock" check must NOT be skipped.
        assert!(!is_skippable("src/block.rs"));
        assert!(!is_skippable("src/flock.c"));
        assert!(!is_skippable("docs/Shakespeare_lock.txt"));
        // Normal source files are kept.
        assert!(!is_skippable("src/main.rs"));
        assert!(!is_skippable("Cargo.toml"));
    }

    #[test]
    fn format_context_header_reflects_status() {
        // Added file → /dev/null on the old side.
        let added = DiffFile {
            filename: "src/new.rs".into(),
            old_filename: None,
            status: DiffStatus::Added,
            hunks: vec![Hunk {
                header: "@@ -0,0 +1,1 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    content: "fn a() {}".into(),
                    old_lineno: None,
                    new_lineno: Some(1),
                }],
            }],
            additions: 1,
            deletions: 0,
            mode_change: None,
        };
        let out = format_diff_context(&[added], usize::MAX);
        assert!(out.contains("diff --git a/dev/null b/src/new.rs"));
        assert!(out.contains("--- /dev/null"));
        assert!(out.contains("+++ b/src/new.rs"));

        // Deleted file → /dev/null on the new side.
        let deleted = DiffFile {
            filename: "src/old.rs".into(),
            old_filename: None,
            status: DiffStatus::Deleted,
            hunks: vec![Hunk {
                header: "@@ -1,1 +0,0 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Removed,
                    content: "fn a() {}".into(),
                    old_lineno: Some(1),
                    new_lineno: None,
                }],
            }],
            additions: 0,
            deletions: 1,
            mode_change: None,
        };
        let out = format_diff_context(&[deleted], usize::MAX);
        assert!(out.contains("diff --git a/src/old.rs b/dev/null"));
        assert!(out.contains("--- a/src/old.rs"));
        assert!(out.contains("+++ /dev/null"));

        // Modified file → both sides present.
        let modified = DiffFile {
            filename: "src/main.rs".into(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "fn main() {}".into(),
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                }],
            }],
            additions: 0,
            deletions: 0,
            mode_change: None,
        };
        let out = format_diff_context(&[modified], usize::MAX);
        assert!(out.contains("--- a/src/main.rs"));
        assert!(out.contains("+++ b/src/main.rs"));
    }

    #[test]
    fn parse_rename_keeps_old_name() {
        let diff = "\
diff --git a/old_path.rs b/new_path.rs
similarity index 95%
rename from old_path.rs
rename to new_path.rs
--- a/old_path.rs
+++ b/new_path.rs
@@ -1,2 +1,2 @@
 fn a() {}
-fn b() {}
+fn c() {}
";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, DiffStatus::Renamed);
        assert_eq!(files[0].filename, "new_path.rs");
        assert_eq!(files[0].old_filename.as_deref(), Some("old_path.rs"));
    }

    #[test]
    fn parse_rename_with_hunks_keeps_both() {
        // A rename that ALSO changes content must retain its hunks (not be
        // treated as a pure rename with empty hunks) and keep the old name.
        let diff = "\
diff --git a/old_path.rs b/new_path.rs
similarity index 80%
rename from old_path.rs
rename to new_path.rs
--- a/old_path.rs
+++ b/new_path.rs
@@ -1,2 +1,2 @@
 fn a() {}
-fn b() {}
+fn c() {}
";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.status, DiffStatus::Renamed);
        assert_eq!(f.filename, "new_path.rs");
        assert_eq!(f.old_filename.as_deref(), Some("old_path.rs"));
        // Crucially the content hunks are preserved, not discarded.
        assert_eq!(f.hunks.len(), 1);
        assert!(
            f.hunks[0]
                .lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Added)
        );
        assert!(
            f.hunks[0]
                .lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Removed)
        );
    }

    #[test]
    fn parse_pure_rename_without_hunks_is_retained() {
        // Pure rename with no content hunks — diffy would fail here, so it must
        // be handled explicitly rather than dropped.
        let diff = "\
diff --git a/old_path.rs b/new_path.rs
similarity index 100%
rename from old_path.rs
rename to new_path.rs";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.status, DiffStatus::Renamed);
        assert_eq!(f.filename, "new_path.rs");
        assert_eq!(f.old_filename.as_deref(), Some("old_path.rs"));
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn parse_pure_copy_without_hunks_is_retained() {
        let diff = "\
diff --git a/template.rs b/instance.rs
similarity index 100%
copy from template.rs
copy to instance.rs";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.status, DiffStatus::Copied);
        assert_eq!(f.filename, "instance.rs");
        assert_eq!(f.old_filename.as_deref(), Some("template.rs"));
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn parse_rename_with_mode_change_keeps_old_name() {
        // Rename + mode change, no hunks. Must keep old_filename AND mode metadata.
        let diff = "\
diff --git a/old.sh b/new.sh
similarity index 100%
rename from old.sh
rename to new.sh
old mode 100644
new mode 100755";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.status, DiffStatus::Renamed);
        assert_eq!(f.filename, "new.sh");
        assert_eq!(f.old_filename.as_deref(), Some("old.sh"));
        assert!(f.mode_change.is_some());
        let mode = f.mode_change.as_deref().unwrap();
        assert!(mode.contains("old mode 100644"));
        assert!(mode.contains("new mode 100755"));
    }

    #[test]
    fn parse_mode_change_with_hunks_is_retained() {
        // A file that changes permissions AND has content hunks must keep both
        // the hunks and the mode metadata (not just one or the other).
        let diff = "\
diff --git a/build.sh b/build.sh
old mode 100644
new mode 100755
--- a/build.sh
+++ b/build.sh
@@ -1 +1 @@
-#!/bin/sh
+#!/usr/bin/env bash
";
        let files = parse_diff(diff).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.status, DiffStatus::Modified);
        assert_eq!(f.hunks.len(), 1);
        assert!(f.mode_change.is_some());
        let mode = f.mode_change.as_deref().unwrap();
        assert!(mode.contains("old mode 100644"));
        assert!(mode.contains("new mode 100755"));
    }

    #[test]
    fn soft_limit_consumed_by_mode_only_first_file() {
        // When the first file is a mode-only change, the soft limit is consumed
        // (soft_used set), so the first hunk of a subsequent file must respect
        // the hard budget and NOT be force-emitted.
        let mk_mode = || DiffFile {
            filename: "build.sh".into(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![],
            additions: 0,
            deletions: 0,
            mode_change: Some("old mode 100644\nnew mode 100755".into()),
        };
        let mk_big = |name: &str| DiffFile {
            filename: name.to_string(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "x".repeat(500).as_str().into(),
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                }],
            }],
            additions: 0,
            deletions: 0,
            mode_change: None,
        };
        // Budget only large enough for the mode-only first file.
        let files = vec![mk_mode(), mk_big("b.rs")];
        let out = format_diff_context(&files, 60);
        assert!(out.contains("build.sh"));
        // The second file's over-budget hunk must NOT be force-emitted.
        assert!(!out.contains("b.rs"));
    }

    #[test]
    fn format_context_rename_header_uses_old_name() {
        let renamed = DiffFile {
            filename: "new_path.rs".into(),
            old_filename: Some("old_path.rs".into()),
            status: DiffStatus::Renamed,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "fn a() {}".into(),
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                }],
            }],
            additions: 0,
            deletions: 0,
            mode_change: None,
        };
        let out = format_diff_context(&[renamed], usize::MAX);
        assert!(out.contains("diff --git a/old_path.rs b/new_path.rs"));
        assert!(out.contains("--- a/old_path.rs"));
        assert!(out.contains("+++ b/new_path.rs"));
    }

    #[test]
    fn soft_limit_applies_only_once_globally() {
        // With a tight budget, only the first file's first hunk is emitted
        // unconditionally; subsequent files' first hunks are NOT force-emitted.
        let mk = |name: &str| DiffFile {
            filename: name.to_string(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "x".repeat(200).as_str().into(),
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                }],
            }],
            additions: 0,
            deletions: 0,
            mode_change: None,
        };
        // Budget just large enough for the first file's hunk.
        let files = vec![mk("a.rs"), mk("b.rs")];
        let out = format_diff_context(&files, 260);
        // Only the first file is present.
        assert!(out.contains("a.rs"));
        assert!(!out.contains("b.rs"));
    }

    #[test]
    fn soft_limit_emits_over_budget_first_hunk_once() {
        // The very first hunk is force-emitted even if it alone exceeds the
        // budget (global soft guarantee). A later file's first hunk must NOT be
        // force-emitted when over budget — it is dropped to respect the limit.
        let big = DiffFile {
            filename: "big.rs".into(),
            old_filename: None,
            status: DiffStatus::Modified,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,1 @@".into(),
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    content: "x".repeat(500).as_str().into(),
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                }],
            }],
            additions: 0,
            deletions: 0,
            mode_change: None,
        };
        // Budget smaller than the single hunk. The first (and only) file is
        // still emitted because the soft limit applies to the global first hunk.
        let out = format_diff_context(&[big], 10);
        assert!(out.contains("big.rs"));
        assert!(out.contains("xxxxxxxxxx"));
    }
}
