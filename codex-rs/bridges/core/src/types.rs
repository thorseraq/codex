use anyhow::Result;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::UserInput;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::WebSocket;
use tungstenite::stream::MaybeTlsStream;

pub const DEFAULT_CONNECT_RETRY_ATTEMPTS: u32 = 120;
pub(crate) const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

pub type BridgeSessionId = String;

#[derive(Debug, Clone)]
pub struct BridgeInboundMessage {
    pub session_id: BridgeSessionId,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct ThreadOverrides {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub cwd: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub sandbox: Option<SandboxMode>,
}

#[derive(Debug, Clone, Default)]
pub struct BridgeRuntimeConfig {
    pub thread: ThreadOverrides,
    pub auto_approve_commands: bool,
    pub auto_approve_file_changes: bool,
    pub stream_responses: bool,
}

impl BridgeRuntimeConfig {
    pub(crate) fn approval_policy(&self) -> ApprovalPolicy {
        ApprovalPolicy {
            auto_approve_commands: self.auto_approve_commands,
            auto_approve_file_changes: self.auto_approve_file_changes,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BridgeState {
    #[serde(default)]
    pub last_update_id: Option<i64>,
    #[serde(default)]
    pub thread_by_chat_id: HashMap<BridgeSessionId, String>,
    #[serde(default)]
    pub cwd_by_chat_id: HashMap<BridgeSessionId, String>,
    #[serde(default)]
    pub model_by_chat_id: HashMap<BridgeSessionId, String>,
    #[serde(default)]
    pub effort_by_chat_id: HashMap<BridgeSessionId, String>,
}

#[derive(Debug)]
pub struct AppServerClient {
    pub(crate) socket: WebSocket<MaybeTlsStream<TcpStream>>,
    pub(crate) next_request_id: i64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ApprovalPolicy {
    pub(crate) auto_approve_commands: bool,
    pub(crate) auto_approve_file_changes: bool,
}

pub trait OutboundSender {
    fn send_text(&mut self, session_id: &str, text: &str) -> Result<()>;

    fn supports_stream_preview(&self) -> bool {
        false
    }

    fn begin_stream(&mut self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    fn send_stream_preview(&mut self, session_id: &str, text: &str) -> Result<()> {
        self.send_text(session_id, text)
    }

    fn complete_stream(&mut self, _session_id: &str) -> Result<()> {
        Ok(())
    }
}

pub trait BridgeStateStore {
    fn load_state(&self) -> Result<BridgeState>;
    fn persist_state(&self, state: &BridgeState) -> Result<()>;
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeTurnStartParams {
    pub(crate) thread_id: String,
    pub(crate) input: Vec<UserInput>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
}

#[derive(Debug)]
pub(crate) struct EffectiveModelEffort {
    pub(crate) model: String,
    pub(crate) model_source: &'static str,
    pub(crate) effort: String,
    pub(crate) effort_source: &'static str,
}

#[cfg(test)]
mod tests {
    use crate::BridgeState;
    use pretty_assertions::assert_eq;

    #[test]
    fn bridge_state_deserializes_without_chat_cwd_map() {
        let json = r#"{
  "last_update_id": 123,
  "thread_by_chat_id": {
    "-1": "019abc"
  }
}"#;

        let state: BridgeState = serde_json::from_str(json).expect("state should deserialize");
        assert_eq!(state.last_update_id, Some(123));
        assert_eq!(
            state.thread_by_chat_id.get("-1"),
            Some(&"019abc".to_string())
        );
        assert!(state.cwd_by_chat_id.is_empty());
        assert!(state.model_by_chat_id.is_empty());
        assert!(state.effort_by_chat_id.is_empty());
    }
}
