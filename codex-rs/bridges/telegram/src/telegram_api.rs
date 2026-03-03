use crate::types::MockOutput;
use crate::types::TELEGRAM_DRAFT_TEXT_LIMIT;
use crate::types::TELEGRAM_MESSAGE_CHUNK_SIZE;
use crate::types::TelegramApiClient;
use crate::types::TelegramApiResponse;
use crate::types::TelegramDraftStreamState;
use crate::types::TelegramGetUpdatesRequest;
use crate::types::TelegramSendMessageDraftRequest;
use crate::types::TelegramSendMessageRequest;
use crate::types::TelegramUpdate;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_bridge_core::BridgeInboundMessage;
use codex_bridge_core::OutboundSender;
use reqwest::blocking::Client;
use tracing::warn;

const DRAFT_ID_START: i64 = 1;

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
            next_draft_id: DRAFT_ID_START,
            stream_draft_by_session: std::collections::HashMap::new(),
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

    pub(crate) fn send_message(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        text: &str,
    ) -> Result<()> {
        for chunk in split_telegram_message(text) {
            self.send_message_chunk(chat_id, message_thread_id, &chunk)?;
        }
        Ok(())
    }

    fn send_message_chunk(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        text: &str,
    ) -> Result<()> {
        let endpoint = self.api_endpoint("sendMessage");
        let request = TelegramSendMessageRequest {
            chat_id,
            message_thread_id,
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

    fn send_message_draft(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        draft_id: i64,
        text: &str,
    ) -> Result<()> {
        let endpoint = self.api_endpoint("sendMessageDraft");
        let request = TelegramSendMessageDraftRequest {
            chat_id,
            message_thread_id,
            draft_id,
            text: text.to_string(),
        };

        let response = self
            .client
            .post(endpoint)
            .json(&request)
            .send()
            .context("call Telegram sendMessageDraft")?
            .error_for_status()
            .context("Telegram sendMessageDraft returned error status")?
            .json::<TelegramApiResponse<serde_json::Value>>()
            .context("decode Telegram sendMessageDraft response")?;

        if !response.ok {
            let description = response
                .description
                .unwrap_or_else(|| "unknown Telegram API error".to_string());
            bail!("Telegram sendMessageDraft failed: {description}");
        }

        Ok(())
    }

    fn api_endpoint(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base_url, self.token, method)
    }
}

impl OutboundSender for TelegramApiClient {
    fn send_text(&mut self, session_id: &str, text: &str) -> Result<()> {
        let (chat_id, message_thread_id) = parse_telegram_session_id(session_id)?;
        self.send_message(chat_id, message_thread_id, text)
    }

    fn supports_stream_preview(&self) -> bool {
        true
    }

    fn begin_stream(&mut self, session_id: &str) -> Result<()> {
        let _parsed = parse_telegram_session_id(session_id)?;
        let draft_id = self.next_draft_id.max(DRAFT_ID_START);
        self.next_draft_id = draft_id.saturating_add(1);
        if self.next_draft_id < DRAFT_ID_START {
            self.next_draft_id = DRAFT_ID_START;
        }
        self.stream_draft_by_session.insert(
            session_id.to_string(),
            TelegramDraftStreamState {
                draft_id,
                full_text: String::new(),
                use_draft_streaming: true,
            },
        );
        Ok(())
    }

    fn send_stream_preview(&mut self, session_id: &str, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        if !self.stream_draft_by_session.contains_key(session_id) {
            self.begin_stream(session_id)?;
        }

        let (chat_id, message_thread_id) = parse_telegram_session_id(session_id)?;
        let (draft_id, draft_text, should_stream) = {
            let Some(stream_state) = self.stream_draft_by_session.get_mut(session_id) else {
                return Ok(());
            };

            stream_state.full_text.push_str(text);
            (
                stream_state.draft_id,
                build_draft_preview_text(&stream_state.full_text),
                stream_state.use_draft_streaming,
            )
        };

        if !should_stream {
            return Ok(());
        }

        if let Err(err) = self.send_message_draft(chat_id, message_thread_id, draft_id, &draft_text)
        {
            warn!(
                session_id,
                error = %err,
                "Telegram sendMessageDraft failed; disabling draft streaming for this turn"
            );
            if let Some(stream_state) = self.stream_draft_by_session.get_mut(session_id) {
                stream_state.use_draft_streaming = false;
            }
        }

        Ok(())
    }

    fn complete_stream(&mut self, session_id: &str) -> Result<()> {
        self.stream_draft_by_session.remove(session_id);
        Ok(())
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

fn parse_telegram_session_id(session_id: &str) -> Result<(i64, Option<i64>)> {
    let trimmed = session_id.trim();
    let Some((chat_raw, thread_raw)) = trimmed.split_once(':') else {
        let chat_id = trimmed
            .parse::<i64>()
            .with_context(|| format!("invalid telegram session id `{session_id}`"))?;
        return Ok((chat_id, None));
    };

    let chat_id = chat_raw
        .trim()
        .parse::<i64>()
        .with_context(|| format!("invalid telegram session chat id in `{session_id}`"))?;
    let thread_id = thread_raw
        .trim()
        .parse::<i64>()
        .with_context(|| format!("invalid telegram session thread id in `{session_id}`"))?;
    Ok((chat_id, (thread_id > 0).then_some(thread_id)))
}

fn build_draft_preview_text(full_text: &str) -> String {
    let trimmed = full_text.trim();
    if trimmed.is_empty() {
        return "(empty response)".to_string();
    }

    let chars = trimmed.chars().collect::<Vec<_>>();
    if chars.len() <= TELEGRAM_DRAFT_TEXT_LIMIT {
        return trimmed.to_string();
    }

    let tail_count = TELEGRAM_DRAFT_TEXT_LIMIT.saturating_sub(3);
    let tail = chars[chars.len() - tail_count..].iter().collect::<String>();
    format!("...{tail}")
}

#[cfg(test)]
mod tests {
    use super::build_draft_preview_text;
    use super::parse_telegram_session_id;
    use super::split_telegram_message;
    use crate::types::TELEGRAM_DRAFT_TEXT_LIMIT;
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

    #[test]
    fn parse_telegram_session_id_supports_legacy_chat_only() {
        let parsed = parse_telegram_session_id("-100123");
        assert_eq!(parsed.ok(), Some((-100123, None)));
    }

    #[test]
    fn parse_telegram_session_id_supports_chat_and_thread() {
        let parsed = parse_telegram_session_id("-100123:42");
        assert_eq!(parsed.ok(), Some((-100123, Some(42))));
    }

    #[test]
    fn parse_telegram_session_id_treats_non_positive_thread_as_none() {
        let parsed = parse_telegram_session_id("-100123:0");
        assert_eq!(parsed.ok(), Some((-100123, None)));
    }

    #[test]
    fn build_draft_preview_text_keeps_short_text() {
        let text = "hello world";
        assert_eq!(build_draft_preview_text(text), "hello world".to_string());
    }

    #[test]
    fn build_draft_preview_text_trims_to_tail_window() {
        let input = "x".repeat(TELEGRAM_DRAFT_TEXT_LIMIT + 25);
        let output = build_draft_preview_text(&input);
        assert_eq!(output.chars().count(), TELEGRAM_DRAFT_TEXT_LIMIT);
        assert!(output.starts_with("..."));
    }
}
