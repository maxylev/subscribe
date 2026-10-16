//! Minimal Telegram Bot API client (long polling).
//!
//! Implements only what the bot needs: `getMe`, `getUpdates` (long poll with
//! offset confirmation), `sendMessage` with HTML formatting and an optional
//! inline keyboard, and `answerCallbackQuery`. The base URL is configurable
//! so tests can point at a mock server.
//!
//! Bot API URLs embed the bot token, so transport errors are always stripped
//! of their URL before they can reach a log line.

use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::config::Secret;

/// Telegram's maximum message length (in characters).
pub const MAX_MESSAGE_CHARS: usize = 4096;

/// Update types the bot processes: plain messages and inline-keyboard taps.
const ALLOWED_UPDATES: &str = r#"["message","callback_query"]"#;

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    #[serde(default)]
    pub username: Option<String>,
}

/// One inline keyboard button: `(label, callback_data)`.
pub type Button = (&'static str, &'static str);

/// Telegram API error, carrying the server-requested back-off if any.
#[derive(Debug)]
pub struct ApiError {
    pub description: String,
    pub retry_after: Option<u64>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "telegram: {}", self.description)
    }
}

impl std::error::Error for ApiError {}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<ResponseParameters>,
}

#[derive(Deserialize)]
struct ResponseParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

#[derive(Clone)]
pub struct TgClient {
    base: String,
    token: Secret,
    client: reqwest::Client,
}

impl TgClient {
    pub fn new(base: &str, token: Secret, poll_timeout_secs: u64) -> Result<Self> {
        let client = crate::http_client()
            // Long polls hold the request open for `poll_timeout_secs`.
            .timeout(Duration::from_secs(poll_timeout_secs.saturating_add(15)))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            client,
        })
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, body: &Value) -> Result<T> {
        let url = format!("{}/bot{}/{method}", self.base, self.token.expose());
        let resp = self
            .client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("{method}: {}", e.without_url()))?;
        let env: Envelope = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("{method}: invalid response: {}", e.without_url()))?;
        if !env.ok {
            return Err(ApiError {
                description: env.description.unwrap_or_else(|| "unknown error".into()),
                retry_after: env.parameters.and_then(|p| p.retry_after),
            }
            .into());
        }
        let Some(result) = env.result else {
            bail!("{method}: response has no result");
        };
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_me(&self) -> Result<Me> {
        self.call("getMe", &json!({})).await
    }

    pub async fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Update>> {
        let allowed: Value = serde_json::from_str(ALLOWED_UPDATES)?;
        self.call(
            "getUpdates",
            &json!({"offset": offset, "timeout": timeout_secs, "allowed_updates": allowed}),
        )
        .await
    }

    /// Send an HTML-formatted message, truncated to Telegram's length limit,
    /// with an optional inline keyboard.
    pub async fn send_html(&self, chat_id: i64, html: &str, keyboard: &[&[Button]]) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": clamp_message(html),
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
        });
        if !keyboard.is_empty() {
            let rows: Vec<Vec<Value>> = keyboard
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|(text, data)| json!({"text": text, "callback_data": data}))
                        .collect()
                })
                .collect();
            body["reply_markup"] = json!({"inline_keyboard": rows});
        }
        self.call::<Value>("sendMessage", &body).await.map(drop)
    }

    /// Stop the client-side "loading" spinner on a tapped inline button.
    pub async fn answer_callback_query(&self, id: &str) -> Result<()> {
        self.call::<Value>("answerCallbackQuery", &json!({"callback_query_id": id}))
            .await
            .map(drop)
    }
}

/// Truncate to [`MAX_MESSAGE_CHARS`], closing an open `<pre>` block so the
/// HTML stays valid. Messages are built so `<pre>` is the only element that
/// can be cut.
fn clamp_message(html: &str) -> String {
    const TAIL: &str = "…</pre>";
    if html.chars().count() <= MAX_MESSAGE_CHARS {
        return html.to_string();
    }
    let mut cut: String = html.chars().take(MAX_MESSAGE_CHARS - TAIL.len()).collect();
    // Never cut inside an entity like `&amp;` or a tag.
    if let Some(amp) = cut.rfind('&')
        && !cut[amp..].contains(';')
    {
        cut.truncate(amp);
    }
    if let Some(lt) = cut.rfind('<')
        && !cut[lt..].contains('>')
    {
        cut.truncate(lt);
    }
    if cut.matches("<pre>").count() > cut.matches("</pre>").count() {
        cut.push_str(TAIL);
    } else {
        cut.push('…');
    }
    cut
}

/// Split a command message into `(command, argument)`, accepting the
/// `/cmd@botname` form Telegram uses in groups.
pub fn parse_command(text: &str) -> Option<(&str, &str)> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }
    let (head, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(h, r)| (h, r.trim()));
    let cmd = head.split_once('@').map_or(head, |(c, _)| c);
    Some((cmd, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_updates() {
        let u: Update = serde_json::from_value(json!({
            "update_id": 42,
            "message": {
                "message_id": 1,
                "from": {"id": 424_242, "is_bot": false, "first_name": "Tester"},
                "chat": {"id": 424_242, "type": "private"},
                "date": 0,
                "text": "/status"
            }
        }))
        .unwrap();
        let msg = u.message.unwrap();
        assert_eq!(msg.chat.id, 424_242);
        assert_eq!(msg.text.as_deref(), Some("/status"));

        let u: Update = serde_json::from_value(json!({
            "update_id": 7,
            "callback_query": {
                "id": "cb-1",
                "from": {"id": 424_242, "is_bot": false, "first_name": "Tester"},
                "message": {"message_id": 2, "chat": {"id": 424_242, "type": "private"}},
                "data": "status"
            }
        }))
        .unwrap();
        let cq = u.callback_query.unwrap();
        assert_eq!(
            (cq.id.as_str(), cq.data.as_deref()),
            ("cb-1", Some("status"))
        );

        let u: Update = serde_json::from_value(json!({"update_id": 8})).unwrap();
        assert!(u.message.is_none() && u.callback_query.is_none());
    }

    #[test]
    fn commands() {
        assert_eq!(parse_command("/start"), Some(("/start", "")));
        assert_eq!(parse_command("/run@my_bot  demo "), Some(("/run", "demo")));
        assert_eq!(
            parse_command("/config\n{\"a\":1}"),
            Some(("/config", "{\"a\":1}"))
        );
        assert_eq!(parse_command("hello"), None);
    }

    #[test]
    fn long_messages_are_clamped_with_valid_html() {
        let html = format!("<b>job</b>\n<pre>{}</pre>", "&amp;".repeat(2000));
        let out = clamp_message(&html);
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(out.ends_with("…</pre>"), "{}", &out[out.len() - 20..]);
        assert!(!out.contains("&am…"), "entity not split");
        assert_eq!(clamp_message("short"), "short");
    }

    #[test]
    fn api_errors_carry_retry_after() {
        let env: Envelope = serde_json::from_value(json!({
            "ok": false, "error_code": 429, "description": "Too Many Requests",
            "parameters": {"retry_after": 7}
        }))
        .unwrap();
        assert!(!env.ok);
        assert_eq!(env.parameters.unwrap().retry_after, Some(7));
    }
}
