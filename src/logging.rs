use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber for structured logging.
///
/// Supports two output formats via the `LOG_FORMAT` env var:
/// - `json` — machine-readable JSON lines (for Docker/CI)
/// - anything else — human-readable compact format (default)
///
/// Log level is controlled by `RUST_LOG` env var or falls back to
/// `info` (or `debug` if `verbose` is true).
pub fn init(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let is_json = std::env::var("LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if is_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .flatten_event(true)
            .with_current_span(false)
            .with_file(false)
            .with_line_number(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .compact()
            .init();
    }
}
