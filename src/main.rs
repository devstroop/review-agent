use clap::{Parser, Subcommand};
use review_agent::github::parse_pr_url;
use review_agent::logging;
use review_agent::tools::review::ReviewTool;

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
    /// Start webhook server for real-time PR review processing
    Serve {
        /// Port to listen on
        #[arg(long, default_value = "8080")]
        port: u16,
        /// Secret for verifying GitHub webhook signatures (optional)
        #[arg(long, env = "WEBHOOK_SECRET")]
        webhook_secret: Option<String>,
    },
    /// Start MCP stdio server for AI agent integration
    Mcp,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(cli.verbose);

    match &cli.command {
        #[allow(deprecated)]
        Command::Review { pr_url } => {
            tracing::info!(pr_url, "Review command");
            let settings = review_agent::Settings::load()?;
            tracing::info!(
                api_base = %settings.ai.api_base,
                model = %settings.ai.model,
                "Settings loaded"
            );

            let (owner, repo, number) =
                parse_pr_url(pr_url).map_err(|e| anyhow::anyhow!("{}", e))?;
            tracing::info!(owner, repo, number, "Parsed PR URL");

            let tool = ReviewTool::new(&settings)?;
            let output = tool.run(&owner, &repo, number).await?;

            // Destructure before constructing closures that capture it.
            let pr_number = output.pr_number;

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

            println!("Review posted for PR #{}", pr_number);
        }
        Command::Serve { port, .. } => {
            tracing::info!(port, "Starting webhook server");

            let settings = review_agent::Settings::load()?;
            tracing::info!(
                api_base = %settings.ai.api_base,
                model = %settings.ai.model,
                "Settings loaded"
            );

            let app = review_agent::server::router(settings);
            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to bind port {}: {}", port, e))?;

            tracing::info!(port, "Webhook server listening");
            axum::serve(listener, app)
                .await
                .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

            println!("Webhook server listening on port {}", port);
        }
        Command::Mcp => {
            tracing::info!("Starting MCP stdio server");
            let settings = review_agent::Settings::load()?;
            review_agent::mcp::run(&settings).await?;
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
#[allow(deprecated)]
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
        format!(
            "| Output tokens | {} |",
            output
                .output_tokens_reported
                .map(|t| t.to_string())
                .unwrap_or_else(|| "N/A".to_string())
        ),
        format!(
            "| Total tokens | {} |",
            output
                .total_tokens_used
                .map(|t| t.to_string())
                .unwrap_or_else(|| "N/A".to_string())
        ),
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
