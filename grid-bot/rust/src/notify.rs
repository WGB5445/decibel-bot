//! Best-effort operator alerts and authenticated Telegram control.
//!
//! Neither delivery nor remote-control transport participates in trading decisions. Commands only
//! request the existing local engine shutdown paths, which retain their normal lifecycle guards.

use std::{collections::BTreeSet, time::Duration};

use crate::control::{EngineHandle, EngineStatus, ExitMode};
use futures_util::future::join_all;
use reqwest::Client;

#[derive(Clone, Debug, Default)]
pub struct NotificationConfig {
    pub discord_webhook_url: Option<String>,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub telegram_allowed_user_ids: BTreeSet<i64>,
    pub discord_status_url: Option<String>,
}

#[derive(Clone)]
pub struct Notifier {
    client: Client,
    config: NotificationConfig,
}

impl Notifier {
    pub fn new(config: NotificationConfig) -> Option<Self> {
        if config.discord_webhook_url.is_none()
            && (config.telegram_bot_token.is_none() || config.telegram_chat_id.is_none())
        {
            return None;
        }
        Some(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client configuration is valid"),
            config,
        })
    }

    pub async fn send(&self, title: &str, body: &str) {
        let message = match &self.config.discord_status_url {
            Some(url) => format!("{title}\n{body}\nStatus: {url}"),
            None => format!("{title}\n{body}"),
        };
        let mut deliveries = Vec::new();
        if let Some(url) = &self.config.discord_webhook_url {
            deliveries.push(
                self.client
                    .post(url)
                    .json(&serde_json::json!({ "content": message }))
                    .send(),
            );
        }
        if let (Some(token), Some(chat_id)) = (
            &self.config.telegram_bot_token,
            &self.config.telegram_chat_id,
        ) {
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            deliveries.push(
                self.client
                    .post(url)
                    .json(&serde_json::json!({ "chat_id": chat_id, "text": message }))
                    .send(),
            );
        }
        for result in join_all(deliveries).await {
            if let Err(error) = result.and_then(|response| response.error_for_status()) {
                eprintln!("operator notification failed: {error}");
            }
        }
    }

    pub fn spawn_telegram_controller(&self, runtime: EngineHandle) {
        let Some(token) = self.config.telegram_bot_token.clone() else {
            return;
        };
        if self.config.telegram_allowed_user_ids.is_empty() {
            eprintln!(
                "Telegram alerts enabled without TELEGRAM_ALLOWED_USER_IDS; remote commands are disabled"
            );
            return;
        }
        let client = self.client.clone();
        let allowed_user_ids = self.config.telegram_allowed_user_ids.clone();
        tokio::spawn(async move {
            let mut offset = 0i64;
            while !runtime.is_cancelled() {
                match poll_telegram_commands(&client, &token, offset).await {
                    Ok((next_offset, commands)) => {
                        offset = offset.max(next_offset);
                        for command in commands {
                            if !allowed_user_ids.contains(&command.user_id) {
                                eprintln!(
                                    "ignored Telegram command from unauthorized user {}",
                                    command.user_id
                                );
                                continue;
                            }
                            let reply = apply_telegram_command(&runtime, &command.text).await;
                            if let Err(error) = send_telegram_message(
                                &client,
                                &token,
                                &command.chat_id.to_string(),
                                &reply,
                            )
                            .await
                            {
                                eprintln!("Telegram command response failed: {error}");
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("Telegram command polling failed: {error}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }
}

#[derive(Debug)]
struct TelegramCommand {
    user_id: i64,
    chat_id: i64,
    text: String,
}

async fn poll_telegram_commands(
    client: &Client,
    token: &str,
    offset: i64,
) -> Result<(i64, Vec<TelegramCommand>), reqwest::Error> {
    let response = client
        .get(format!("https://api.telegram.org/bot{token}/getUpdates"))
        .query(&[("timeout", "25"), ("offset", &offset.to_string())])
        .send()
        .await?
        .error_for_status()?;
    let payload: serde_json::Value = response.json().await?;
    let mut next_offset = offset;
    let mut commands = Vec::new();
    for update in payload["result"].as_array().into_iter().flatten() {
        let Some(update_id) = update["update_id"].as_i64() else {
            continue;
        };
        next_offset = next_offset.max(update_id.saturating_add(1));
        let message = &update["message"];
        let (Some(user_id), Some(chat_id), Some(text)) = (
            message["from"]["id"].as_i64(),
            message["chat"]["id"].as_i64(),
            message["text"].as_str(),
        ) else {
            continue;
        };
        commands.push(TelegramCommand {
            user_id,
            chat_id,
            text: text.to_owned(),
        });
    }
    Ok((next_offset, commands))
}

async fn apply_telegram_command(runtime: &EngineHandle, text: &str) -> String {
    let command = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match command.as_str() {
        "/status" => format_engine_status(&runtime.status().await),
        "/stop" if text.split_whitespace().nth(1) == Some("CONFIRM") => {
            runtime.request_stop(ExitMode::Hold);
            "Stop accepted. The engine will cancel its ladder and retain the current position."
                .to_owned()
        }
        "/flatten" if text.split_whitespace().nth(1) == Some("CONFIRM") => {
            runtime.request_stop(ExitMode::Liquidate);
            "Flatten accepted. The engine will cancel its ladder, then use its existing guarded Perp close path."
                .to_owned()
        }
        "/stop" => "Usage: /stop CONFIRM\nCancels the ladder and retains the position.".to_owned(),
        "/flatten" => {
            "Usage: /flatten CONFIRM\nCancels the ladder and closes through the guarded Perp close path."
                .to_owned()
        }
        _ => "Commands:\n/status\n/stop CONFIRM\n/flatten CONFIRM".to_owned(),
    }
}

fn format_engine_status(status: &EngineStatus) -> String {
    format!(
        "Decibel grid status\nphase: {}\nmarket: {} {}\nposition: {}\ntarget: {}\nreconciliation: matched={} missing={} unmanaged={}\nlast error: {}",
        status.phase,
        status.product,
        status.market,
        status.position.as_deref().unwrap_or("unavailable"),
        status.target_position.as_deref().unwrap_or("unavailable"),
        status
            .matched
            .map_or_else(|| "-".to_owned(), |value| value.to_string()),
        status
            .missing
            .map_or_else(|| "-".to_owned(), |value| value.to_string()),
        status
            .unmanaged
            .map_or_else(|| "-".to_owned(), |value| value.to_string()),
        status.last_error.as_deref().unwrap_or("none"),
    )
}

async fn send_telegram_message(
    client: &Client,
    token: &str,
    chat_id: &str,
    text: &str,
) -> Result<(), reqwest::Error> {
    client
        .post(format!("https://api.telegram.org/bot{token}/sendMessage"))
        .json(&serde_json::json!({ "chat_id": chat_id, "text": text }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{NotificationConfig, Notifier, apply_telegram_command, format_engine_status};
    use crate::control::{EngineHandle, EngineStatus, ExitMode};

    #[test]
    fn notifier_requires_a_complete_channel() {
        assert!(Notifier::new(NotificationConfig::default()).is_none());
        assert!(
            Notifier::new(NotificationConfig {
                discord_webhook_url: Some("https://discord.invalid/webhook".to_owned()),
                ..NotificationConfig::default()
            })
            .is_some()
        );
        assert!(
            Notifier::new(NotificationConfig {
                telegram_bot_token: Some("token".to_owned()),
                telegram_chat_id: Some("chat".to_owned()),
                ..NotificationConfig::default()
            })
            .is_some()
        );
    }

    #[test]
    fn formatted_status_does_not_require_optional_fields() {
        let status = EngineStatus {
            phase: "degraded".to_owned(),
            market: "BTC/USD".to_owned(),
            product: "perp".to_owned(),
            ..EngineStatus::default()
        };
        assert!(format_engine_status(&status).contains("phase: degraded"));
    }

    #[tokio::test]
    async fn flatten_command_requires_confirmation_and_uses_guarded_exit_mode() {
        let runtime = EngineHandle::new(EngineStatus::default());
        assert!(
            apply_telegram_command(&runtime, "/flatten")
                .await
                .contains("Usage")
        );
        assert!(!runtime.is_cancelled());

        assert!(
            apply_telegram_command(&runtime, "/flatten CONFIRM")
                .await
                .contains("Flatten accepted")
        );
        assert!(runtime.is_cancelled());
        assert_eq!(runtime.requested_exit_mode(), Some(ExitMode::Liquidate));
    }
}
