use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::SandboxMode;
use codex_bridge_core::BridgeInboundMessage;
use codex_bridge_core::BridgeRuntimeConfig;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

pub(crate) const DEFAULT_TELEGRAM_API_BASE_URL: &str = "https://api.telegram.org";
pub(crate) const DEFAULT_POLL_TIMEOUT_SECONDS: u32 = 30;
pub(crate) const TELEGRAM_MESSAGE_CHUNK_SIZE: usize = 3800;
pub(crate) const TELEGRAM_DRAFT_TEXT_LIMIT: usize = 4096;
pub(crate) const DEFAULT_MOCK_CHAT_ID: i64 = 1;

#[derive(Debug, Clone)]
pub struct TelegramBridgeArgs {
    pub app_server_url: String,
    pub config_path: PathBuf,
    pub state_path: Option<PathBuf>,
    pub mock_mode: bool,
    pub mock_messages: Vec<String>,
    pub mock_chat_id: Option<i64>,
    pub connect_retry_attempts: Option<u32>,
}

#[derive(Debug, Clone)]
pub(crate) struct BridgeConfig {
    pub(crate) token: Option<String>,
    pub(crate) allowed_chat_ids: HashSet<i64>,
    pub(crate) allowed_user_ids: HashSet<i64>,
    pub(crate) poll_timeout_seconds: u32,
    pub(crate) api_base_url: String,
    pub(crate) runtime: BridgeRuntimeConfig,
    pub(crate) mock_messages: Vec<BridgeInboundMessage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct TelegramBridgeFileConfig {
    pub(crate) token: Option<String>,
    pub(crate) chat_id: Option<i64>,
    pub(crate) allowed_chat_ids: Option<Vec<i64>>,
    pub(crate) reply_chat_id: Option<i64>,
    pub(crate) reply_chat_ids: Option<Vec<i64>>,
    pub(crate) allowed_user_ids: Option<Vec<i64>>,
    pub(crate) poll_timeout_seconds: Option<u32>,
    pub(crate) base_url: Option<String>,
    pub(crate) auto_approve_commands: Option<bool>,
    pub(crate) auto_approve_file_changes: Option<bool>,
    pub(crate) stream_responses: Option<bool>,
    pub(crate) codex: Option<TelegramCodexFileConfig>,
    pub(crate) mock_messages: Option<Vec<MockMessageFileConfig>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct TelegramCodexFileConfig {
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) sandbox: Option<SandboxMode>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct MockMessageFileConfig {
    pub(crate) chat_id: Option<i64>,
    pub(crate) text: String,
}

pub(crate) struct TelegramApiClient {
    pub(crate) client: Client,
    pub(crate) token: String,
    pub(crate) api_base_url: String,
    pub(crate) poll_timeout_seconds: u32,
    pub(crate) next_draft_id: i64,
    pub(crate) stream_draft_by_session: HashMap<String, TelegramDraftStreamState>,
}

#[derive(Debug, Clone)]
pub(crate) struct TelegramDraftStreamState {
    pub(crate) draft_id: i64,
    pub(crate) full_text: String,
    pub(crate) use_draft_streaming: bool,
}

#[derive(Debug, Default)]
pub(crate) struct MockOutput {
    pub(crate) responses: Vec<BridgeInboundMessage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramApiResponse<T> {
    pub(crate) ok: bool,
    pub(crate) result: Option<T>,
    pub(crate) description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramUpdate {
    pub(crate) update_id: i64,
    pub(crate) message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramMessage {
    pub(crate) chat: TelegramChat,
    pub(crate) from: Option<TelegramUser>,
    pub(crate) text: Option<String>,
    pub(crate) message_thread_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramChat {
    pub(crate) id: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramUser {
    pub(crate) id: i64,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TelegramGetUpdatesRequest {
    pub(crate) offset: Option<i64>,
    pub(crate) timeout: u32,
    pub(crate) allowed_updates: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct TelegramSendMessageRequest {
    pub(crate) chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message_thread_id: Option<i64>,
    pub(crate) text: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TelegramSendMessageDraftRequest {
    pub(crate) chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message_thread_id: Option<i64>,
    pub(crate) draft_id: i64,
    pub(crate) text: String,
}
