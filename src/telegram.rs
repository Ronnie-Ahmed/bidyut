//! Minimal Telegram Bot API client (long polling): only what the bot needs.
//!
//! The bot token is part of every request URL. Error messages therefore have
//! the URL stripped (`without_url`), and this type has no Debug implementation.

use std::{fmt, time::Duration};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    pub location: Option<Location>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub message: Option<Message>,
    pub data: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Deserialize)]
struct BotInfo {
    username: Option<String>,
}

#[derive(Deserialize)]
struct ResponseParameters {
    retry_after: Option<u64>,
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<i64>,
    parameters: Option<ResponseParameters>,
}

#[derive(Debug)]
pub enum CallError {
    Transport(String),
    Api {
        code: i64,
        description: String,
        retry_after: Option<u64>,
    },
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Transport(msg) => write!(f, "network error: {msg}"),
            CallError::Api {
                code,
                description,
                retry_after,
            } => {
                write!(f, "telegram error {code}: {description}")?;
                if let Some(secs) = retry_after {
                    write!(f, " (retry after {secs}s)")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// The user blocked the bot or the chat no longer exists: stop trying.
    Blocked,
    /// Worth retrying later.
    Failed(String),
}

pub struct Telegram {
    http: reqwest::Client,
    base: String,
}

impl Telegram {
    pub fn new(token: &str) -> Result<Telegram, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // longer than the 30 s long-poll timeout we ask Telegram for
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|e| e.without_url().to_string())?;
        Ok(Telegram {
            http,
            base: format!("https://api.telegram.org/bot{token}"),
        })
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, body: Value) -> Result<T, CallError> {
        let response = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|e| CallError::Transport(e.without_url().to_string()))?;

        let parsed: ApiResponse<T> = response
            .json()
            .await
            .map_err(|e| CallError::Transport(e.without_url().to_string()))?;

        if parsed.ok {
            parsed
                .result
                .ok_or_else(|| CallError::Transport("empty result".to_string()))
        } else {
            Err(CallError::Api {
                code: parsed.error_code.unwrap_or(0),
                description: parsed.description.unwrap_or_default(),
                retry_after: parsed.parameters.and_then(|p| p.retry_after),
            })
        }
    }

    pub async fn get_me(&self) -> Result<String, CallError> {
        let me: BotInfo = self.call("getMe", json!({})).await?;
        Ok(me.username.unwrap_or_else(|| "unknown".to_string()))
    }

    /// Polling does not work while a webhook is set, so clear any.
    pub async fn delete_webhook(&self) -> Result<(), CallError> {
        let _: bool = self
            .call("deleteWebhook", json!({ "drop_pending_updates": false }))
            .await?;
        Ok(())
    }

    pub async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<Vec<Update>, CallError> {
        self.call(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message", "callback_query"],
            }),
        )
        .await
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: Option<Value>,
    ) -> SendOutcome {
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "link_preview_options": { "is_disabled": true },
        });
        if let Some(markup) = reply_markup {
            body["reply_markup"] = markup;
        }

        match self.call::<Value>("sendMessage", body).await {
            Ok(_) => SendOutcome::Sent,
            Err(CallError::Api {
                code, description, ..
            }) if code == 403 || (code == 400 && description.contains("chat not found")) => {
                SendOutcome::Blocked
            }
            Err(e) => SendOutcome::Failed(e.to_string()),
        }
    }

    /// Stops the loading spinner on a tapped button. Failures don't matter.
    pub async fn answer_callback_query(&self, callback_query_id: &str) {
        let result: Result<bool, CallError> = self
            .call(
                "answerCallbackQuery",
                json!({ "callback_query_id": callback_query_id }),
            )
            .await;
        if let Err(e) = result {
            tracing::debug!("answerCallbackQuery failed: {e}");
        }
    }
}
