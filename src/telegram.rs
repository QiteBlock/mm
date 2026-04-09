use std::sync::Arc;

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::TelegramConfig;

#[derive(Clone, Debug)]
pub enum TelegramCommand {
    Start,
    Stop,
    Restart,
}

#[derive(Clone, Default)]
pub struct TelegramNotifier {
    inner: Option<Arc<TelegramInner>>,
}

#[derive(Clone)]
struct TelegramInner {
    client: Client,
    bot_token: String,
    chat_id: String,
}

impl TelegramNotifier {
    pub fn from_config(config: Option<TelegramConfig>) -> Self {
        match config {
            Some(cfg) if cfg.enabled => Self {
                inner: Some(Arc::new(TelegramInner {
                    client: Client::new(),
                    bot_token: cfg.bot_token,
                    chat_id: cfg.chat_id,
                })),
            },
            _ => Self::default(),
        }
    }

    pub async fn send(&self, message: impl Into<String>) {
        let Some(inner) = &self.inner else {
            return;
        };

        let request = TelegramMessage {
            chat_id: inner.chat_id.clone(),
            text: message.into(),
        };

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            inner.bot_token
        );
        match inner.client.post(url).json(&request).send().await {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() {
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "<failed to read body>".to_string());
                    warn!(%status, %body, "telegram alert rejected");
                }
            }
            Err(err) => {
                warn!(?err, "telegram alert failed");
            }
        }
    }

    pub async fn poll_commands(
        &self,
        offset: Option<i64>,
        timeout_seconds: u64,
    ) -> Result<Vec<(i64, TelegramCommand)>> {
        let Some(inner) = &self.inner else {
            return Ok(Vec::new());
        };

        let url = format!("https://api.telegram.org/bot{}/getUpdates", inner.bot_token);
        let response = inner
            .client
            .post(url)
            .json(&TelegramGetUpdatesRequest {
                offset,
                timeout: timeout_seconds,
                allowed_updates: vec!["message".to_string()],
            })
            .send()
            .await?;
        let body: TelegramGetUpdatesResponse = response.json().await?;
        if !body.ok {
            return Ok(Vec::new());
        }

        let mut commands = Vec::new();
        let expected_chat_id = match inner.chat_id.parse::<i64>() {
            Ok(value) => value,
            Err(_) => return Ok(Vec::new()),
        };
        for update in body.result {
            let Some(message) = update.message else {
                continue;
            };
            if message.chat.id != expected_chat_id {
                continue;
            }
            let Some(text) = message.text else {
                continue;
            };
            let Some(command) = parse_command(&text) else {
                continue;
            };
            info!(chat_id = message.chat.id, text = %text, "received telegram command");
            commands.push((update.update_id, command));
        }

        Ok(commands)
    }
}

#[derive(Serialize)]
struct TelegramMessage {
    chat_id: String,
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TelegramGetUpdatesRequest {
    offset: Option<i64>,
    timeout: u64,
    allowed_updates: Vec<String>,
}

#[derive(Deserialize)]
struct TelegramGetUpdatesResponse {
    ok: bool,
    result: Vec<TelegramUpdate>,
}

#[derive(Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramIncomingMessage>,
}

#[derive(Deserialize)]
struct TelegramIncomingMessage {
    text: Option<String>,
    chat: TelegramIncomingChat,
}

#[derive(Deserialize)]
struct TelegramIncomingChat {
    id: i64,
}

fn parse_command(text: &str) -> Option<TelegramCommand> {
    let command = text.split_whitespace().next()?.trim().split('@').next()?;
    match command {
        "/start" => Some(TelegramCommand::Start),
        "/stop" => Some(TelegramCommand::Stop),
        "/restart" => Some(TelegramCommand::Restart),
        _ => None,
    }
}
