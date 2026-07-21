//! Sanitize AI model output before posting to GitHub.
//!
//! Strips potential prompt-injection vectors:
//! - GitHub Actions workflow command syntax (`::command params::value`)
//! - ANSI escape sequences
//!
//! Fenced code blocks are preserved verbatim and force-closed after
//! `MAX_CODE_BLOCK_LINES` lines as a safety net against unmatched
//! opening fences.

/// Maximum number of lines inside a single fenced code block before we
/// force-close it as a safety net against unmatched opening fences.
const MAX_CODE_BLOCK_LINES: usize = 500;

/// Sanitize AI output before posting to GitHub.
pub(crate) fn sanitize_output(text: &str) -> String {
    use std::fmt::Write;

    let mut result = String::with_capacity(text.len());

    // Track the active code block fence (None = outside a block).
    let mut fence: Option<String> = None;
    let mut code_block_lines: usize = 0;

    for line in text.lines() {
        // ── Fenced code block detection ──────────────────────────
        if let Some((ch, count)) = parse_fence(line) {
            match fence {
                None => {
                    let delim: String = (0..count).map(|_| ch).collect();
                    fence = Some(delim);
                    code_block_lines = 0;
                    let _ = writeln!(result, "{line}");
                    continue;
                }
                Some(ref open_delim) => {
                    let expected_count = open_delim.len();
                    let is_close =
                        ch == open_delim.chars().next().unwrap() && count >= expected_count;
                    if is_close {
                        fence = None;
                    } else {
                        code_block_lines += 1;
                        if code_block_lines > MAX_CODE_BLOCK_LINES {
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
                let _ = writeln!(result, "{line}");
                let _ = writeln!(result, "{delim}");
                fence = None;
                continue;
            } else {
                let _ = writeln!(result, "{line}");
                continue;
            }
        }

        let cleaned = strip_ansi_escapes(line);
        let cleaned = strip_workflow_commands(&cleaned);
        let _ = writeln!(result, "{cleaned}");
    }

    if let Some(ref delim) = fence {
        let _ = writeln!(result, "{delim}");
    }

    result
}

/// Check whether a line is a Markdown fenced code block delimiter.
fn parse_fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars().peekable();

    let (ch, count) = match chars.peek() {
        Some('`') => ('`', trimmed.bytes().take_while(|&b| b == b'`').count()),
        Some('~') => ('~', trimmed.bytes().take_while(|&b| b == b'~').count()),
        _ => return None,
    };

    if count < 3 {
        return None;
    }

    // Always accept — our permissive parser errs on the side of treating a
    // line as a fence, because content inside a code block is preserved
    // verbatim (safe).  Treating a fence as non-fence would let workflow
    // commands bypass sanitization.
    Some((ch, count))
}

/// Remove GitHub Actions workflow command syntax from a string.
fn strip_workflow_commands(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == ':' && chars.peek() == Some(&':') {
            chars.next();
            let mut ahead = chars.clone();
            let mut found = false;
            while let Some(c) = ahead.next() {
                if c == ':' && ahead.peek() == Some(&':') {
                    found = true;
                    break;
                }
            }
            if found {
                loop {
                    match chars.next() {
                        Some(':') if chars.peek() == Some(&':') => {
                            chars.next();
                            break;
                        }
                        Some(_) => continue,
                        None => break,
                    }
                }
                for c in &mut chars {
                    if c == '\n' {
                        break;
                    }
                }
            } else {
                result.push_str("::");
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Remove ANSI escape sequences from a string.
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
    fn sanitize_ansi_obfuscated_command_stripped() {
        let input = ":\u{1b}[0m:warning::something happened";
        let result = sanitize_output(input);
        assert!(!result.contains("warning::"), "got: {result}");
    }

    #[test]
    fn sanitize_preserves_code_blocks_verbatim() {
        let input = "before\n```\n::set-output name=foo::bar\n\x1b[31mred\x1b[0m\n```\nafter";
        let result = sanitize_output(input);
        assert!(result.contains("before"));
        assert!(result.contains("after"));
        assert!(result.contains("::set-output"));
        assert!(
            result.contains("\x1b[31mred\x1b[0m"),
            "ANSI escapes inside code blocks should be preserved"
        );
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
        assert_eq!(parse_fence("``` rust"), Some(('`', 3)));
        assert_eq!(parse_fence("```rust"), Some(('`', 3)));
        assert_eq!(parse_fence("``` ruby"), Some(('`', 3)));
        assert_eq!(parse_fence("~~~bash"), Some(('~', 3)));
        assert_eq!(parse_fence("~~~ python"), Some(('~', 3)));
    }

    #[test]
    fn parse_fence_less_than_three_rejected() {
        assert_eq!(parse_fence("``"), None);
        assert_eq!(parse_fence("~~"), None);
        assert_eq!(parse_fence("`"), None);
    }

    #[test]
    fn parse_fence_inline_code_not_a_fence() {
        assert_eq!(parse_fence("`code`"), None);
        assert_eq!(parse_fence("``code``"), None);
    }

    // ── Fence matching in sanitize_output ────────────────────────

    #[test]
    fn sanitize_mismatched_fences_rejected() {
        let input = "before\n```\n::warning::x\n~~~~\nafter";
        let result = sanitize_output(input);
        assert!(result.contains("::warning::"));
        assert!(result.contains("~~~~"));
    }

    #[test]
    fn sanitize_inline_backticks_not_fences() {
        let input = "text with `inline code` and ::warning::x";
        let result = sanitize_output(input);
        assert!(!result.contains("::warning::"));
    }

    #[test]
    fn sanitize_force_closes_unmatched_fence() {
        let mut lines = vec!["```".to_string()];
        for i in 0..MAX_CODE_BLOCK_LINES + 2 {
            lines.push(format!("line {i}"));
        }
        lines.push("::warning::should be stripped".to_string());
        let input = lines.join("\n");
        let result = sanitize_output(&input);
        assert_eq!(
            result.matches("```").count(),
            2,
            "expected opening + force-closed fence, got: {result}"
        );
        assert!(
            !result.contains("::warning::"),
            "after force-close the workflow command should be stripped"
        );
    }

    #[test]
    fn sanitize_closes_fence_at_end_of_input() {
        let input = "text\n```\nunclosed block";
        let result = sanitize_output(input);
        assert!(
            result.ends_with("```\n") || result.ends_with("```"),
            "expected closing fence at end, got: {result:?}"
        );
    }

    #[test]
    fn sanitize_false_fences_count_toward_limit() {
        let mut lines = vec!["```".to_string()];
        // MAX_CODE_BLOCK_LINES lines inside the block, then a false fence,
        // then MAX_CODE_BLOCK_LINES more lines — the false fence line counts
        // toward the limit so the block should force-close shortly after.
        for i in 0..MAX_CODE_BLOCK_LINES / 2 {
            lines.push(format!("line {i}"));
        }
        lines.push("~~~~".to_string());
        for i in 0..MAX_CODE_BLOCK_LINES / 2 {
            lines.push(format!("line {}", MAX_CODE_BLOCK_LINES / 2 + i));
        }
        lines.push("::warning::should be stripped".to_string());
        let input = lines.join("\n");
        let result = sanitize_output(&input);
        assert!(!result.contains("::warning::"));
    }
}
