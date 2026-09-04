//! Local control plane for one running grid engine.
//!
//! The protocol intentionally stays small: one newline-delimited JSON request and response. It
//! never carries credentials or signed transaction data.
//!
//! Transport is a Unix-domain socket on Unix and a local named pipe on Windows. PID/log files live
//! next to that endpoint (`/tmp/grid-bot` or `%TEMP%\grid-bot`).

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast, mpsc};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitMode {
    Hold,
    Liquidate,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    /// Keep this connection open: send one snapshot, then broadcast state changes.
    Subscribe,
    Stop {
        exit_mode: ExitMode,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Status { status: Box<EngineStatus> },
    Update { status: Box<EngineStatus> },
    Accepted { message: String },
    Error { message: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LadderLevel {
    pub side: String,
    pub price: String,
    pub size: String,
    pub state: String,
}

/// Build attach/TUI ladder rows from the executable grid plan for this cycle.
pub fn ladder_from_plan(plan: &crate::GridPlan) -> Vec<LadderLevel> {
    plan.all_levels()
        .map(|level| LadderLevel {
            side: format!("{:?}", level.side),
            price: level.price.to_string(),
            size: level.size.to_string(),
            state: format!("{:?}", level.state),
        })
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EngineEvent {
    pub at: DateTime<Utc>,
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EngineStatus {
    pub pid: u32,
    pub started_at: Option<DateTime<Utc>>,
    pub network: String,
    pub subaccount: String,
    pub market: String,
    pub product: String,
    pub phase: String,
    pub last_cycle_at: Option<DateTime<Utc>>,
    pub mid: Option<String>,
    pub matched: Option<usize>,
    pub missing: Option<usize>,
    pub unmanaged: Option<usize>,
    pub last_error: Option<String>,
    pub pfs_base_symbol: Option<String>,
    pub pfs_base_balance: Option<String>,
    pub pfs_quote_symbol: Option<String>,
    pub pfs_quote_balance: Option<String>,
    pub realized_pnl: Option<String>,
    pub perp_mode: Option<String>,
    pub max_position: Option<String>,
    pub position: Option<String>,
    pub available_margin: Option<String>,
    pub estimated_margin: Option<String>,
    pub planning_price: Option<String>,
    pub target_position: Option<String>,
    pub convergence_delta: Option<String>,
    pub worst_long: Option<String>,
    pub worst_short: Option<String>,
    pub perp_blocked_reason: Option<String>,
    pub out_of_range_action: Option<String>,
    pub paused_by_out_of_range: bool,
    /// Absolute path of this engine process's stdout/stderr log file, when logging is enabled.
    pub log_path: Option<String>,
    pub ladder: Vec<LadderLevel>,
    pub events: Vec<EngineEvent>,
}

#[derive(Clone)]
pub struct EngineHandle {
    cancel: Arc<AtomicBool>,
    stop_mode: Arc<Mutex<Option<ExitMode>>>,
    status: Arc<RwLock<EngineStatus>>,
    updates: broadcast::Sender<EngineStatus>,
}

impl EngineHandle {
    pub fn new(status: EngineStatus) -> Self {
        let (updates, _) = broadcast::channel(256);
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            stop_mode: Arc::new(Mutex::new(None)),
            status: Arc::new(RwLock::new(status)),
            updates,
        }
    }

    pub fn cancel(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn request_stop(&self, exit_mode: ExitMode) {
        *self.stop_mode.lock().expect("stop mode mutex poisoned") = Some(exit_mode);
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn requested_exit_mode(&self) -> Option<ExitMode> {
        *self.stop_mode.lock().expect("stop mode mutex poisoned")
    }

    pub async fn status(&self) -> EngineStatus {
        self.status.read().await.clone()
    }

    pub async fn update_status(&self, update: impl FnOnce(&mut EngineStatus)) {
        let mut status = self.status.write().await;
        update(&mut status);
        // A slow or disconnected subscriber may miss intermediate updates; it receives the
        // latest complete status on the next broadcast or reconnect snapshot.
        let _ = self.updates.send(status.clone());
    }

    fn subscribe(&self) -> broadcast::Receiver<EngineStatus> {
        self.updates.subscribe()
    }
}

fn control_directory() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/tmp/grid-bot")
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join("grid-bot")
    }
}

fn control_endpoint(directory: &Path, account: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let _ = directory;
        PathBuf::from(format!(r"\\.\pipe\grid-bot-{account}"))
    }
    #[cfg(not(windows))]
    {
        directory.join(format!("{account}.sock"))
    }
}

#[derive(Clone, Debug)]
pub struct ControlPaths {
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub pid: PathBuf,
    pub log: PathBuf,
}

impl ControlPaths {
    /// Socket names deliberately key only on the normalized subaccount, as requested: one
    /// account must never have two engines independently issuing bulk sequence numbers.
    pub fn for_subaccount(subaccount: &str) -> Result<Self> {
        let account = subaccount
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches('0')
            .to_ascii_lowercase();
        if account.is_empty() || !account.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("SUBACCOUNT_ADDRESS must be a non-empty hexadecimal Aptos address")
        }
        let directory = control_directory();
        Ok(Self {
            socket: control_endpoint(&directory, &account),
            pid: directory.join(format!("{account}.pid")),
            log: directory.join(format!("{account}.log")),
            directory,
        })
    }

    pub fn ensure_directory(&self) -> Result<()> {
        fs::create_dir_all(&self.directory)
            .with_context(|| format!("create control directory {}", self.directory.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700)).with_context(
                || format!("protect control directory {}", self.directory.display()),
            )?;
        }
        Ok(())
    }

    pub fn read_pid(&self) -> Result<Option<u32>> {
        match fs::read_to_string(&self.pid) {
            Ok(raw) => Ok(Some(raw.trim().parse().with_context(|| {
                format!("invalid engine pid file {}", self.pid.display())
            })?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("read pid file {}", self.pid.display()))
            }
        }
    }

    pub fn remove_runtime_files(&self) {
        // Named pipes are not filesystem objects; only the Unix socket path should be unlinked.
        #[cfg(not(windows))]
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.pid);
    }

    pub fn write_pid(&self, pid: u32) -> Result<()> {
        self.ensure_directory()?;
        fs::write(&self.pid, format!("{pid}\n"))
            .with_context(|| format!("write pid file {}", self.pid.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.pid, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("protect pid file {}", self.pid.display()))?;
        }
        Ok(())
    }
}

mod protocol {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

    const MAX_LINE_BYTES: usize = 16 * 1024;

    pub async fn handle_client<R, W>(reader: R, mut writer: W, handle: EngineHandle) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .await
            .context("read control request")?;
        if bytes > 0
            && matches!(
                serde_json::from_str(line.trim_end()),
                Ok(Request::Subscribe)
            )
        {
            return stream_updates(writer, handle).await;
        }
        let response = if bytes == 0 {
            Response::Error {
                message: "empty control request".to_owned(),
            }
        } else if bytes > MAX_LINE_BYTES {
            Response::Error {
                message: "control request exceeds 16 KiB".to_owned(),
            }
        } else {
            match serde_json::from_str::<Request>(line.trim_end()) {
                Ok(Request::Ping) => Response::Pong,
                Ok(Request::Status) => Response::Status {
                    status: Box::new(handle.status().await),
                },
                Ok(Request::Subscribe) => Response::Error {
                    message: "subscribe request was not upgraded".to_owned(),
                },
                Ok(Request::Stop { exit_mode }) => {
                    handle.request_stop(exit_mode);
                    Response::Accepted {
                        message: "graceful shutdown requested".to_owned(),
                    }
                }
                Err(error) => Response::Error {
                    message: format!("invalid control request: {error}"),
                },
            }
        };
        write_line(&mut writer, &response)
            .await
            .context("write control response")?;
        writer.shutdown().await.context("close control response")?;
        Ok(())
    }

    async fn stream_updates<W>(mut writer: W, handle: EngineHandle) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let mut updates = handle.subscribe();
        write_line(
            &mut writer,
            &Response::Status {
                status: Box::new(handle.status().await),
            },
        )
        .await?;
        loop {
            match updates.recv().await {
                Ok(status) => {
                    write_line(
                        &mut writer,
                        &Response::Update {
                            status: Box::new(status),
                        },
                    )
                    .await?
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    write_line(
                        &mut writer,
                        &Response::Update {
                            status: Box::new(handle.status().await),
                        },
                    )
                    .await?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        Ok(())
    }

    async fn write_line<W>(writer: &mut W, response: &Response) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        writer
            .write_all(format!("{}\n", serde_json::to_string(response)?).as_bytes())
            .await
            .context("write status update")?;
        writer.flush().await.context("flush status update")?;
        Ok(())
    }

    pub async fn exchange_oneshot<R, W>(
        mut writer: W,
        reader: R,
        request: &Request,
    ) -> Result<Response>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        writer
            .write_all(format!("{}\n", serde_json::to_string(request)?).as_bytes())
            .await
            .context("write control request")?;
        writer.shutdown().await.context("close control request")?;
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .await
            .context("read control response")?;
        if bytes == 0 || bytes > MAX_LINE_BYTES {
            bail!("engine returned an empty or oversized control response")
        }
        serde_json::from_str(line.trim_end()).context("decode control response")
    }

    pub async fn spawn_subscribe_stream<R, W>(
        mut writer: W,
        reader: R,
    ) -> Result<mpsc::Receiver<Result<EngineStatus>>>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        writer
            .write_all(format!("{}\n", serde_json::to_string(&Request::Subscribe)?).as_bytes())
            .await
            .context("write subscribe request")?;
        writer.flush().await.context("flush subscribe request")?;
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(async move {
            let _writer = writer;
            let mut reader = BufReader::new(reader);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => match serde_json::from_str::<Response>(line.trim_end()) {
                        Ok(Response::Status { status } | Response::Update { status }) => {
                            if sender.send(Ok(*status)).await.is_err() {
                                break;
                            }
                        }
                        Ok(Response::Error { message }) => {
                            let _ = sender.send(Err(anyhow::anyhow!(message))).await;
                            break;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = sender.send(Err(anyhow::Error::from(error))).await;
                            break;
                        }
                    },
                    Err(error) => {
                        let _ = sender.send(Err(anyhow::Error::from(error))).await;
                        break;
                    }
                }
            }
        });
        Ok(receiver)
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use tokio::{
        net::{UnixListener, UnixStream},
        task::JoinHandle,
    };

    pub async fn start_server(
        paths: &ControlPaths,
        handle: EngineHandle,
    ) -> Result<JoinHandle<Result<()>>> {
        paths.ensure_directory()?;
        let _ = fs::remove_file(&paths.socket);
        let listener = UnixListener::bind(&paths.socket)
            .with_context(|| format!("bind control socket {}", paths.socket.display()))?;
        let socket = paths.socket.clone();
        Ok(tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.context("accept control socket client")?;
                        let client_handle = handle.clone();
                        tokio::spawn(async move {
                            let (reader, writer) = stream.into_split();
                            let _ = protocol::handle_client(reader, writer, client_handle).await;
                        });
                    }
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        if handle.is_cancelled() {
                            break;
                        }
                    }
                }
            }
            let _ = fs::remove_file(&socket);
            Ok(())
        }))
    }

    pub async fn request(paths: &ControlPaths, request: &Request) -> Result<Response> {
        let stream = UnixStream::connect(&paths.socket)
            .await
            .with_context(|| format!("connect to grid engine at {}", paths.socket.display()))?;
        let (reader, writer) = stream.into_split();
        protocol::exchange_oneshot(writer, reader, request).await
    }

    pub async fn subscribe(paths: &ControlPaths) -> Result<mpsc::Receiver<Result<EngineStatus>>> {
        let stream = UnixStream::connect(&paths.socket)
            .await
            .with_context(|| format!("connect to grid engine at {}", paths.socket.display()))?;
        let (reader, writer) = stream.into_split();
        protocol::spawn_subscribe_stream(writer, reader).await
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use tokio::{
        io::split,
        net::windows::named_pipe::{ClientOptions, NamedPipeClient, ServerOptions},
        task::JoinHandle,
    };

    const ERROR_PIPE_BUSY: i32 = 231;

    pub async fn start_server(
        paths: &ControlPaths,
        handle: EngineHandle,
    ) -> Result<JoinHandle<Result<()>>> {
        paths.ensure_directory()?;
        let pipe_name = paths.socket.clone();
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&pipe_name)
            .with_context(|| format!("bind control pipe {}", pipe_name.display()))?;
        Ok(tokio::spawn(async move {
            loop {
                tokio::select! {
                    connected = server.connect() => {
                        connected.context("accept control pipe client")?;
                        let client = server;
                        server = ServerOptions::new()
                            .reject_remote_clients(true)
                            .create(&pipe_name)
                            .context("create next control pipe instance")?;
                        let client_handle = handle.clone();
                        tokio::spawn(async move {
                            let (reader, writer) = split(client);
                            let _ = protocol::handle_client(reader, writer, client_handle).await;
                        });
                    }
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        if handle.is_cancelled() {
                            break;
                        }
                    }
                }
            }
            Ok(())
        }))
    }

    async fn connect(paths: &ControlPaths) -> Result<NamedPipeClient> {
        let mut last_busy = None;
        for _ in 0..50 {
            match ClientOptions::new().open(&paths.socket) {
                Ok(client) => return Ok(client),
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    last_busy = Some(error);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("connect to grid engine at {}", paths.socket.display())
                    });
                }
            }
        }
        Err(last_busy.expect("pipe busy retry exhausted without an error"))
            .with_context(|| format!("connect to grid engine at {}", paths.socket.display()))
    }

    pub async fn request(paths: &ControlPaths, request: &Request) -> Result<Response> {
        let stream = connect(paths).await?;
        let (reader, writer) = split(stream);
        protocol::exchange_oneshot(writer, reader, request).await
    }

    pub async fn subscribe(paths: &ControlPaths) -> Result<mpsc::Receiver<Result<EngineStatus>>> {
        let stream = connect(paths).await?;
        let (reader, writer) = split(stream);
        protocol::spawn_subscribe_stream(writer, reader).await
    }
}

#[cfg(unix)]
pub use unix::{request, start_server, subscribe};

#[cfg(windows)]
pub use windows::{request, start_server, subscribe};

#[cfg(not(any(unix, windows)))]
pub async fn request(_paths: &ControlPaths, _request: &Request) -> Result<Response> {
    Err(anyhow!(
        "the local grid control plane requires Unix-domain sockets or Windows named pipes"
    ))
}

#[cfg(not(any(unix, windows)))]
pub async fn start_server(
    _paths: &ControlPaths,
    _handle: EngineHandle,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    Err(anyhow!(
        "the local grid control plane requires Unix-domain sockets or Windows named pipes"
    ))
}

#[cfg(not(any(unix, windows)))]
pub async fn subscribe(_paths: &ControlPaths) -> Result<mpsc::Receiver<Result<EngineStatus>>> {
    Err(anyhow!(
        "the local grid control plane requires Unix-domain sockets or Windows named pipes"
    ))
}

pub fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

pub fn tail_lines(path: &Path, limit: usize) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content
            .lines()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("engine log {} does not exist", path.display())
        }
        Err(error) => {
            Err(anyhow!(error)).with_context(|| format!("read engine log {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broadcasts_each_state_change_to_multiple_subscribers() {
        let handle = EngineHandle::new(EngineStatus::default());
        let mut first = handle.subscribe();
        let mut second = handle.subscribe();
        handle
            .update_status(|status| status.phase = "running".to_owned())
            .await;
        assert_eq!(first.recv().await.unwrap().phase, "running");
        assert_eq!(second.recv().await.unwrap().phase, "running");
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn status_and_stop_use_single_line_json_protocol() {
        let root =
            std::env::temp_dir().join(format!("decibel-grid-control-test-{}", std::process::id()));
        let socket = {
            #[cfg(unix)]
            {
                root.join("engine.sock")
            }
            #[cfg(windows)]
            {
                PathBuf::from(format!(
                    r"\\.\pipe\decibel-grid-control-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("system time")
                        .as_nanos()
                ))
            }
        };
        let paths = ControlPaths {
            directory: root.clone(),
            socket,
            pid: root.join("engine.pid"),
            log: root.join("engine.log"),
        };
        let handle = EngineHandle::new(EngineStatus {
            pid: 42,
            phase: "running".to_owned(),
            ..Default::default()
        });
        let server = start_server(&paths, handle.clone()).await.unwrap();
        match request(&paths, &Request::Status).await.unwrap() {
            Response::Status { status } => assert_eq!(status.pid, 42),
            response => panic!("unexpected response: {response:?}"),
        }
        match request(
            &paths,
            &Request::Stop {
                exit_mode: ExitMode::Hold,
            },
        )
        .await
        .unwrap()
        {
            Response::Accepted { .. } => {}
            response => panic!("unexpected response: {response:?}"),
        }
        assert!(handle.is_cancelled());
        server.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn control_paths_key_only_on_subaccount() {
        let paths = ControlPaths::for_subaccount("0x0abc").unwrap();
        assert!(paths.pid.ends_with("abc.pid"));
        #[cfg(unix)]
        assert!(paths.socket.ends_with("abc.sock"));
        #[cfg(windows)]
        assert_eq!(paths.socket.to_string_lossy(), r"\\.\pipe\grid-bot-abc");
    }

    #[test]
    fn engine_status_round_trips_perp_fields() {
        let status = EngineStatus {
            perp_mode: Some("long".to_owned()),
            max_position: Some("0.01".to_owned()),
            position: Some("0.002".to_owned()),
            available_margin: Some("120".to_owned()),
            estimated_margin: Some("500".to_owned()),
            ..Default::default()
        };
        let encoded = serde_json::to_string(&status).unwrap();
        let decoded: EngineStatus = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.perp_mode, status.perp_mode);
        assert_eq!(decoded.max_position, status.max_position);
        assert_eq!(decoded.position, status.position);
        assert_eq!(decoded.available_margin, status.available_margin);
        assert_eq!(decoded.estimated_margin, status.estimated_margin);
    }

    #[test]
    fn ladder_from_plan_formats_bid_and_ask_levels() {
        use crate::{GridLevel, GridPlan, LevelState, Side};
        use rust_decimal_macros::dec;

        let plan = GridPlan {
            mid: dec!(100),
            lower: dec!(90),
            upper: dec!(110),
            per_grid_base_size: None,
            bids: vec![GridLevel {
                side: Side::Bid,
                price: dec!(99),
                size: dec!(0.5),
                notional: dec!(49.5),
                state: LevelState::Planned,
            }],
            asks: vec![GridLevel {
                side: Side::Ask,
                price: dec!(101),
                size: dec!(0.25),
                notional: dec!(25.25),
                state: LevelState::Selected,
            }],
            quote_required: dec!(49.5),
            base_required: dec!(0.25),
            estimated_margin: None,
            ..Default::default()
        };

        let ladder = ladder_from_plan(&plan);
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[0].side, "Bid");
        assert_eq!(ladder[0].price, "99");
        assert_eq!(ladder[0].size, "0.5");
        assert_eq!(ladder[0].state, "Planned");
        assert_eq!(ladder[1].side, "Ask");
        assert_eq!(ladder[1].price, "101");
        assert_eq!(ladder[1].size, "0.25");
        assert_eq!(ladder[1].state, "Selected");
    }
}
