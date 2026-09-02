//! Typed client for the local grid-engine control socket. No terminal rendering belongs here.

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use crate::control::{ControlPaths, EngineStatus, ExitMode, Request, Response};

#[derive(Clone, Debug)]
pub struct EngineClient {
    paths: ControlPaths,
}

impl EngineClient {
    pub fn for_subaccount(subaccount: &str) -> Result<Self> {
        Ok(Self {
            paths: ControlPaths::for_subaccount(subaccount)?,
        })
    }

    pub async fn get_status(&self) -> Result<EngineStatus> {
        match crate::control::request(&self.paths, &Request::Status).await? {
            Response::Status { status } => Ok(*status),
            Response::Error { message } => bail!("engine rejected status request: {message}"),
            response => bail!("unexpected engine response: {response:?}"),
        }
    }

    pub async fn send_command(&self, command: ClientCommand) -> Result<String> {
        let request = match command {
            ClientCommand::Stop { exit_mode } => Request::Stop { exit_mode },
        };
        match crate::control::request(&self.paths, &request).await? {
            Response::Accepted { message } => Ok(message),
            Response::Error { message } => bail!("engine rejected command: {message}"),
            response => bail!("unexpected engine response: {response:?}"),
        }
    }

    /// Returns a typed status stream backed by one long-lived Unix-socket subscription.
    #[cfg(unix)]
    pub async fn subscribe_updates(&self) -> Result<mpsc::Receiver<Result<EngineStatus>>> {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixStream,
        };
        let stream = UnixStream::connect(&self.paths.socket)
            .await
            .with_context(|| {
                format!("connect to grid engine at {}", self.paths.socket.display())
            })?;
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(format!("{}\n", serde_json::to_string(&Request::Subscribe)?).as_bytes())
            .await?;
        writer.flush().await?;
        let (sender, receiver) = mpsc::channel(16);
        tokio::spawn(async move {
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

#[derive(Clone, Copy, Debug)]
pub enum ClientCommand {
    Stop { exit_mode: ExitMode },
}
