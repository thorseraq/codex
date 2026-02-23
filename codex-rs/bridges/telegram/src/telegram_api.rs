use crate::types::MockOutput;
use crate::types::TELEGRAM_MESSAGE_CHUNK_SIZE;
use crate::types::TelegramApiClient;
use crate::types::TelegramApiResponse;
use crate::types::TelegramGetUpdatesRequest;
use crate::types::TelegramSendMessageRequest;
use crate::types::TelegramUpdate;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_bridge_core::BridgeInboundMessage;
use codex_bridge_core::OutboundSender;
use reqwest::blocking::Client;

impl TelegramApiClient {
    pub(crate) fn new(
        token: String,
        api_base_url: String,
        poll_timeout_seconds: u32,
    ) -> Result<Self> {
        let timeout = std::time::Duration::from_secs(u64::from(poll_timeout_seconds) + 15);
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .context("create telegram http client")?;

        Ok(Self {
            client,
            token,
            api_base_url,
            poll_timeout_seconds,
        })
    }

    pub(crate) fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>> {
        let endpoint = self.api_endpoint("getUpdates");
        let request = TelegramGetUpdatesRequest {
            offset,
            timeout: self.poll_timeout_seconds,
            allowed_updates: vec!["message".to_string()],
        };

        let response = self
            .client
            .post(endpoint)
            .json(&request)
            .send()
            .context("call Telegram getUpdates")?
            .error_for_status()
            .context("Telegram getUpdates returned error status")?
            .json::<TelegramApiResponse<Vec<TelegramUpdate>>>()
            .context("decode Telegram getUpdates response")?;

        if !response.ok {
            let description = response
                .description
                .unwrap_or_else(|| "unknown Telegram API error".to_string());
            bail!("Telegram getUpdates failed: {description}");
        }

        Ok(response.result.unwrap_or_default())
    }

    pub(crate) fn send_message(&self, chat_id: i64, text: &str) -> Result<()> {
        for chunk in split_telegram_message(text) {
            self.send_message_chunk(chat_id, &chunk)?;
        }
        Ok(())
    }

    fn send_message_chunk(&self, chat_id: i64, text: &str) -> Result<()> {
        let endpoint = self.api_endpoint("sendMessage");
        let request = TelegramSendMessageRequest {
            chat_id,
            text: text.to_string(),
        };

        let response = self
            .client
            .post(endpoint)
            .json(&request)
            .send()
            .context("call Telegram sendMessage")?
            .error_for_status()
            .context("Telegram sendMessage returned error status")?
            .json::<TelegramApiResponse<serde_json::Value>>()
            .context("decode Telegram sendMessage response")?;

        if !response.ok {
            let description = response
                .description
                .unwrap_or_else(|| "unknown Telegram API error".to_string());
            bail!("Telegram sendMessage failed: {description}");
        }

        Ok(())
    }

    fn api_endpoint(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base_url, self.token, method)
    }
}

impl OutboundSender for TelegramApiClient {
    fn send_text(&mut self, session_id: &str, text: &str) -> Result<()> {
        let chat_id = session_id
            .parse::<i64>()
            .with_context(|| format!("invalid telegram session id `{session_id}`"))?;
        self.send_message(chat_id, text)
    }
}

impl OutboundSender for MockOutput {
    fn send_text(&mut self, session_id: &str, text: &str) -> Result<()> {
        self.responses.push(BridgeInboundMessage {
            session_id: session_id.to_string(),
            text: text.to_string(),
        });
        Ok(())
    }
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
    use super::split_telegram_message;
    use crate::types::TELEGRAM_MESSAGE_CHUNK_SIZE;
    use pretty_assertions::assert_eq;

    #[test]
    fn split_telegram_message_limits_chunk_length() {
        let input = "x".repeat(TELEGRAM_MESSAGE_CHUNK_SIZE + 25);
        let chunks = split_telegram_message(&input);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), TELEGRAM_MESSAGE_CHUNK_SIZE);
        assert_eq!(chunks[1].chars().count(), 25);
    }

    #[test]
    fn split_telegram_message_falls_back_for_empty_text() {
        let chunks = split_telegram_message("   ");
        assert_eq!(chunks, vec!["(empty response)".to_string()]);
    }
}
