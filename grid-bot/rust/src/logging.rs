use tracing_subscriber::EnvFilter;

/// Install one bounded, parseable log format for CLI and detached engine output.
///
/// `GRID_LOG` takes precedence over the standard `RUST_LOG`. The default deliberately excludes
/// per-cycle plan detail, which is available with `GRID_LOG=debug` during diagnosis.
pub fn init() {
    let filter = std::env::var("GRID_LOG")
        .ok()
        .or_else(|| std::env::var("RUST_LOG").ok())
        .and_then(|value| value.parse::<EnvFilter>().ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .try_init();
}
