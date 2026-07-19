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

/// Parse a GitHub PR URL of the form
/// `https://github.com/{owner}/{repo}/pull/{number}` into its components.
fn parse_pr_url(url_str: &str) -> anyhow::Result<(String, String, u64)> {
    let parsed = url::Url::parse(url_str).map_err(|e| anyhow::anyhow!("Invalid URL: {}", e))?;

    // Validate scheme.
    let scheme = parsed.scheme();
    if scheme != "https" && scheme != "http" {
        anyhow::bail!("Invalid PR URL scheme '{}': expected http or https", scheme);
    }

    // Get clean path segments (query, fragment, trailing slash handled by Url).
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();

    // Find "pull" segment that is followed by a parseable PR number.
    // Require exactly 2 segments before it (owner, repo) — extra
    // subdirectories such as /a/b/c/pull/1 are rejected.
    let pull_idx = segments
        .windows(2)
        .position(|w| w[0] == "pull" && w[1].parse::<u64>().is_ok())
        .filter(|&i| i == 2)
        .ok_or_else(|| {
            anyhow::anyhow!("Invalid PR URL: expected https://HOST/owner/repo/pull/N")
        })?;

    let owner = segments[pull_idx - 2].to_string();
    let repo = segments[pull_idx - 1].to_string();
    let number: u64 = segments[pull_idx + 1]
        .parse()
        .map_err(|e: ParseIntError| anyhow::anyhow!("Invalid PR number: {}", e))?;

    if owner.is_empty() || repo.is_empty() {
        anyhow::bail!("Could not extract owner/repo from URL");
    }

    Ok((owner, repo, number))
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
            if let Ok(summary_path) = std::env::var("GITHUB_STEP_SUMMARY") {
                let write_result = (|| -> std::io::Result<()> {
                    use std::io::Write;
                    let f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&summary_path)?;
                    let mut w = std::io::BufWriter::new(f);
                    writeln!(w, "## 🔍 Review Complete")?;
                    writeln!(w)?;
                    writeln!(w, "| Metric | Value |")?;
                    writeln!(w, "|---|---|")?;
                    writeln!(
                        w,
                        "| PR | #{} `{}` |",
                        output.pr_number,
                        output.pr_title.replace('|', "&#124;")
                    )?;
                    writeln!(w, "| Files changed | {} |", output.files_changed)?;
                    writeln!(w, "| Files reviewed | {} |", output.files_reviewed)?;
                    writeln!(w, "| Files skipped | {} |", output.files_skipped)?;
                    writeln!(
                        w,
                        "| Est. input tokens | {} |",
                        output.input_tokens_estimated
                    )?;
                    writeln!(w, "| Latency | {} ms |", output.latency_ms)?;
                    w.flush()?;
                    Ok(())
                })();
                if let Err(e) = write_result {
                    tracing::warn!(error = %e, "Failed to write GITHUB_STEP_SUMMARY");
                }
            }

            println!("Review posted for PR #{}", output.pr_number);
        }
        Command::Serve { port } => {
            tracing::info!(port, "Serve command (not yet implemented)");
            println!("Webhook server stub on port {}", port);
        }
    }

    Ok(())
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
    fn parse_pr_url_accepts_any_host() {
        // The parser is purely positional — any host works, which supports
        // GitHub Enterprise instances with a custom API base URL.
        let (owner, repo, number) =
            parse_pr_url("https://gitlab.internal/owner/repo/pull/1").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
        assert_eq!(number, 1);

        let (owner2, repo2, number2) = parse_pr_url("http://example.com/a/b/pull/2").unwrap();
        assert_eq!(owner2, "a");
        assert_eq!(repo2, "b");
        assert_eq!(number2, 2);
    }

    #[test]
    fn parse_pr_url_rejects_wrong_scheme() {
        assert!(parse_pr_url("ftp://github.com/owner/repo/pull/1").is_err());
        assert!(parse_pr_url("ssh://github.com/o/r/pull/1").is_err());
    }

    #[test]
    fn parse_pr_url_with_trailing_path_segments() {
        // Canonical PR URLs often include /files, /commits, etc.
        let (owner, repo, number) = parse_pr_url("https://github.com/o/r/pull/7/files").unwrap();
        assert_eq!(owner, "o");
        assert_eq!(repo, "r");
        assert_eq!(number, 7);

        let (_, _, number2) = parse_pr_url("https://github.com/o/r/pull/99/commits").unwrap();
        assert_eq!(number2, 99);
    }

    #[test]
    fn parse_pr_url_with_fragment() {
        let (owner, repo, number) =
            parse_pr_url("https://github.com/o/r/pull/7#issuecomment-123").unwrap();
        assert_eq!(owner, "o");
        assert_eq!(repo, "r");
        assert_eq!(number, 7);
    }

    #[test]
    fn parse_pr_url_with_http() {
        let (owner, repo, number) = parse_pr_url("http://github.com/o/r/pull/3").unwrap();
        assert_eq!(owner, "o");
        assert_eq!(repo, "r");
        assert_eq!(number, 3);
    }
}
