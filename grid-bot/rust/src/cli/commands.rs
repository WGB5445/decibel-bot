use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use decibel_grid_tui::process_lock::{SubaccountRunLock, SubaccountStartupLock};
use decibel_grid_tui::*;
use rust_decimal::{Decimal, prelude::ToPrimitive};

use crate::cli::settings::Settings;
use crate::engine::{optional_subaccount, print_snapshot, run_cli};
use crate::tui::USDC_CROSS_DUST;

/// Send this process's stdout and stderr to `path`, replacing any previous contents.
///
/// This replaces file descriptors 1 and 2 (or the Windows standard handles) rather than merely
/// wrapping `println!`, so panic reports and anything a dependency writes directly to those
/// descriptors land in the same file. Rust's stdout is line-buffered even when it is not a
/// terminal, so the file stays readable while a long `run` is still going.
#[cfg(unix)]
pub(crate) fn redirect_output_to_log(path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create log directory {}", parent.display()))?;
    }
    // Truncating open: each run starts from a clean file instead of appending to stale output.
    let file = fs::File::create(path)
        .with_context(|| format!("cannot create log file {}", path.display()))?;
    let fd = file.as_raw_fd();
    for (target, name) in [
        (libc::STDOUT_FILENO, "stdout"),
        (libc::STDERR_FILENO, "stderr"),
    ] {
        // SAFETY: `fd` is a valid descriptor owned by `file` and still open here, and `target`
        // is one of the two standard descriptors, which are always valid dup2 targets.
        if unsafe { libc::dup2(fd, target) } < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("cannot redirect {name} to {}", path.display()));
        }
    }
    // Descriptors 1 and 2 now reference the same open file, so the original handle is redundant.
    drop(file);
    Ok(())
}

#[cfg(windows)]
pub(crate) fn redirect_output_to_log(path: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create log directory {}", parent.display()))?;
    }
    let file = fs::File::create(path)
        .with_context(|| format!("cannot create log file {}", path.display()))?;
    let handle = file.as_raw_handle();

    // SAFETY: `handle` is a valid file handle owned by `file`. It is intentionally leaked so
    // the standard handles remain valid for the lifetime of the process.
    unsafe {
        let ok = windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
            handle as _,
        ) != 0
            && windows_sys::Win32::System::Console::SetStdHandle(
                windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
                handle as _,
            ) != 0;
        if !ok {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("cannot redirect stdout/stderr to {}", path.display()));
        }
    }
    std::mem::forget(file);
    Ok(())
}

pub(crate) fn simulate_cli(scenario_path: Option<&Path>) -> Result<()> {
    let path = scenario_path.context("simulate requires --scenario <path> (YAML or JSON)")?;
    let raw =
        fs::read_to_string(path).with_context(|| format!("read scenario {}", path.display()))?;
    let scenario = decibel_grid_tui::simulation::parse_scenario(&raw)?;
    decibel_grid_tui::simulation::simulate_scenario(&scenario, std::io::stdout())?;
    Ok(())
}

pub async fn check_api_key(settings: Settings) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    let api = DecibelClient::new(&settings.network, &settings.api_key)?;
    api.verify_api_key().await?;
    println!(
        "API key format is valid and the key is accepted by the {} API.",
        settings.network
    );
    Ok(())
}

struct EngineRuntimeGuard {
    paths: control::ControlPaths,
}

impl Drop for EngineRuntimeGuard {
    fn drop(&mut self) {
        self.paths.remove_runtime_files();
    }
}

pub(crate) fn control_paths(settings: &Settings) -> Result<control::ControlPaths> {
    control::ControlPaths::for_subaccount(&settings.subaccount)
}

pub(crate) async fn control_request(
    settings: &Settings,
    request: control::Request,
) -> Result<control::Response> {
    control::request(&control_paths(settings)?, &request).await
}

pub async fn status_client(settings: Settings) -> Result<()> {
    match control_request(&settings, control::Request::Status).await? {
        control::Response::Status { status } => {
            println!("engine pid={} phase={}", status.pid, status.phase);
            println!(
                "{} {} {} {}",
                status.network, status.subaccount, status.product, status.market
            );
            println!(
                "last cycle: {:?}; mid: {:?}",
                status.last_cycle_at, status.mid
            );
            println!(
                "reconciliation: matched={:?} missing={:?} unmanaged={:?}",
                status.matched, status.missing, status.unmanaged
            );
            if let Some(error) = status.last_error {
                println!("last error: {error}");
            }
            Ok(())
        }
        control::Response::Error { message } => {
            anyhow::bail!("engine rejected status request: {message}")
        }
        response => anyhow::bail!("unexpected engine status response: {response:?}"),
    }
}

pub async fn stop_client(
    settings: Settings,
    confirm_mainnet: Option<&str>,
    exit_mode: Option<&str>,
) -> Result<()> {
    if settings.network.eq_ignore_ascii_case("mainnet") && confirm_mainnet != Some("MAINNET") {
        anyhow::bail!("mainnet stop requires --confirm-mainnet MAINNET")
    }
    let mode = match exit_mode.unwrap_or("hold") {
        "hold" => control::ExitMode::Hold,
        "liquidate" => control::ExitMode::Liquidate,
        _ => anyhow::bail!("--exit-mode must be hold or liquidate"),
    };
    match control_request(&settings, control::Request::Stop { exit_mode: mode }).await? {
        control::Response::Accepted { message } => {
            println!("{message}");
            Ok(())
        }
        control::Response::Error { message } => {
            anyhow::bail!("engine rejected stop request: {message}")
        }
        response => anyhow::bail!("unexpected engine stop response: {response:?}"),
    }
}

pub async fn logs_client(settings: Settings, follow: bool) -> Result<()> {
    let paths = control_paths(&settings)?;
    let shown = control::tail_lines(&paths.log, 200)?;
    if !shown.is_empty() {
        println!("{shown}");
    }
    if !follow {
        return Ok(());
    }
    let mut offset = fs::metadata(&paths.log)?.len();
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let bytes = fs::read(&paths.log)?;
        if bytes.len() < offset as usize {
            offset = 0;
        }
        if bytes.len() > offset as usize {
            let appended = String::from_utf8_lossy(&bytes[offset as usize..]);
            print!("{appended}");
            io::stdout().flush()?;
            offset = bytes.len() as u64;
        }
    }
}

pub async fn attach_client(settings: Settings) -> Result<()> {
    let client = decibel_grid_tui::client::EngineClient::for_subaccount(&settings.subaccount)?;
    decibel_grid_tui::attach_tui::run(client).await
}

pub async fn start_cli(settings: Settings, confirm_mainnet: Option<&str>) -> Result<()> {
    if settings.network.eq_ignore_ascii_case("mainnet") && confirm_mainnet != Some("MAINNET") {
        anyhow::bail!("mainnet start requires --confirm-mainnet MAINNET")
    }
    let paths = control_paths(&settings)?;
    paths.ensure_directory()?;
    if let Some(pid) = paths.read_pid()? {
        if control::process_is_alive(pid) {
            anyhow::bail!("engine already running for this account (pid {pid})")
        }
        paths.remove_runtime_files();
    }
    // Preflight and hold the startup lock until the child engine responds. This serializes
    // concurrent `start` invocations without blocking the engine's long-lived run lock.
    let _startup_lock = SubaccountStartupLock::acquire(&settings.network, &settings.subaccount)?;
    let executable = std::env::current_exe().context("resolve grid-bot executable")?;
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let Some(index) = args.iter().position(|arg| arg == "start") else {
        anyhow::bail!("could not rewrite start command for engine child")
    };
    args[index] = "engine".into();
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("launch grid engine child")?;
    for _ in 0..40 {
        if let Ok(status) = child.try_wait() {
            if let Some(status) = status {
                anyhow::bail!(
                    "engine process {} exited before opening its control socket (status: {status}); \
                     inspect {}. If another grid process (engine, shadow, or attach) already holds \
                     the subaccount run lock, stop it first",
                    child.id(),
                    paths.log.display()
                );
            }
        }
        if matches!(
            control::request(&paths, &control::Request::Ping).await,
            Ok(control::Response::Pong)
        ) {
            println!(
                "grid engine started (pid {}); socket {}",
                child.id(),
                paths.socket.display()
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let exit_hint = match child.try_wait() {
        Ok(Some(status)) => format!(" (exited with status: {status})"),
        Ok(None) => String::new(),
        Err(err) => format!(" (could not check exit status: {err})"),
    };
    anyhow::bail!(
        "engine process {} did not open its control socket{exit_hint}; inspect {}. \
         If another grid process (engine, shadow, or attach) already holds the subaccount run lock, stop it first",
        child.id(),
        paths.log.display()
    )
}

pub async fn engine_cli(settings: Settings, confirm_mainnet: Option<&str>) -> Result<()> {
    let _subaccount_lock = SubaccountRunLock::acquire(&settings.network, &settings.subaccount)?;
    let paths = control_paths(&settings)?;
    paths.ensure_directory()?;
    if let Some(pid) = paths.read_pid()? {
        if control::process_is_alive(pid) {
            anyhow::bail!("engine already running for this account (pid {pid})")
        }
        paths.remove_runtime_files();
    }
    let runtime = control::EngineHandle::new(control::EngineStatus {
        pid: std::process::id(),
        started_at: Some(Utc::now()),
        network: settings.network.clone(),
        subaccount: settings.subaccount.clone(),
        market: settings.market.clone(),
        product: format!("{:?}", settings.product).to_lowercase(),
        phase: "starting".to_owned(),
        ..Default::default()
    });
    paths.write_pid(std::process::id())?;
    let _guard = EngineRuntimeGuard {
        paths: paths.clone(),
    };
    let server = control::start_server(&paths, runtime.clone()).await?;
    let result = run_cli(settings, true, confirm_mainnet, Some(runtime.clone())).await;
    runtime
        .update_status(|status| status.phase = "stopped".to_owned())
        .await;
    server.abort();
    result
}

/// Legacy direct status implementation retained for compatibility tests; the public `status`
/// command now queries the running engine's local socket.
#[allow(dead_code)]
async fn status_cli(settings: Settings) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let snapshot = fetch_snapshot(&api, &config, optional_subaccount(&settings)).await?;
    print_snapshot(&snapshot, &config);
    Ok(())
}

/// Legacy direct lifecycle implementation retained for compatibility tests; the public `stop`
/// command now tells the running engine to execute this shutdown flow.
#[allow(dead_code)]
async fn stop_cli(settings: Settings, confirm_mainnet: Option<&str>) -> Result<()> {
    if settings.api_key.trim().is_empty()
        || settings.aptos_private_key.trim().is_empty()
        || settings.subaccount.trim().is_empty()
    {
        anyhow::bail!("stop requires DECIBEL_API_KEY, APTOS_PRIVATE_KEY, and SUBACCOUNT_ADDRESS")
    }
    if settings.network.eq_ignore_ascii_case("mainnet") && confirm_mainnet != Some("MAINNET") {
        anyhow::bail!("mainnet stop requires --confirm-mainnet MAINNET")
    }
    let _lock = SubaccountRunLock::acquire(&settings.network, &settings.subaccount)?;
    let mut config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let market = api.market(&config.market_name, config.product).await?;
    match settings.exit_asset_policy {
        ExitAssetPolicy::Retain => {
            let hash = spot_lifecycle::cancel_bulk_ladder(
                &settings.network,
                &settings.aptos_private_key,
                &settings.subaccount,
                &market,
            )
            .await?;
            println!("Grid stopped: ladder cancelled in tx {hash}; assets retained.");
        }
        ExitAssetPolicy::Sell => {
            let spot_guard = if market.product == Product::Spot {
                let rates = api.spot_fee_rates(&settings.subaccount).await?;
                config.maker_fee_rate = rates.maker_rate;
                Some((config.spot, rates))
            } else {
                None
            };
            let guard_refs = spot_guard.as_ref().map(|(policy, rates)| (policy, rates));
            let hashes = exit_sell_assets(
                &settings.network,
                &settings.api_key,
                &settings.aptos_private_key,
                &settings.subaccount,
                &market,
                guard_refs,
            )
            .await?;
            println!(
                "Grid stopped and liquidation attempted in {} transaction(s): {:?}",
                hashes.len(),
                hashes
            );
        }
    }
    Ok(())
}

/// Explicit, operator-confirmed Cross→PFS transfer. The bot does not attempt to set
/// HOLD_AS_NON_COLLATERAL: that entry function is owner-only, while the bot signer may only have
/// delegated trading/funds permissions. The operator must set the future-settlement flag manually
/// in the Decibel UI/wallet first.
pub async fn spot_funding_setup_cli(
    settings: Settings,
    amount: String,
    metadata: Option<String>,
) -> Result<()> {
    if settings.aptos_private_key.trim().is_empty() {
        anyhow::bail!("spot-funding-setup requires APTOS_PRIVATE_KEY")
    }
    if settings.subaccount.trim().is_empty() {
        anyhow::bail!("spot-funding-setup requires SUBACCOUNT_ADDRESS")
    }
    let metadata = metadata.unwrap_or_else(|| decibel_grid_tui::TESTNET_USDC_METADATA.to_owned());
    let amount_decimal = Decimal::from_str(amount.trim())
        .context("--spot-funding-amount/SPOT_FUNDING_AMOUNT must be a decimal USDC amount")?;
    if amount_decimal < Decimal::ZERO {
        anyhow::bail!("--spot-funding-amount/SPOT_FUNDING_AMOUNT cannot be negative")
    }
    println!(
        "NOTICE: HOLD_AS_NON_COLLATERAL is owner-only and is not submitted by this bot. Set it manually in the Decibel UI/wallet before relying on future Spot proceeds staying in PFS."
    );
    if amount_decimal.is_zero() {
        println!("No transfer amount given (0); skipping the Cross→PFS transfer.");
        return Ok(());
    }
    let raw = (amount_decimal * Decimal::from(1_000_000u64))
        .floor()
        .to_i64()
        .ok_or_else(|| anyhow::anyhow!("--spot-funding-amount is outside the supported range"))?;
    println!("Transferring {amount_decimal} USDC from Cross to PFS...");
    let transfer_tx = decibel_grid_tui::transfer_spot_cross_pfs(
        &settings.network,
        &settings.aptos_private_key,
        &settings.subaccount,
        &metadata,
        -raw,
    )
    .await?;
    println!("  Transfer submitted. tx {transfer_tx}");
    Ok(())
}

/// Verify the prerequisites for a safe Testnet/Mainnet run without modifying Decibel state.
pub async fn doctor_cli(settings: Settings) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    if settings.subaccount.trim().is_empty() {
        anyhow::bail!("doctor requires SUBACCOUNT_ADDRESS")
    }
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    api.verify_api_key()
        .await
        .context("API key verification failed")?;
    let (snapshot, result) = reconcile_snapshot(&api, &config, &settings.subaccount).await?;
    println!(
        "DOCTOR OK — {} {} on {}",
        snapshot.market.name,
        match config.product {
            Product::Spot => "Spot",
            Product::Perp => "Perp",
        },
        settings.network
    );
    println!(
        "  rules: tick={} lot={} min_size={}",
        snapshot.market.tick_size, snapshot.market.lot_size, snapshot.market.min_size
    );
    println!(
        "  plan: {} bid(s), {} ask(s), quote={}, base={}",
        snapshot.plan.bids.len(),
        snapshot.plan.asks.len(),
        snapshot.plan.quote_required,
        snapshot.plan.base_required
    );
    match config.product {
        Product::Spot => {
            let funds = snapshot.account.spot_funds.as_ref().ok_or_else(|| {
                anyhow::anyhow!("spot PFS balances unavailable in account overview")
            })?;
            println!(
                "  PFS: {} {} available, {} {} available",
                funds.available_base(),
                funds.base_symbol,
                funds.available_quote(),
                funds.quote_symbol
            );
            // A bulk replacement also gets credit for whatever is already escrowed in the
            // resting ladder, so report that separately rather than implying it is unusable.
            if funds.base_reserved > Decimal::ZERO || funds.quote_reserved > Decimal::ZERO {
                println!(
                    "  bulk escrow (credited on replacement): {} {}, {} {} → usable {} {} / {} {}",
                    funds.base_reserved,
                    funds.base_symbol,
                    funds.quote_reserved,
                    funds.quote_symbol,
                    funds.available_base_for_bulk(),
                    funds.base_symbol,
                    funds.available_quote_for_bulk(),
                    funds.quote_symbol
                );
            }
            if funds.quote_cross_balance() >= USDC_CROSS_DUST {
                println!(
                    "  note: {} {} sits in Cross and is NOT spendable by spot bulk orders; transfer it into PFS to fund bids.",
                    funds.quote_cross_balance(),
                    funds.quote_symbol
                );
            }
            if funds.available_base_for_bulk() < snapshot.plan.base_required
                || funds.available_quote_for_bulk() < snapshot.plan.quote_required
            {
                println!(
                    "  note: the pinned Spot grid is underfunded; it will not be placed until the missing asset is funded."
                );
            }
        }
        Product::Perp => {
            let margin = snapshot.account.available_margin.ok_or_else(|| {
                anyhow::anyhow!("available Perp margin unavailable in account overview")
            })?;
            let required = snapshot.plan.estimated_margin.unwrap_or(Decimal::ZERO);
            let position = snapshot.account.position.size;
            println!("  margin: available={} estimated={}", margin, required);
            println!("  position: {}", position);
            if let Some(max) = config.max_position {
                println!("  max_position: {}", max);
                if !perp_position_is_safe(position, &snapshot.plan, &config) {
                    anyhow::bail!(
                        "Perp position {position} or worst-case exposure exceeds max_position {max}"
                    )
                }
            }
            if margin < required {
                anyhow::bail!(
                    "estimated Perp margin {} exceeds available {}",
                    required,
                    margin
                )
            }
        }
    }
    println!("  reconciliation: {}", result.summary());
    let blocking = decibel_grid_tui::reconcile::blocking_orders(&result.unmanaged);
    if !blocking.is_empty() {
        println!(
            "  warning: {} standalone order(s) of unprovable ownership will block live bulk replacement.",
            blocking.len()
        );
    } else if !result.unmanaged.is_empty() {
        println!(
            "  note: {} unmanaged level(s) belong to this account's bulk ladder; a new bulk submission replaces them atomically.",
            result.unmanaged.len()
        );
    }
    println!("  result: read-only checks passed; no exchange state changed.");
    Ok(())
}

/// Compare the current desired grid with open orders. This is intentionally read-only: any order
/// not exactly covered by the current plan remains unmanaged until a future client-ID-backed
/// execution ledger can establish ownership safely.
pub async fn reconcile_cli(settings: Settings) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    if settings.subaccount.trim().is_empty() {
        anyhow::bail!("reconcile requires SUBACCOUNT_ADDRESS")
    }
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let (snapshot, result) = reconcile_snapshot(&api, &config, &settings.subaccount).await?;
    print_snapshot(&snapshot, &config);
    println!("RECONCILE-ONLY — {}", result.summary());
    for order in &result.missing {
        println!(
            "  MISSING {} {} @ {}",
            order.side.as_str(),
            format_decimal(order.size, 8),
            format_decimal(order.price, 8)
        );
    }
    for order in &result.unmanaged {
        println!(
            "  UNMANAGED {} {} @ {} (order {})",
            order.side.as_str(),
            format_decimal(order.remaining_size, 8),
            format_decimal(order.price, 8),
            order.order_id
        );
    }
    if result.is_converged() {
        println!("Grid and exchange snapshot converge; no changes were made.");
    } else {
        println!("No changes were made. Unmanaged orders are never cancelled automatically.");
    }
    Ok(())
}

/// Continuous shadow reconciliation: the same loop as `run -e` but never signs or submits.
/// Every cycle fetches a snapshot, reconciles, journals events, and reports drift — without
/// sending any Aptos transaction. Use this as a long-lived dry-run monitor that produces a
/// complete audit trail.
pub async fn shadow_cli(settings: Settings, max_cycles: Option<usize>) -> Result<()> {
    validate_api_key_format(&settings.api_key).context("API key format check failed")?;
    if settings.subaccount.trim().is_empty() {
        anyhow::bail!("shadow requires SUBACCOUNT_ADDRESS")
    }
    if max_cycles == Some(0) {
        anyhow::bail!("shadow --cycles must be at least 1")
    }
    let _subaccount_lock = SubaccountRunLock::acquire(&settings.network, &settings.subaccount)?;
    let config = settings.to_grid_config()?;
    let api = settings.api_client()?;
    let run_id = journal::generate_run_id();
    let journal = journal::Journal::new(&run_id)
        .context("shadow reconciliation requires a writable run journal")?;
    let mut metadata = journal::RunMetadata {
        run_id: run_id.clone(),
        started_at: Utc::now(),
        network: settings.network.clone(),
        subaccount: settings.subaccount.clone(),
        market: config.market_name.clone(),
        product: format!("{:?}", config.product).to_lowercase(),
        config_hash: {
            use sha3::{Digest, Sha3_256};
            hex::encode(Sha3_256::digest(format!("{config:?}")))
        },
        program_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    metadata.fingerprint_subaccount();
    let _ = journal.append(&journal::JournalEvent::RunStart(metadata));
    println!("Shadow reconciliation run {run_id}. No orders will be placed or cancelled.");
    if config.product == Product::Spot {
        println!("Spot: only PFS balances will be used. No automatic Cross→PFS transfer.");
    }
    let mut remaining_cycles = max_cycles.unwrap_or(usize::MAX);
    loop {
        let cycle_start = tokio::time::Instant::now();
        let snapshot = match fetch_snapshot(&api, &config, optional_subaccount(&settings)).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("shadow refresh failed: {e:#}");
                tokio::time::sleep(config.refresh).await;
                continue;
            }
        };
        let mut snapshot = snapshot;
        // Preserve the fixed Spot geometry across refreshes; only clear historical fill markers.
        snapshot.plan = snapshot.plan.executable();
        if let Some(adjustment) = fit_spot_snapshot_to_pfs(&mut snapshot)? {
            println!("Spot funding check: {adjustment}");
        }
        print_snapshot(&snapshot, &config);
        let event = journal::JournalEvent::PlanGenerated {
            at: Utc::now(),
            mid: snapshot.plan.mid.normalize().to_string(),
            bid_levels: snapshot.plan.bids.len(),
            ask_levels: snapshot.plan.asks.len(),
            quote_required: snapshot.plan.quote_required.normalize().to_string(),
            base_required: snapshot.plan.base_required.normalize().to_string(),
        };
        journal.append(&event)?;
        if let Ok(actual) = api
            .open_orders(&settings.subaccount, &snapshot.market)
            .await
        {
            let desired = decibel_grid_tui::reconcile::desired_orders(
                &snapshot.plan,
                snapshot.market.tick_size,
                snapshot.market.lot_size,
            );
            let result = decibel_grid_tui::reconcile::reconcile(
                &desired,
                &actual,
                snapshot.market.tick_size,
                snapshot.market.lot_size,
            );
            println!("SHADOW RECONCILE — {}", result.summary());
            let event = journal::JournalEvent::ReconciliationResult {
                at: Utc::now(),
                matched: result.matched.len(),
                missing: result.missing.len(),
                unmanaged: result.unmanaged.clone(),
                is_converged: result.is_converged(),
            };
            journal.append(&event)?;
            let blocking = decibel_grid_tui::reconcile::blocking_orders(&result.unmanaged);
            if !blocking.is_empty() {
                println!(
                    "  {} standalone order(s) of unprovable ownership detected. Bulk replacement would be blocked until operator review.",
                    blocking.len()
                );
            } else if !result.unmanaged.is_empty() {
                println!(
                    "  {} unmanaged level(s) belong to this account's bulk ladder; a new bulk submission would replace them atomically.",
                    result.unmanaged.len()
                );
            }
            remaining_cycles = remaining_cycles.saturating_sub(1);
            if remaining_cycles == 0 {
                journal.append(&journal::JournalEvent::Shutdown {
                    at: Utc::now(),
                    reason: "requested shadow cycle limit reached".to_owned(),
                })?;
                println!("Shadow cycle limit reached. No orders were placed or cancelled.");
                return Ok(());
            }
        }
        let elapsed = cycle_start.elapsed();
        let wait = config.refresh.saturating_sub(elapsed);
        tokio::time::sleep(wait).await;
    }
}
