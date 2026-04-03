use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod api;
mod aptos;
mod bot;
mod config;
mod pricing;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env before anything else so env vars are visible to clap and tracing.
    // Using `.ok()` so the bot still starts when no .env file is present.
    match dotenvy::dotenv() {
        Ok(path) => eprintln!("Loaded env from {}", path.display()),
        Err(dotenvy::Error::Io(_)) => {} // .env not found — that's fine
        Err(e) => eprintln!("Warning: .env parse error: {e}"),
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("decibel_mm_bot=debug,info")),
        )
        .with_target(false)
        .init();

    let args = config::Args::parse();
    bot::run(args).await
}
