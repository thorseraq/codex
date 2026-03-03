use crate::types::AppServerClient;
use crate::types::ApprovalPolicy;
use crate::types::BridgeTurnStartParams;
use crate::types::CONNECT_RETRY_DELAY;
use crate::types::ThreadOverrides;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::FileChangeRequestApprovalResponse;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::SkillApprovalDecision;
use codex_app_server_protocol::SkillRequestApprovalResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ToolRequestUserInputAnswer;
use codex_app_server_protocol::ToolRequestUserInputResponse;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::thread;
use tracing::warn;
use tungstenite::Message;
use tungstenite::connect;
use url::Url;

impl AppServerClient {
    pub fn connect_with_retry(url: &str, retry_attempts: u32) -> Result<Self> {
        let parsed_url =
            Url::parse(url).with_context(|| format!("invalid app-server URL `{url}`"))?;

        let total_attempts = retry_attempts.max(1);
        for attempt in 1..=total_attempts {
            match connect(parsed_url.as_str()) {
                Ok((socket, _response)) => {
                    return Ok(Self {
                        socket,
                        next_request_id: 1,
                    });
                }
                Err(err) => {
                    if attempt == total_attempts {
                        return Err(err).with_context(|| {
                            format!(
                                "failed to connect to app-server websocket `{url}` after {total_attempts} attempts"
                            )
                        });
                    }
                    warn!(
                        attempt,
                        total_attempts,
                        error = %err,
                        "failed to connect to app-server websocket; retrying"
                    );
                    thread::sleep(CONNECT_RETRY_DELAY);
                }
            }
        }

        bail!("connect retry loop exited unexpectedly")
    }

    pub fn initialize(&mut self) -> Result<()> {
        let params = InitializeParams {
            client_info: ClientInfo {
                name: "codex_bridge".to_string(),
                title: Some("Codex Bridge".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            capabilities: Some(InitializeCapabilities {
                experimental_api: true,
                opt_out_notification_methods: None,
            }),
        };
        let _response: InitializeResponse = self.send_request("initialize", params)?;

        let initialized_notification = JSONRPCMessage::Notification(JSONRPCNotification {
            method: "initialized".to_string(),
            params: None,
        });
        self.write_jsonrpc_message(initialized_notification)?;
        Ok(())
    }

    pub(crate) fn start_thread(&mut self, overrides: &ThreadOverrides) -> Result<String> {
        let params = ThreadStartParams {
            model: overrides.model.clone(),
            cwd: overrides.cwd.clone(),
            approval_policy: overrides.approval_policy,
            sandbox: overrides.sandbox,
            ..Default::default()
        };
        let response: ThreadStartResponse = self.send_request("thread/start", params)?;
        Ok(response.thread.id)
    }

    pub(crate) fn resume_thread(&mut self, thread_id: &str) -> Result<()> {
        let params = ThreadResumeParams {
            thread_id: thread_id.to_string(),
            ..Default::default()
        };
        let _response: ThreadResumeResponse = self.send_request("thread/resume", params)?;
        Ok(())
    }

    pub fn request_turn_interrupt(&mut self, thread_id: &str, turn_id: &str) -> Result<()> {
        let request_id = RequestId::Integer(self.next_request_id);
        self.next_request_id += 1;

        let params = TurnInterruptParams {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
        };
        let request = JSONRPCMessage::Request(JSONRPCRequest {
            id: request_id,
            method: "turn/interrupt".to_string(),
            params: Some(
                serde_json::to_value(params).context("serialize `turn/interrupt` params")?,
            ),
        });
        self.write_jsonrpc_message(request)
    }

    pub(crate) fn start_turn_with_overrides(
        &mut self,
        thread_id: &str,
        message: &str,
        overrides: &ThreadOverrides,
    ) -> Result<String> {
        let params = BridgeTurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: message.to_string(),
                text_elements: Vec::new(),
            }],
            model: overrides.model.clone(),
            effort: overrides.reasoning_effort.clone(),
        };

        let response: TurnStartResponse = self.send_request("turn/start", params)?;
        Ok(response.turn.id)
    }

    pub(crate) fn list_models(&mut self) -> Result<Vec<Model>> {
        let mut cursor = None;
        let mut all = Vec::new();

        loop {
            let params = ModelListParams {
                cursor,
                limit: Some(200),
                include_hidden: Some(false),
            };
            let response: ModelListResponse = self.send_request("model/list", params)?;
            all.extend(response.data);
            cursor = response.next_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(all)
    }

    pub(crate) fn run_turn_until_complete(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        approvals: ApprovalPolicy,
    ) -> Result<String> {
        let (response_text, _saw_agent_deltas) = self
            .run_turn_until_complete_with_agent_message_deltas(
                thread_id,
                turn_id,
                approvals,
                |_delta| Ok(()),
            )?;
        Ok(response_text)
    }

    pub(crate) fn run_turn_until_complete_with_agent_message_deltas<F>(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        approvals: ApprovalPolicy,
        mut on_agent_delta: F,
    ) -> Result<(String, bool)>
    where
        F: FnMut(&str) -> Result<()>,
    {
        let mut streamed_text = String::new();
        let mut completed_agent_messages = Vec::new();
        let mut last_error: Option<String> = None;
        let mut saw_agent_deltas = false;

        loop {
            let incoming = self.read_jsonrpc_message()?;
            match incoming {
                JSONRPCMessage::Request(request) => {
                    self.handle_server_request(request, approvals)?;
                }
                JSONRPCMessage::Notification(notification) => {
                    let Ok(server_notification) = ServerNotification::try_from(notification) else {
                        continue;
                    };

                    match server_notification {
                        ServerNotification::AgentMessageDelta(delta)
                            if delta.thread_id == thread_id && delta.turn_id == turn_id =>
                        {
                            streamed_text.push_str(&delta.delta);
                            if !delta.delta.is_empty() {
                                on_agent_delta(&delta.delta)?;
                                saw_agent_deltas = true;
                            }
                        }
                        ServerNotification::ItemCompleted(item_completed)
                            if item_completed.thread_id == thread_id
                                && item_completed.turn_id == turn_id =>
                        {
                            if let ThreadItem::AgentMessage { text, .. } = item_completed.item {
                                completed_agent_messages.push(text);
                            }
                        }
                        ServerNotification::Error(error)
                            if error.thread_id == thread_id && error.turn_id == turn_id =>
                        {
                            last_error = Some(error.error.message);
                        }
                        ServerNotification::TurnCompleted(turn_completed)
                            if turn_completed.thread_id == thread_id
                                && turn_completed.turn.id == turn_id =>
                        {
                            if let Some(turn_error) = turn_completed.turn.error {
                                last_error = Some(turn_error.message);
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {
                    // There are no outstanding client requests at this point.
                }
            }
        }

        if !completed_agent_messages.is_empty() {
            let merged = completed_agent_messages.join("\n\n");
            if !merged.trim().is_empty() {
                return Ok((merged, saw_agent_deltas));
            }
        }

        if !streamed_text.trim().is_empty() {
            return Ok((streamed_text, saw_agent_deltas));
        }

        if let Some(error) = last_error {
            return Ok((format!("Turn failed: {error}"), saw_agent_deltas));
        }

        Ok((
            "Turn completed with no assistant text output.".to_string(),
            saw_agent_deltas,
        ))
    }

    fn handle_server_request(
        &mut self,
        request: JSONRPCRequest,
        approvals: ApprovalPolicy,
    ) -> Result<()> {
        let request_id = request.id.clone();
        match ServerRequest::try_from(request) {
            Ok(ServerRequest::CommandExecutionRequestApproval { request_id, .. }) => {
                let decision = if approvals.auto_approve_commands {
                    CommandExecutionApprovalDecision::Accept
                } else {
                    CommandExecutionApprovalDecision::Decline
                };
                let response = CommandExecutionRequestApprovalResponse { decision };
                self.send_response(request_id, response)
            }
            Ok(ServerRequest::FileChangeRequestApproval { request_id, .. }) => {
                let decision = if approvals.auto_approve_file_changes {
                    FileChangeApprovalDecision::Accept
                } else {
                    FileChangeApprovalDecision::Decline
                };
                let response = FileChangeRequestApprovalResponse { decision };
                self.send_response(request_id, response)
            }
            Ok(ServerRequest::SkillRequestApproval { request_id, .. }) => {
                let decision = if approvals.auto_approve_commands {
                    SkillApprovalDecision::Approve
                } else {
                    SkillApprovalDecision::Decline
                };
                let response = SkillRequestApprovalResponse { decision };
                self.send_response(request_id, response)
            }
            Ok(ServerRequest::ToolRequestUserInput { request_id, params }) => {
                let answers = params
                    .questions
                    .into_iter()
                    .map(|question| {
                        let selected = question
                            .options
                            .and_then(|options| options.into_iter().next())
                            .map(|option| vec![option.label])
                            .unwrap_or_default();
                        (
                            question.id,
                            ToolRequestUserInputAnswer { answers: selected },
                        )
                    })
                    .collect::<HashMap<_, _>>();
                let response = ToolRequestUserInputResponse { answers };
                self.send_response(request_id, response)
            }
            Ok(ServerRequest::DynamicToolCall { request_id, .. }) => {
                let response = DynamicToolCallResponse {
                    content_items: vec![DynamicToolCallOutputContentItem::InputText {
                        text: "telegram bridge does not implement dynamic tool execution"
                            .to_string(),
                    }],
                    success: false,
                };
                self.send_response(request_id, response)
            }
            Ok(ServerRequest::ChatgptAuthTokensRefresh { request_id, .. }) => self.send_error(
                request_id,
                -32603,
                "telegram bridge cannot refresh ChatGPT auth tokens",
            ),
            Ok(ServerRequest::ApplyPatchApproval { request_id, .. })
            | Ok(ServerRequest::ExecCommandApproval { request_id, .. }) => self.send_error(
                request_id,
                -32601,
                "legacy approval request is unsupported by telegram bridge",
            ),
            Err(err) => self.send_error(request_id, -32601, &format!("unsupported request: {err}")),
        }
    }

    fn send_response<T>(&mut self, request_id: RequestId, response: T) -> Result<()>
    where
        T: Serialize,
    {
        let message = JSONRPCMessage::Response(JSONRPCResponse {
            id: request_id,
            result: serde_json::to_value(response).context("serialize request response")?,
        });
        self.write_jsonrpc_message(message)
    }

    fn send_error(&mut self, request_id: RequestId, code: i64, message: &str) -> Result<()> {
        let error = JSONRPCMessage::Error(JSONRPCError {
            id: request_id,
            error: JSONRPCErrorError {
                code,
                message: message.to_string(),
                data: None,
            },
        });
        self.write_jsonrpc_message(error)
    }

    fn send_request<TParams, TResponse>(
        &mut self,
        method: &str,
        params: TParams,
    ) -> Result<TResponse>
    where
        TParams: Serialize,
        TResponse: for<'de> Deserialize<'de>,
    {
        let request_id = RequestId::Integer(self.next_request_id);
        self.next_request_id += 1;

        let request = JSONRPCMessage::Request(JSONRPCRequest {
            id: request_id.clone(),
            method: method.to_string(),
            params: Some(serde_json::to_value(params).context("serialize request params")?),
        });
        self.write_jsonrpc_message(request)?;

        loop {
            let incoming = self.read_jsonrpc_message()?;
            match incoming {
                JSONRPCMessage::Response(response) if response.id == request_id => {
                    return serde_json::from_value(response.result).with_context(|| {
                        format!("deserialize `{method}` response from app-server")
                    });
                }
                JSONRPCMessage::Error(error) if error.id == request_id => {
                    bail!(
                        "app-server `{method}` failed with code {}: {}",
                        error.error.code,
                        error.error.message
                    );
                }
                JSONRPCMessage::Request(request) => {
                    self.handle_server_request(
                        request,
                        ApprovalPolicy {
                            auto_approve_commands: false,
                            auto_approve_file_changes: false,
                        },
                    )?;
                }
                JSONRPCMessage::Notification(_)
                | JSONRPCMessage::Response(_)
                | JSONRPCMessage::Error(_) => {
                    // Ignore out-of-band notifications while waiting for the target response.
                }
            }
        }
    }

    fn write_jsonrpc_message(&mut self, message: JSONRPCMessage) -> Result<()> {
        let payload = serde_json::to_string(&message).context("serialize json-rpc message")?;
        self.socket
            .send(Message::Text(payload.into()))
            .context("send websocket payload")?;
        Ok(())
    }

    fn read_jsonrpc_message(&mut self) -> Result<JSONRPCMessage> {
        loop {
            let frame = self.socket.read().context("read websocket frame")?;
            match frame {
                Message::Text(text) => {
                    return serde_json::from_str::<JSONRPCMessage>(&text)
                        .context("decode app-server JSON-RPC message");
                }
                Message::Close(_) => {
                    bail!("app-server websocket closed the connection");
                }
                Message::Ping(payload) => {
                    self.socket
                        .send(Message::Pong(payload))
                        .context("send websocket pong")?;
                }
                Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
            }
        }
    }
}
