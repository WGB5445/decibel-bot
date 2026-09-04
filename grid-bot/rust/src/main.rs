mod cli;
mod engine;
mod tui;

use std::{
    backtrace::Backtrace,
    io::{self, Write},
    panic::{self, PanicHookInfo},
};

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::DisableMouseCapture,
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};
use dotenvy::dotenv;

use cli::settings::has_complete_grid_config;
use cli::{Cli, Cmd, Settings, control_paths, redirect_output_to_log};
use cli::{
    attach_client, check_api_key, doctor_cli, engine_cli, logs_client, reconcile_cli, shadow_cli,
    simulate_cli, spot_funding_setup_cli, start_cli, status_client, stop_client,
};
use tui::{TAB_CONFIG, TAB_MONITOR, TAB_PREVIEW, run_tui, save_error_report};

fn install_panic_reporter() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        let backtrace = Backtrace::force_capture();
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown location".to_owned());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        let report = format!(
            "Rust panic\n\nmessage: {payload}\nlocation: {location}\n\nbacktrace:\n{backtrace}\n"
        );
        let path = save_error_report(&report).ok();

        // Panic can happen while crossterm is in raw/alternate-screen mode. Restore the
        // terminal before printing, otherwise the panic text remains trapped in the TUI.
        let mut stdout = io::stdout();
        let _ = disable_raw_mode();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
        let _ = writeln!(
            stdout,
            "\nDecibel Grid TUI panicked: {payload}\nLocation: {location}"
        );
        if let Some(path) = path {
            let _ = writeln!(stdout, "Full panic report: {}", path.display());
        }
        let _ = stdout.flush();
        previous(info);
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    // reqwest 0.13's `rustls` feature hard-selects aws-lc-rs, and aptos-sdk pulls it in with
    // default features, so aws-lc-rs is the only provider in the tree. Install it explicitly
    // before any TLS handshake: rustls refuses to guess, and tokio-tungstenite builds its
    // ClientConfig lazily on first connect, which is where the panic surfaced.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls aws-lc-rs CryptoProvider"))?;
    install_panic_reporter();
    dotenv().ok();
    let cli = Cli::parse();
    let opens_tui = matches!(&cli.command, Some(Cmd::Preview | Cmd::Tui) | None);
    let engine_log = if matches!(&cli.command, Some(Cmd::Engine)) {
        Some(control_paths(&Settings::from(&cli.args))?.log)
    } else {
        None
    };
    let log_path = cli.args.log_file.clone().or(engine_log);
    if let Some(path) = log_path.as_deref() {
        if opens_tui {
            anyhow::bail!("--log-file is only supported by CLI commands, not TUI/preview")
        }
        redirect_output_to_log(path)?;
        println!(
            "CLI log started; output is being overwritten at {}",
            path.display()
        );
    }
    match cli.command {
        Some(Cmd::Start) => {
            start_cli(
                Settings::from(&cli.args),
                cli.args.confirm_mainnet.as_deref(),
            )
            .await
        }
        Some(Cmd::Engine) => {
            engine_cli(
                Settings::from(&cli.args),
                cli.args.confirm_mainnet.as_deref(),
                log_path.clone(),
            )
            .await
        }
        Some(Cmd::Logs) => logs_client(Settings::from(&cli.args), cli.args.follow).await,
        Some(Cmd::Attach) => attach_client(Settings::from(&cli.args)).await,
        Some(Cmd::CheckKey) => check_api_key(Settings::from(&cli.args)).await,
        Some(Cmd::Reconcile) => reconcile_cli(Settings::from(&cli.args)).await,
        Some(Cmd::Status) => status_client(Settings::from(&cli.args)).await,
        Some(Cmd::Doctor) => doctor_cli(Settings::from(&cli.args)).await,
        Some(Cmd::Shadow) => shadow_cli(Settings::from(&cli.args), cli.args.shadow_cycles).await,
        Some(Cmd::SpotFundingSetup) => {
            spot_funding_setup_cli(
                Settings::from(&cli.args),
                cli.args.spot_funding_amount.clone(),
                cli.args.spot_funding_metadata.clone(),
            )
            .await
        }
        Some(Cmd::Simulate) => simulate_cli(cli.args.scenario.as_deref()),
        Some(Cmd::Run) => anyhow::bail!(
            "`run` no longer owns a live trading loop; use `start` (or let systemd/tmux run the internal `engine` command) and control it with status/logs/stop/attach"
        ),
        Some(Cmd::Stop) => {
            stop_client(
                Settings::from(&cli.args),
                cli.args.confirm_mainnet.as_deref(),
            )
            .await
        }
        Some(Cmd::Preview) => {
            run_tui(
                Settings::from(&cli.args),
                cli.args.profile.clone(),
                TAB_PREVIEW,
            )
            .await
        }
        Some(Cmd::Tui) => {
            run_tui(
                Settings::from(&cli.args),
                cli.args.profile.clone(),
                TAB_CONFIG,
            )
            .await
        }
        None if has_complete_grid_config(&cli.args) => {
            run_tui(
                Settings::from(&cli.args),
                cli.args.profile.clone(),
                TAB_MONITOR,
            )
            .await
        }
        None => {
            run_tui(
                Settings::from(&cli.args),
                cli.args.profile.clone(),
                TAB_CONFIG,
            )
            .await
        }
    }
}
