use clap::{Parser, Subcommand};
use review_agent::logging;

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
            println!("PR URL: {}", pr_url);
            println!("Model: {}", settings.ai.model);
            println!("API Base: {}", settings.ai.api_base);
            // TODO: Phase 5 — wire up the review tool
        }
        Command::Serve { port } => {
            tracing::info!(port, "Serve command (not yet implemented)");
            println!("Webhook server stub on port {}", port);
        }
    }

    Ok(())
}
