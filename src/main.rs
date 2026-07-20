use clap::{Parser, Subcommand};
use review_agent::logging;
use review_agent::tools::review::ReviewTool;
use std::num::ParseIntError;

#[derive(Parser)]
#[command(name = "review-agent", version, about = "AI-powered PR review agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Review a pull request
    Review {
        /// URL of the pull request to review
        #[arg(long)]
        pr_url: String,
    },
    /// Start webhook server (coming soon)
    Serve {
        /// Port to listen on
        #[arg(long, default_value = "8080")]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(cli.verbose);

    match &cli.command {
        Command::Review { pr_url } => {
            tracing::info!(pr_url, "Review command");
            let settings = review_agent::Settings::load()?;
            tracing::info!(
                api_base = %settings.ai.api_base,
                model = %settings.ai.model,
                "Settings loaded"
            );

            let (owner, repo, number) = parse_pr_url(pr_url)?;
            tracing::info!(owner, repo, number, "Parsed PR URL");

            let tool = ReviewTool::new(&settings)?;
            let output = tool.run(&owner, &repo, number).await?;

            // Emit a step summary if running in GitHub Actions.
            // Unix: full TOCTOU-safe open with symlink-swap detection.
            // Non-Unix: simpler open + write (no dev/ino comparison available).
            #[cfg(unix)]
            let _ = (|| -> anyhow::Result<()> {
                use std::os::unix::fs::MetadataExt;

                let summary_path = match std::env::var("GITHUB_STEP_SUMMARY") {
                    Ok(p) => p,
                    Err(_) => return Ok(()),
                };

                // Open the file first, then validate its properties on the
                // open file handle — this eliminates the TOCTOU race between
                // checking the path and opening the file.
                let mut f = match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&summary_path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = %summary_path,
                            "Failed to open step summary file — skipping summary"
                        );
                        return Ok(());
                    }
                };

                // Resolve the real path and verify it matches the open file
                // handle via device+inode comparison.  This closes the symlink
                // swap window between open + metadata + canonicalize.
                let canonical = match f.metadata() {
                    Ok(meta) => {
                        if !meta.is_file() {
                            tracing::warn!(
                                path = %summary_path,
                                "GITHUB_STEP_SUMMARY is not a regular file — skipping summary"
                            );
                            return Ok(());
                        }
                        let dev = meta.dev();
                        let ino = meta.ino();
                        let canon = std::fs::canonicalize(&summary_path)
                            .unwrap_or_else(|_| summary_path.clone().into());
                        // Verify the canonical path resolves to the same inode
                        // as the already-open handle — detects symlink swaps.
                        match std::fs::metadata(&canon) {
                            Ok(canon_meta)
                                if canon_meta.dev() == dev && canon_meta.ino() == ino =>
                            {
                                canon
                            }
                            Ok(_) => {
                                tracing::warn!(
                                    path = %canon.display(),
                                    "GITHUB_STEP_SUMMARY symlink swapped between open and resolve — skipping summary"
                                );
                                return Ok(());
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    path = %canon.display(),
                                    "Cannot read canonical path metadata — skipping summary"
                                );
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = %summary_path,
                            "Cannot read step summary metadata — skipping summary"
                        );
                        return Ok(());
                    }
                };

                // Verify the path is in an expected GitHub Actions directory.
                if !is_safe_summary_path(&canonical) {
                    return Ok(());
                }

                write_step_summary(&mut f, &output)
            })();

            #[cfg(not(unix))]
            let _ = (|| -> anyhow::Result<()> {
                let summary_path = match std::env::var("GITHUB_STEP_SUMMARY") {
                    Ok(p) => p,
                    Err(_) => return Ok(()),
                };

                let mut f = match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&summary_path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = %summary_path,
                            "Failed to open step summary file — skipping summary"
                        );
                        return Ok(());
                    }
                };

                // Basic safety check: verify it's a regular file.
                if let Ok(meta) = f.metadata() {
                    if !meta.is_file() {
                        tracing::warn!(
                            path = %summary_path,
                            "GITHUB_STEP_SUMMARY is not a regular file — skipping summary"
                        );
                        return Ok(());
                    }
                }

                write_step_summary(&mut f, &output)
            })();

            println!("Review posted for PR #{}", output.pr_number);
        }
        Command::Serve { port } => {
            tracing::info!(port, "Serve command (not yet implemented)");
            println!("Webhook server stub on port {}", port);
        }
    }

    Ok(())
}

/// Check whether a canonicalized path is in an expected GitHub Actions location.
/// Only used on Unix (where TOCTOU detection is active).
#[cfg(unix)]
fn is_safe_summary_path(canonical: &std::path::Path) -> bool {
    let runner_temp = std::env::var("RUNNER_TEMP").unwrap_or_default();
    let workspace = std::env::var("GITHUB_WORKSPACE").unwrap_or_default();
    let path_str = canonical.to_string_lossy();
    path_str.starts_with("/tmp/")
        || path_str.starts_with("/home/runner/")
        || path_str.starts_with("/github/")
        || (!runner_temp.is_empty() && path_str.starts_with(&runner_temp))
        || (!workspace.is_empty() && path_str.starts_with(&workspace))
}

/// Write the step summary rows and flush. Shared by Unix and non-Unix code paths.
fn write_step_summary(
    f: &mut std::fs::File,
    output: &review_agent::tools::review::ReviewOutput,
) -> anyhow::Result<()> {
    use std::io::Write;
    // Escape a value for safe inclusion in a markdown table cell.
    // Uses the HTML entity &#124; for pipes since \| is not universally
    // supported by Markdown renderers.
    // A single char-scan avoids clippy::collapsible_str_replace.
    let md_escape = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '|' => out.push_str("&#124;"),
                '\n' | '\r' => out.push(' '),
                _ => out.push(c),
            }
        }
        out
    };
    // Leading newline ensures separation from any prior
    // content in the step summary file.
    let rows = [
        String::new(),
        "## 🔍 Review Complete".to_string(),
        String::new(),
        "| Metric | Value |".to_string(),
        "|---|---|".to_string(),
        format!(
            "| PR | #{} {} |",
            output.pr_number,
            md_escape(&output.pr_title)
        ),
        format!("| Files changed | {} |", output.files_changed),
        format!("| Files reviewed | {} |", output.files_reviewed),
        format!("| Files skipped | {} |", output.files_skipped),
        format!("| Est. input tokens | {} |", output.input_tokens_estimated),
        format!("| Latency | {} ms |", output.latency_ms),
    ];
    for row in &rows {
        if let Err(e) = writeln!(f, "{row}") {
            tracing::warn!(error = %e, "Failed to write step summary row");
        }
    }
    // Flush to ensure all content is written before the
    // process exits — otherwise a panic after this point
    // could lose buffered output.
    if let Err(e) = f.flush() {
        tracing::warn!(error = %e, "Failed to flush step summary file");
    }
    Ok(())
}

/// Parse a GitHub PR URL of the form
/// `https://github.com/{owner}/{repo}/pull/{number}` into its components.
///
/// Also accepts `www.github.com`. Uses the `url` crate for parsing so that
/// percent-encoded path segments (e.g. `user%2Fname`) are decoded correctly.
fn parse_pr_url(url_str: &str) -> anyhow::Result<(String, String, u64)> {
    // Trim trailing slash before parsing — without it, path_segments() includes
    // a trailing empty segment that breaks segment-count checks.
    let url_str = url_str.trim_end_matches('/');
    let parsed = url::Url::parse(url_str).map_err(|_| {
        anyhow::anyhow!("Invalid PR URL: expected https://github.com/owner/repo/pull/N")
    })?;

    if parsed.scheme() != "https" {
        anyhow::bail!("Invalid PR URL: expected https://github.com/owner/repo/pull/N");
    }

    let host = parsed.host_str().unwrap_or("");
    if !host.eq_ignore_ascii_case("github.com") && !host.eq_ignore_ascii_case("www.github.com") {
        anyhow::bail!("Invalid PR URL: expected https://github.com/owner/repo/pull/N");
    }

    // url::Url path_segments() returns raw (percent-encoded) segments.
    // Decode each segment — reject non-UTF-8 instead of silently replacing.
    let segments: Vec<String> = parsed
        .path_segments()
        .map(|s| -> anyhow::Result<Vec<String>> {
            s.map(|seg| {
                percent_encoding::percent_decode_str(seg)
                    .decode_utf8()
                    .map(|c| c.into_owned())
                    .map_err(|_| {
                        anyhow::anyhow!("Invalid PR URL: path segment contains non-UTF-8 bytes")
                    })
            })
            .collect()
        })
        .transpose()?
        .unwrap_or_default();
    // Expected: ["owner", "repo", "pull", "number"]
    if segments.len() != 4 || !segments[segments.len() - 2].eq_ignore_ascii_case("pull") {
        anyhow::bail!("Invalid PR URL: expected https://github.com/owner/repo/pull/N");
    }

    let owner = segments[0].clone();
    let repo = segments[1].clone();
    let number: u64 = segments[3]
        .parse()
        .map_err(|e: ParseIntError| anyhow::anyhow!("Invalid PR number: {}", e))?;

    // Validate that owner/repo are safe identifiers.  After percent-decoding,
    // a `%2F` would produce a literal `/`, enabling path traversal in downstream
    // API calls.  GitHub owner/repo names only allow: alphanumeric, `.`, `_`, `-`.
    let validate_segment = |name: &str, label: &str| -> anyhow::Result<()> {
        if name.is_empty() {
            anyhow::bail!("{label} is empty");
        }
        if name.contains('/') || name.contains("..") {
            anyhow::bail!("{label} `{name}` contains path traversal characters");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        {
            anyhow::bail!("{label} `{name}` contains invalid characters");
        }
        Ok(())
    };
    validate_segment(&owner, "owner")?;
    validate_segment(&repo, "repo")?;

    Ok((owner, repo, number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_pr_url() {
        let (owner, repo, number) =
            parse_pr_url("https://github.com/devstroop/review-agent/pull/42").unwrap();
        assert_eq!(owner, "devstroop");
        assert_eq!(repo, "review-agent");
        assert_eq!(number, 42);
    }

    #[test]
    fn parse_pr_url_with_trailing_slash() {
        let (owner, repo, number) = parse_pr_url("https://github.com/o/r/pull/7/").unwrap();
        assert_eq!(owner, "o");
        assert_eq!(repo, "r");
        assert_eq!(number, 7);
    }

    #[test]
    fn parse_pr_url_with_query() {
        let (_owner, _repo, number) =
            parse_pr_url("https://github.com/a/b/pull/99?foo=bar").unwrap();
        assert_eq!(number, 99);
    }

    #[test]
    fn parse_invalid_pr_url() {
        assert!(parse_pr_url("https://github.com/devstroop/review-agent").is_err());
        assert!(parse_pr_url("not-a-url").is_err());
        assert!(parse_pr_url("https://github.com/o/r/pull/abc").is_err());
    }

    #[test]
    fn parse_pr_url_with_fragment() {
        let (owner, repo, number) = parse_pr_url("https://github.com/o/r/pull/7#section").unwrap();
        assert_eq!(owner, "o");
        assert_eq!(repo, "r");
        assert_eq!(number, 7);
    }

    #[test]
    fn parse_pr_url_extra_path_segments_rejected() {
        // Extra path before owner/repo should not silently mis-parse.
        assert!(parse_pr_url("https://github.com/base/o/r/pull/123").is_err());
    }

    #[test]
    fn parse_pr_url_wrong_host_rejected() {
        assert!(parse_pr_url("https://gitlab.com/o/r/pull/1").is_err());
        assert!(parse_pr_url("https://malicious.example.com/o/r/pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_http_scheme_rejected() {
        assert!(parse_pr_url("http://github.com/o/r/pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_case_insensitive_host() {
        let number = parse_pr_url("https://GITHUB.COM/o/r/pull/1").unwrap().2;
        assert_eq!(number, 1);
        let number = parse_pr_url("https://Github.com/o/r/pull/1").unwrap().2;
        assert_eq!(number, 1);
    }

    #[test]
    fn parse_pr_url_www_subdomain() {
        let (owner, repo, number) = parse_pr_url("https://www.github.com/o/r/pull/1").unwrap();
        assert_eq!(owner, "o");
        assert_eq!(repo, "r");
        assert_eq!(number, 1);
    }

    #[test]
    fn parse_pr_url_percent_decoded_rejected() {
        // Percent-decoded `/` would enable path traversal — must be rejected.
        assert!(parse_pr_url("https://github.com/user%2Fname/repo%2Btest/pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_error_messages_are_descriptive() {
        let err = parse_pr_url("not-a-url").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Invalid PR URL"),
            "expected error about invalid URL, got: {msg}"
        );

        let err = parse_pr_url("https://gitlab.com/o/r/pull/1").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Invalid PR URL"),
            "expected error about invalid PR URL, got: {msg}"
        );

        let err = parse_pr_url("http://github.com/o/r/pull/1").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Invalid PR URL"),
            "expected error about invalid PR URL, got: {msg}"
        );
    }

    #[test]
    fn parse_pr_url_case_insensitive_pull() {
        let (_owner, _repo, number) = parse_pr_url("https://github.com/o/r/Pull/1").unwrap();
        assert_eq!(number, 1);

        let number = parse_pr_url("https://github.com/o/r/PULL/2").unwrap().2;
        assert_eq!(number, 2);

        let number = parse_pr_url("https://github.com/o/r/pUlL/3").unwrap().2;
        assert_eq!(number, 3);
    }
}
