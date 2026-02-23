use crate::types::BridgeConfig;
use crate::types::DEFAULT_MOCK_CHAT_ID;
use crate::types::DEFAULT_POLL_TIMEOUT_SECONDS;
use crate::types::DEFAULT_TELEGRAM_API_BASE_URL;
use crate::types::TelegramBridgeArgs;
use crate::types::TelegramBridgeFileConfig;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_bridge_core::BridgeInboundMessage;
use codex_bridge_core::BridgeRuntimeConfig;
use codex_bridge_core::ThreadOverrides;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

impl BridgeConfig {
    pub(crate) fn load(args: &TelegramBridgeArgs) -> Result<Self> {
        let file_config = load_file_config(&args.config_path)?;
        let token = std::env::var("CODEX_TELEGRAM_BOT_TOKEN")
            .ok()
            .or(file_config.token.clone());
        let allowed_chat_ids = resolve_allowed_chat_ids(&file_config)?;
        let allowed_user_ids = resolve_allowed_user_ids(&file_config)?;

        let poll_timeout_seconds = file_config
            .poll_timeout_seconds
            .unwrap_or(DEFAULT_POLL_TIMEOUT_SECONDS)
            .max(1);
        let api_base_url = std::env::var("CODEX_TELEGRAM_BASE_URL")
            .ok()
            .or(file_config.base_url.clone())
            .unwrap_or_else(|| DEFAULT_TELEGRAM_API_BASE_URL.to_string());

        let auto_approve_commands = bool_env_override("CODEX_TELEGRAM_AUTO_APPROVE_COMMANDS")?
            .or(file_config.auto_approve_commands)
            .unwrap_or(false);
        let auto_approve_file_changes =
            bool_env_override("CODEX_TELEGRAM_AUTO_APPROVE_FILE_CHANGES")?
                .or(file_config.auto_approve_file_changes)
                .unwrap_or(false);

        let thread = file_config
            .codex
            .clone()
            .map(|codex| ThreadOverrides {
                model: codex.model,
                reasoning_effort: codex.reasoning_effort,
                cwd: codex.cwd,
                approval_policy: codex.approval_policy,
                sandbox: codex.sandbox,
            })
            .unwrap_or_default();
        let runtime = BridgeRuntimeConfig {
            thread,
            auto_approve_commands,
            auto_approve_file_changes,
        };

        let mock_chat_id = args
            .mock_chat_id
            .or(file_config.chat_id)
            .unwrap_or_else(|| {
                allowed_chat_ids
                    .iter()
                    .copied()
                    .next()
                    .unwrap_or(DEFAULT_MOCK_CHAT_ID)
            });
        let mock_messages = resolve_mock_messages(args, &file_config, mock_chat_id);

        if !args.mock_mode {
            if token.is_none() {
                bail!(
                    "telegram bridge requires CODEX_TELEGRAM_BOT_TOKEN (or token in {})",
                    args.config_path.display()
                );
            }
            if allowed_chat_ids.is_empty() && allowed_user_ids.is_empty() {
                bail!(
                    "telegram bridge requires at least one allowed chat id or allowed user id via CODEX_TELEGRAM_CHAT_ID/CODEX_TELEGRAM_CHAT_IDS/CODEX_TELEGRAM_ALLOWED_USER_ID/CODEX_TELEGRAM_ALLOWED_USER_IDS or config chat_id/allowed_chat_ids/allowed_user_ids"
                );
            }
        }

        Ok(Self {
            token,
            allowed_chat_ids,
            allowed_user_ids,
            poll_timeout_seconds,
            api_base_url,
            runtime,
            mock_messages,
        })
    }
}

fn bool_env_override(name: &str) -> Result<Option<bool>> {
    let Some(raw) = std::env::var(name).ok() else {
        return Ok(None);
    };
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "y" => Ok(Some(true)),
        "0" | "false" | "no" | "n" => Ok(Some(false)),
        _ => bail!("invalid {name} value `{raw}`; expected true/false"),
    }
}

fn resolve_mock_messages(
    args: &TelegramBridgeArgs,
    file_config: &TelegramBridgeFileConfig,
    fallback_chat_id: i64,
) -> Vec<BridgeInboundMessage> {
    if !args.mock_messages.is_empty() {
        return args
            .mock_messages
            .iter()
            .map(|text| BridgeInboundMessage {
                session_id: fallback_chat_id.to_string(),
                text: text.clone(),
            })
            .collect();
    }

    file_config
        .mock_messages
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| BridgeInboundMessage {
            session_id: entry.chat_id.unwrap_or(fallback_chat_id).to_string(),
            text: entry.text,
        })
        .collect()
}

fn resolve_allowed_chat_ids(file_config: &TelegramBridgeFileConfig) -> Result<HashSet<i64>> {
    if let Ok(value) = std::env::var("CODEX_TELEGRAM_CHAT_IDS") {
        return parse_chat_id_csv(&value);
    }
    if let Ok(value) = std::env::var("CODEX_TELEGRAM_CHAT_ID") {
        return parse_chat_id_csv(&value);
    }

    let mut ids = file_config.allowed_chat_ids.clone().unwrap_or_default();
    if let Some(chat_id) = file_config.chat_id {
        ids.push(chat_id);
    }

    Ok(ids.into_iter().collect())
}

fn resolve_allowed_user_ids(file_config: &TelegramBridgeFileConfig) -> Result<HashSet<i64>> {
    if let Ok(value) = std::env::var("CODEX_TELEGRAM_ALLOWED_USER_IDS") {
        return parse_chat_id_csv(&value);
    }
    if let Ok(value) = std::env::var("CODEX_TELEGRAM_ALLOWED_USER_ID") {
        return parse_chat_id_csv(&value);
    }

    Ok(file_config
        .allowed_user_ids
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect())
}

fn parse_chat_id_csv(raw: &str) -> Result<HashSet<i64>> {
    let mut values = HashSet::new();
    for entry in raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let value = entry
            .parse::<i64>()
            .with_context(|| format!("invalid chat id `{entry}`"))?;
        values.insert(value);
    }
    Ok(values)
}

fn load_file_config(path: &Path) -> Result<TelegramBridgeFileConfig> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .with_context(|| format!("parse telegram config {}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(TelegramBridgeFileConfig::default()),
        Err(err) => Err(err).with_context(|| format!("read telegram config {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_chat_id_csv;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_chat_id_csv_handles_multiple_values() {
        let parsed = parse_chat_id_csv("123, 456,789");
        assert!(parsed.is_ok());
        let parsed = parsed.unwrap_or_default();
        assert_eq!(parsed.len(), 3);
        assert!(parsed.contains(&123));
        assert!(parsed.contains(&456));
        assert!(parsed.contains(&789));
    }

    #[test]
    fn parse_chat_id_csv_rejects_invalid_entry() {
        let parsed = parse_chat_id_csv("abc");
        assert!(parsed.is_err());
    }
}
