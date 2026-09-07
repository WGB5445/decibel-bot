//! Typed client for the local grid-engine control socket. No terminal rendering belongs here.

use std::path::Path;

use anyhow::{Result, bail};
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

    /// Returns a typed status stream backed by one long-lived local control subscription.
    pub async fn subscribe_updates(&self) -> Result<mpsc::Receiver<Result<EngineStatus>>> {
        crate::control::subscribe(&self.paths).await
    }

    pub fn log_path(&self) -> &Path {
        &self.paths.log
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ClientCommand {
    Stop { exit_mode: ExitMode },
}
