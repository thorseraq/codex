use crate::config::load_file_config;
use crate::config::parse_chat_id_csv;
use crate::types::DEFAULT_TELEGRAM_API_BASE_URL;
use crate::types::TELEGRAM_MESSAGE_CHUNK_SIZE;
use crate::types::TelegramApiResponse;
use crate::types::TelegramBridgeFileConfig;
use crate::types::TelegramSendMessageRequest;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::warn;

const REPLY_RELAY_ENABLED_ENV: &str = "CODEX_TELEGRAM_REPLY_RELAY";
const REPLY_RELAY_BOT_TOKEN_ENV: &str = "CODEX_TELEGRAM_REPLY_BOT_TOKEN";
const REPLY_RELAY_CHAT_IDS_ENV: &str = "CODEX_TELEGRAM_REPLY_CHAT_IDS";
const REPLY_RELAY_BASE_URL_ENV: &str = "CODEX_TELEGRAM_REPLY_BASE_URL";
const TELEGRAM_BOT_TOKEN_ENV: &str = "CODEX_TELEGRAM_BOT_TOKEN";
const TELEGRAM_CHAT_ID_ENV: &str = "CODEX_TELEGRAM_CHAT_ID";
const TELEGRAM_CHAT_IDS_ENV: &str = "CODEX_TELEGRAM_CHAT_IDS";
const TELEGRAM_BASE_URL_ENV: &str = "CODEX_TELEGRAM_BASE_URL";
const REPLY_RELAY_TIMEOUT_SECONDS: u64 = 15;
const MAX_DEDUP_KEYS: usize = 4_096;

#[derive(Debug, Clone)]
pub struct TelegramReplyRelayArgs {
    pub codex_home: PathBuf,
    pub config_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct TelegramReplyRelay {
    tx: mpsc::Sender<RelayRequest>,
}

#[derive(Debug, Clone, Copy)]
pub struct TelegramReplyRelayMessage<'a> {
    pub thread_id: &'a str,
    pub turn_id: &'a str,
    pub response: &'a str,
    pub cwd: Option<&'a Path>,
    pub prompt: Option<&'a str>,
}

#[derive(Debug)]
struct RelayRequest {
    delivery_key: String,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelayRuntimeConfig {
    token: String,
    api_base_url: String,
    chat_ids: Vec<i64>,
}

impl TelegramReplyRelay {
    pub fn from_args(args: TelegramReplyRelayArgs) -> Result<Option<Self>> {
        let env = std::env::vars().collect::<HashMap<_, _>>();
        if !reply_relay_enabled(&env)? {
            return Ok(None);
        }

        let config_path = args
            .config_path
            .unwrap_or_else(|| args.codex_home.join("telegram").join("config.toml"));
        let file_config = load_file_config(&config_path)?;
        let runtime = resolve_runtime_config(&env, file_config, &config_path)?;

        let (tx, rx) = mpsc::channel::<RelayRequest>();
        thread::Builder::new()
            .name("telegram-reply-relay".to_string())
            .spawn(move || run_reply_relay_worker(runtime, rx))
            .context("spawn telegram reply relay worker")?;

        Ok(Some(Self { tx }))
    }

    pub fn send_turn_reply(&self, thread_id: &str, turn_id: &str, text: &str) {
        self.send_turn_reply_with_context(TelegramReplyRelayMessage {
            thread_id,
            turn_id,
            response: text,
            cwd: None,
            prompt: None,
        });
    }

    pub fn send_turn_reply_with_context(&self, message: TelegramReplyRelayMessage<'_>) {
        let response = message.response.trim();
        if response.is_empty() {
            return;
        }

        let delivery_key = format!("{}:{}", message.thread_id, message.turn_id);
        let formatted_text = format_relay_message(message);
        let _ignored = self.tx.send(RelayRequest {
            delivery_key,
            text: formatted_text,
        });
    }
}

fn format_relay_message(message: TelegramReplyRelayMessage<'_>) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Thread: {}", message.thread_id));
    lines.push(format!("Turn: {}", message.turn_id));
    lines.push(format!(
        "CWD: {}",
        message
            .cwd
            .map(|cwd| cwd.display().to_string())
            .unwrap_or_else(|| "(unknown)".to_string())
    ));
    lines.push(String::new());

    if let Some(prompt) = message
        .prompt
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        lines.push("Prompt:".to_string());
        lines.push(prompt.to_string());
    } else {
        lines.push("Prompt: (unavailable)".to_string());
    }
    lines.push(String::new());
    lines.push("Reply:".to_string());
    lines.push(message.response.trim().to_string());
    lines.join("\n")
}

fn reply_relay_enabled(env: &HashMap<String, String>) -> Result<bool> {
    Ok(parse_bool_env(env, REPLY_RELAY_ENABLED_ENV)?.unwrap_or(false))
}

fn parse_bool_env(env: &HashMap<String, String>, name: &str) -> Result<Option<bool>> {
    let Some(raw) = env.get(name) else {
        return Ok(None);
    };
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "y" => Ok(Some(true)),
        "0" | "false" | "no" | "n" => Ok(Some(false)),
        _ => bail!("invalid {name} value `{raw}`; expected true/false"),
    }
}

fn resolve_runtime_config(
    env: &HashMap<String, String>,
    file_config: TelegramBridgeFileConfig,
    config_path: &Path,
) -> Result<RelayRuntimeConfig> {
    let token = env
        .get(REPLY_RELAY_BOT_TOKEN_ENV)
        .cloned()
        .or_else(|| env.get(TELEGRAM_BOT_TOKEN_ENV).cloned())
        .or(file_config.token.clone())
        .filter(|token| !token.trim().is_empty())
        .with_context(|| {
            format!(
                "telegram reply relay requires {REPLY_RELAY_BOT_TOKEN_ENV} or {TELEGRAM_BOT_TOKEN_ENV} (or token in {})",
                config_path.display()
            )
        })?;

    let chat_ids = resolve_chat_ids(env, &file_config)?;
    if chat_ids.is_empty() {
        bail!(
            "telegram reply relay requires at least one destination chat id via {REPLY_RELAY_CHAT_IDS_ENV}/{TELEGRAM_CHAT_IDS_ENV}/{TELEGRAM_CHAT_ID_ENV} or config chat_id/allowed_chat_ids"
        );
    }

    let api_base_url = env
        .get(REPLY_RELAY_BASE_URL_ENV)
        .cloned()
        .or_else(|| env.get(TELEGRAM_BASE_URL_ENV).cloned())
        .or(file_config.base_url)
        .unwrap_or_else(|| DEFAULT_TELEGRAM_API_BASE_URL.to_string());

    Ok(RelayRuntimeConfig {
        token,
        api_base_url,
        chat_ids,
    })
}

fn resolve_chat_ids(
    env: &HashMap<String, String>,
    file_config: &TelegramBridgeFileConfig,
) -> Result<Vec<i64>> {
    let chat_ids = if let Some(value) = env.get(REPLY_RELAY_CHAT_IDS_ENV) {
        parse_chat_id_csv(value)?
    } else if let Some(value) = env.get(TELEGRAM_CHAT_IDS_ENV) {
        parse_chat_id_csv(value)?
    } else if let Some(value) = env.get(TELEGRAM_CHAT_ID_ENV) {
        parse_chat_id_csv(value)?
    } else {
        let mut ids = file_config.reply_chat_ids.clone().unwrap_or_default();
        if let Some(chat_id) = file_config.reply_chat_id {
            ids.push(chat_id);
        }
        if ids.is_empty() {
            ids = file_config.allowed_chat_ids.clone().unwrap_or_default();
            if let Some(chat_id) = file_config.chat_id {
                ids.push(chat_id);
            }
        }
        ids.into_iter().collect::<HashSet<_>>()
    };

    let mut chat_ids = chat_ids.into_iter().collect::<Vec<_>>();
    chat_ids.sort_unstable();
    Ok(chat_ids)
}

fn run_reply_relay_worker(runtime: RelayRuntimeConfig, rx: mpsc::Receiver<RelayRequest>) {
    let client = match Client::builder()
        .timeout(Duration::from_secs(REPLY_RELAY_TIMEOUT_SECONDS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            warn!(error = %err, "failed to create telegram reply relay http client");
            return;
        }
    };

    let mut sent_delivery_keys = HashSet::new();
    while let Ok(request) = rx.recv() {
        if !sent_delivery_keys.insert(request.delivery_key) {
            continue;
        }
        if sent_delivery_keys.len() > MAX_DEDUP_KEYS {
            sent_delivery_keys.clear();
        }

        for chat_id in &runtime.chat_ids {
            if let Err(err) = send_message(&client, &runtime, *chat_id, &request.text) {
                warn!(
                    chat_id,
                    error = %err,
                    "failed to send telegram reply relay message"
                );
            }
        }
    }
}

fn send_message(
    client: &Client,
    runtime: &RelayRuntimeConfig,
    chat_id: i64,
    text: &str,
) -> Result<()> {
    for chunk in split_telegram_message(text) {
        send_message_chunk(client, runtime, chat_id, &chunk)?;
    }
    Ok(())
}

fn send_message_chunk(
    client: &Client,
    runtime: &RelayRuntimeConfig,
    chat_id: i64,
    text: &str,
) -> Result<()> {
    let endpoint = format!("{}/bot{}/sendMessage", runtime.api_base_url, runtime.token);
    let request = TelegramSendMessageRequest {
        chat_id,
        text: text.to_string(),
    };

    let response = client
        .post(endpoint)
        .json(&request)
        .send()
        .context("call Telegram sendMessage for reply relay")?
        .error_for_status()
        .context("Telegram sendMessage returned error status for reply relay")?
        .json::<TelegramApiResponse<serde_json::Value>>()
        .context("decode Telegram sendMessage response for reply relay")?;

    if response.ok {
        return Ok(());
    }

    let description = response
        .description
        .unwrap_or_else(|| "unknown Telegram API error".to_string());
    bail!("Telegram sendMessage failed: {description}")
}

fn split_telegram_message(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return vec!["(empty response)".to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for ch in trimmed.chars() {
        if current_len >= TELEGRAM_MESSAGE_CHUNK_SIZE {
            chunks.push(current);
            current = String::new();
            current_len = 0;
        }
        current.push(ch);
        current_len += 1;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::REPLY_RELAY_ENABLED_ENV;
    use super::RelayRuntimeConfig;
    use super::TelegramReplyRelayMessage;
    use super::format_relay_message;
    use super::reply_relay_enabled;
    use super::resolve_runtime_config;
    use crate::types::TelegramBridgeFileConfig;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::path::Path;

    #[test]
    fn reply_relay_enabled_defaults_to_false() {
        let env = HashMap::new();
        assert!(!reply_relay_enabled(&env).expect("reply relay enabled check"));
    }

    #[test]
    fn reply_relay_enabled_accepts_true_values() {
        let mut env = HashMap::new();
        env.insert(REPLY_RELAY_ENABLED_ENV.to_string(), "true".to_string());
        assert!(reply_relay_enabled(&env).expect("reply relay enabled check"));
    }

    #[test]
    fn resolve_runtime_config_prefers_reply_specific_env_over_file() {
        let mut env = HashMap::new();
        env.insert(
            "CODEX_TELEGRAM_REPLY_BOT_TOKEN".to_string(),
            "env-token".to_string(),
        );
        env.insert(
            "CODEX_TELEGRAM_REPLY_CHAT_IDS".to_string(),
            "10,20".to_string(),
        );
        env.insert(
            "CODEX_TELEGRAM_REPLY_BASE_URL".to_string(),
            "https://relay.example".to_string(),
        );

        let file = TelegramBridgeFileConfig {
            token: Some("file-token".to_string()),
            chat_id: Some(1),
            allowed_chat_ids: Some(vec![2]),
            ..Default::default()
        };

        let runtime = resolve_runtime_config(&env, file, Path::new("/tmp/config.toml"));
        assert!(runtime.is_ok());
        assert_eq!(
            runtime.expect("runtime config from env"),
            RelayRuntimeConfig {
                token: "env-token".to_string(),
                api_base_url: "https://relay.example".to_string(),
                chat_ids: vec![10, 20],
            }
        );
    }

    #[test]
    fn resolve_runtime_config_uses_file_defaults() {
        let env = HashMap::new();
        let file = TelegramBridgeFileConfig {
            token: Some("file-token".to_string()),
            chat_id: Some(42),
            allowed_chat_ids: Some(vec![7, 42]),
            ..Default::default()
        };

        let runtime = resolve_runtime_config(&env, file, Path::new("/tmp/config.toml"));
        assert!(runtime.is_ok());
        assert_eq!(
            runtime.expect("runtime config from file"),
            RelayRuntimeConfig {
                token: "file-token".to_string(),
                api_base_url: "https://api.telegram.org".to_string(),
                chat_ids: vec![7, 42],
            }
        );
    }

    #[test]
    fn resolve_runtime_config_prefers_reply_chat_ids_in_file() {
        let env = HashMap::new();
        let file = TelegramBridgeFileConfig {
            token: Some("file-token".to_string()),
            chat_id: Some(42),
            allowed_chat_ids: Some(vec![7, 42]),
            reply_chat_id: Some(100),
            reply_chat_ids: Some(vec![200, 300]),
            ..Default::default()
        };

        let runtime = resolve_runtime_config(&env, file, Path::new("/tmp/config.toml"));
        assert!(runtime.is_ok());
        assert_eq!(
            runtime.expect("runtime config from file"),
            RelayRuntimeConfig {
                token: "file-token".to_string(),
                api_base_url: "https://api.telegram.org".to_string(),
                chat_ids: vec![100, 200, 300],
            }
        );
    }

    #[test]
    fn format_relay_message_includes_metadata_and_sections() {
        let text = format_relay_message(TelegramReplyRelayMessage {
            thread_id: "thread-1",
            turn_id: "turn-1",
            response: "final answer",
            cwd: Some(Path::new("/tmp/project")),
            prompt: Some("what changed?"),
        });

        assert_eq!(
            text,
            "Thread: thread-1\nTurn: turn-1\nCWD: /tmp/project\n\nPrompt:\nwhat changed?\n\nReply:\nfinal answer"
        );
    }
}
