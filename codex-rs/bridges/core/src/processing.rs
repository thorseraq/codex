use crate::types::AppServerClient;
use crate::types::BridgeInboundMessage;
use crate::types::BridgeRuntimeConfig;
use crate::types::BridgeState;
use crate::types::EffectiveModelEffort;
use crate::types::OutboundSender;
use crate::types::ThreadOverrides;
use anyhow::Result;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::Thread;
use std::path::Path;
use tracing::warn;

const STREAM_DELTA_FLUSH_CHARS: usize = 240;
const STREAM_DELTA_NEWLINE_FLUSH_CHARS: usize = 80;
const DEFAULT_THREAD_LIST_LIMIT: u32 = 10;
const MAX_THREAD_LIST_LIMIT: u32 = 20;
const THREAD_LIST_TITLE_LIMIT: usize = 72;

fn command_args<'a>(text: &'a str, command: &str) -> Option<&'a str> {
    let trimmed = text.trim();
    let first_token = trimmed.split_whitespace().next()?;
    if first_token == command || first_token.starts_with(&format!("{command}@")) {
        return Some(trimmed[first_token.len()..].trim());
    }
    None
}

fn parse_reasoning_effort(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "none" => Some("none".to_string()),
        "minimal" => Some("minimal".to_string()),
        "low" => Some("low".to_string()),
        "medium" => Some("medium".to_string()),
        "high" => Some("high".to_string()),
        "xhigh" | "x-high" | "extra-high" | "extra_high" => Some("xhigh".to_string()),
        _ => None,
    }
}

fn parse_thread_list_limit(args: &str) -> Result<u32, &'static str> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_THREAD_LIST_LIMIT);
    }

    let parsed = trimmed
        .parse::<u32>()
        .map_err(|_| "Invalid list limit. Use /list or /list <1-20>.")?;
    if parsed == 0 {
        return Err("Invalid list limit. Use /list or /list <1-20>.");
    }

    Ok(parsed.min(MAX_THREAD_LIST_LIMIT))
}

fn normalize_inline_text(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_text(raw: &str, max_chars: usize) -> String {
    let total_chars = raw.chars().count();
    if total_chars <= max_chars {
        return raw.to_string();
    }

    let keep = max_chars.saturating_sub(1);
    let truncated = raw.chars().take(keep).collect::<String>();
    format!("{truncated}…")
}

fn format_thread_label(thread: &Thread) -> String {
    let title = thread
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(thread.preview.as_str());
    let normalized = normalize_inline_text(title);
    if normalized.is_empty() {
        return "(untitled thread)".to_string();
    }

    truncate_text(&normalized, THREAD_LIST_TITLE_LIMIT)
}

fn resolve_effective_model_effort(
    state: &BridgeState,
    config: &BridgeRuntimeConfig,
    session_id: &str,
    models: &[Model],
) -> EffectiveModelEffort {
    let default_model = models.iter().find(|model| model.is_default);
    let model_override = state.model_by_chat_id.get(session_id).cloned();
    let effort_override = state.effort_by_chat_id.get(session_id).cloned();

    let (model, model_source) = if let Some(model) = model_override {
        (model, "chat override")
    } else if let Some(model) = config.thread.model.clone() {
        (model, "config default")
    } else if let Some(default_model) = default_model {
        (default_model.id.clone(), "server default")
    } else {
        ("(unknown)".to_string(), "server default")
    };

    let (effort, effort_source) = if let Some(effort) = effort_override {
        (effort, "chat override")
    } else if let Some(effort) = config.thread.reasoning_effort.clone() {
        (effort, "config default")
    } else if let Some(model_entry) = models
        .iter()
        .find(|entry| entry.id == model || entry.model == model)
        .or(default_model)
    {
        (
            model_entry.default_reasoning_effort.to_string(),
            "server default",
        )
    } else {
        ("(unknown)".to_string(), "server default")
    };

    EffectiveModelEffort {
        model,
        model_source,
        effort,
        effort_source,
    }
}

fn ensure_thread_for_chat(
    app_server: &mut AppServerClient,
    state: &mut BridgeState,
    session_id: &str,
    thread_overrides: &ThreadOverrides,
) -> Result<String> {
    if let Some(thread_id) = state.thread_by_chat_id.get(session_id).cloned() {
        match app_server.resume_thread(&thread_id) {
            Ok(_) => return Ok(thread_id),
            Err(err) if is_stale_thread_error(&err) => {
                warn!(
                    session_id,
                    thread_id = %thread_id,
                    error = %err,
                    "stored thread mapping is stale; starting a new thread"
                );
                state.thread_by_chat_id.remove(session_id);
            }
            Err(err) => return Err(err),
        }
    }

    let thread_id = app_server.start_thread(thread_overrides)?;
    state
        .thread_by_chat_id
        .insert(session_id.to_string(), thread_id.clone());
    Ok(thread_id)
}

fn is_stale_thread_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("thread not found") || message.contains("no rollout found for thread id")
    })
}

#[derive(Debug, Default)]
struct StreamDeltaBuffer {
    text: String,
    char_count: usize,
}

impl StreamDeltaBuffer {
    fn push(&mut self, delta: &str) {
        self.text.push_str(delta);
        self.char_count += delta.chars().count();
    }

    fn should_flush_after_delta(&self, delta: &str) -> bool {
        if self.char_count >= STREAM_DELTA_FLUSH_CHARS {
            return true;
        }

        delta.contains('\n') && self.char_count >= STREAM_DELTA_NEWLINE_FLUSH_CHARS
    }

    fn take_chunk(&mut self) -> Option<String> {
        if self.text.is_empty() {
            return None;
        }
        self.char_count = 0;
        Some(std::mem::take(&mut self.text))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessMessageOutcome {
    NoTurnStarted,
    TurnStarted { thread_id: String, turn_id: String },
}

pub fn process_message_until_turn_start(
    app_server: &mut AppServerClient,
    outbound: &mut dyn OutboundSender,
    state: &mut BridgeState,
    inbound: &BridgeInboundMessage,
    config: &BridgeRuntimeConfig,
) -> Result<ProcessMessageOutcome> {
    let text = inbound.text.trim();
    if text.is_empty() {
        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    let session_id = &inbound.session_id;

    let mut thread_overrides = config.thread.clone();
    if let Some(cwd) = state.cwd_by_chat_id.get(session_id) {
        thread_overrides.cwd = Some(cwd.clone());
    }
    if let Some(model) = state.model_by_chat_id.get(session_id) {
        thread_overrides.model = Some(model.clone());
    }
    if let Some(effort) = state.effort_by_chat_id.get(session_id) {
        thread_overrides.reasoning_effort = Some(effort.clone());
    }

    let (cwd_label, cwd_source) = if let Some(cwd) = thread_overrides.cwd.clone() {
        let source = if state.cwd_by_chat_id.contains_key(session_id) {
            "chat override"
        } else {
            "config default"
        };
        (cwd, source)
    } else {
        ("(server default)".to_string(), "server default")
    };
    let (model_label, model_source) = if let Some(model) = thread_overrides.model.clone() {
        let source = if state.model_by_chat_id.contains_key(session_id) {
            "chat override"
        } else {
            "config default"
        };
        (model, source)
    } else {
        ("(server default)".to_string(), "server default")
    };
    let (effort_label, effort_source) =
        if let Some(effort) = thread_overrides.reasoning_effort.clone() {
            let source = if state.effort_by_chat_id.contains_key(session_id) {
                "chat override"
            } else {
                "config default"
            };
            (effort, source)
        } else {
            ("(server default)".to_string(), "server default")
        };

    if command_args(text, "/status").is_some() {
        let thread_id =
            match ensure_thread_for_chat(app_server, state, session_id, &thread_overrides) {
                Ok(thread_id) => thread_id,
                Err(err) => {
                    warn!(
                        session_id,
                        error = %err,
                        "failed to ensure thread for /status"
                    );
                    "(none)".to_string()
                }
            };
        let resolved = match app_server.list_models() {
            Ok(models) => resolve_effective_model_effort(state, config, session_id, &models),
            Err(err) => {
                warn!(
                    session_id,
                    error = %err,
                    "failed to list models for /status"
                );
                EffectiveModelEffort {
                    model: model_label,
                    model_source,
                    effort: effort_label,
                    effort_source,
                }
            }
        };
        outbound.send_text(
            session_id,
            &format!(
                "Bridge is online.\nThread: {thread_id}\nCWD: {cwd_label} ({cwd_source})\nModel: {} ({})\nReasoning effort: {} ({})",
                resolved.model, resolved.model_source, resolved.effort, resolved.effort_source
            ),
        )?;
        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    if let Some(args) = command_args(text, "/fork") {
        if !args.is_empty() {
            outbound.send_text(
                session_id,
                "Use /fork to branch the current conversation. It does not take arguments.",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        let Some(current_thread_id) = state.thread_by_chat_id.get(session_id).cloned() else {
            outbound.send_text(
                session_id,
                "No active conversation for this chat. Send a prompt first.",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        };

        match app_server.fork_thread(&current_thread_id) {
            Ok(new_thread_id) => {
                state
                    .thread_by_chat_id
                    .insert(session_id.to_string(), new_thread_id.clone());
                outbound.send_text(
                    session_id,
                    &format!(
                        "Conversation forked.\nOld thread: {current_thread_id}\nNew thread: {new_thread_id}\nThis chat now continues on the new fork."
                    ),
                )?;
            }
            Err(err) if is_stale_thread_error(&err) => {
                state.thread_by_chat_id.remove(session_id);
                outbound.send_text(
                    session_id,
                    "The current mapped thread no longer exists. Send a prompt first to start a new conversation.",
                )?;
            }
            Err(err) => return Err(err),
        }

        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    if let Some(args) = command_args(text, "/resume") {
        let mut tokens = args.split_whitespace();
        let Some(target_thread_id) = tokens.next() else {
            outbound.send_text(
                session_id,
                "Use /resume <thread-id> to bind this chat to an existing conversation.",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        };
        if tokens.next().is_some() {
            outbound.send_text(
                session_id,
                "Use /resume <thread-id> to bind this chat to an existing conversation.",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        match app_server.resume_thread(target_thread_id) {
            Ok(response) => {
                let resumed_thread_id = response.thread.id;
                state
                    .thread_by_chat_id
                    .insert(session_id.to_string(), resumed_thread_id.clone());

                let resumed_cwd = response.cwd.to_string_lossy().into_owned();
                if resumed_cwd.is_empty() {
                    state.cwd_by_chat_id.remove(session_id);
                } else {
                    state
                        .cwd_by_chat_id
                        .insert(session_id.to_string(), resumed_cwd.clone());
                }

                if response.model.trim().is_empty() {
                    state.model_by_chat_id.remove(session_id);
                } else {
                    state
                        .model_by_chat_id
                        .insert(session_id.to_string(), response.model.clone());
                }

                if let Some(effort) = response.reasoning_effort {
                    state
                        .effort_by_chat_id
                        .insert(session_id.to_string(), effort.to_string());
                } else {
                    state.effort_by_chat_id.remove(session_id);
                }

                let cwd_label = if resumed_cwd.is_empty() {
                    "(server default)".to_string()
                } else {
                    resumed_cwd
                };
                let effort_label = response.reasoning_effort.map_or_else(
                    || "(server default)".to_string(),
                    |effort| effort.to_string(),
                );
                outbound.send_text(
                    session_id,
                    &format!(
                        "Conversation resumed.\nThread: {resumed_thread_id}\nCWD: {cwd_label}\nModel: {}\nReasoning effort: {effort_label}\nThis chat now continues on the selected thread.",
                        response.model
                    ),
                )?;
            }
            Err(err) if is_stale_thread_error(&err) => {
                outbound.send_text(
                    session_id,
                    "Thread not found. Use /list to inspect recent conversations in this CWD.",
                )?;
            }
            Err(err) => return Err(err),
        }

        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    if let Some(args) = command_args(text, "/list") {
        let limit = match parse_thread_list_limit(args) {
            Ok(limit) => limit,
            Err(message) => {
                outbound.send_text(session_id, message)?;
                return Ok(ProcessMessageOutcome::NoTurnStarted);
            }
        };

        let cwd_filter = thread_overrides.cwd.as_deref();
        let threads = app_server.list_threads(limit, cwd_filter)?;
        let current_thread_id = state.thread_by_chat_id.get(session_id).cloned();
        let mut lines = Vec::new();
        lines.push(format!(
            "Current chat thread: {}",
            current_thread_id.as_deref().unwrap_or("(none)")
        ));
        if let Some(cwd) = cwd_filter {
            lines.push(format!("CWD scope: {cwd} ({cwd_source})"));
        } else {
            lines.push("CWD scope: (none; showing all threads)".to_string());
        }
        if threads.is_empty() {
            lines.push(if cwd_filter.is_some() {
                "No stored threads found for this CWD.".to_string()
            } else {
                "No stored threads found.".to_string()
            });
            outbound.send_text(session_id, &lines.join("\n"))?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        if current_thread_id
            .as_deref()
            .is_some_and(|current| !threads.iter().any(|thread| thread.id == current))
        {
            lines.push("Current chat thread is not in this page.".to_string());
        }

        lines.push(if cwd_filter.is_some() {
            format!("Recent threads in this CWD (latest {limit}):")
        } else {
            format!("Recent threads (latest {limit}):")
        });
        for thread in threads {
            let marker = if current_thread_id.as_deref() == Some(thread.id.as_str()) {
                " [current]"
            } else {
                ""
            };
            lines.push(format!(
                "-{marker} {} | {}",
                thread.id,
                format_thread_label(&thread)
            ));
        }

        outbound.send_text(session_id, &lines.join("\n"))?;
        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    if let Some(args) = command_args(text, "/cwd") {
        if args.is_empty() {
            outbound.send_text(
                session_id,
                &format!(
                    "Current CWD: {cwd_label} ({cwd_source}). Use /cwd <absolute-path> to change it."
                ),
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        if matches!(args, "reset" | "default" | "clear") {
            state.cwd_by_chat_id.remove(session_id);
            state.thread_by_chat_id.remove(session_id);
            outbound.send_text(
                session_id,
                "Chat CWD override cleared. Next message will use default CWD and start a new thread.",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        let path = Path::new(args);
        if !path.is_absolute() {
            outbound.send_text(
                session_id,
                "CWD must be an absolute path. Example: /cwd /path/to/workspace",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }
        if !path.is_dir() {
            outbound.send_text(
                session_id,
                &format!("CWD not found or not a directory: {args}"),
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        state
            .cwd_by_chat_id
            .insert(session_id.to_string(), args.to_string());
        state.thread_by_chat_id.remove(session_id);
        outbound.send_text(
            session_id,
            &format!(
                "CWD set to {args}. Conversation reset for this chat; next message starts a new thread."
            ),
        )?;
        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    if let Some(args) = command_args(text, "/model") {
        if args.is_empty() {
            match app_server.list_models() {
                Ok(models) => {
                    let resolved =
                        resolve_effective_model_effort(state, config, session_id, &models);
                    let mut lines = vec![
                        format!(
                            "Current model: {} ({}).",
                            resolved.model, resolved.model_source
                        ),
                        format!(
                            "Current reasoning effort: {} ({}).",
                            resolved.effort, resolved.effort_source
                        ),
                        "Available model selections:".to_string(),
                    ];

                    for model in models {
                        let default_suffix = if model.is_default {
                            " [server default]"
                        } else {
                            ""
                        };
                        lines.push(format!(
                            "- {} (default effort: {}){}",
                            model.id, model.default_reasoning_effort, default_suffix
                        ));
                    }

                    lines.push("Use /model <model-id> to change it.".to_string());
                    outbound.send_text(session_id, &lines.join("\n"))?;
                }
                Err(err) => {
                    warn!(
                        session_id,
                        error = %err,
                        "failed to list models for /model"
                    );
                    outbound.send_text(
                        session_id,
                        &format!(
                            "Current model: {model_label} ({model_source}). Unable to query model list right now. Use /model <model-id> to change it."
                        ),
                    )?;
                }
            }
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        if matches!(args, "reset" | "default" | "clear") {
            state.model_by_chat_id.remove(session_id);
            outbound.send_text(
                session_id,
                "Model override cleared for this chat. Default model will be used.",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        state
            .model_by_chat_id
            .insert(session_id.to_string(), args.to_string());
        outbound.send_text(
            session_id,
            &format!("Model set to {args}. It will apply to the next turn."),
        )?;
        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    if let Some(args) = command_args(text, "/effort") {
        if args.is_empty() {
            let resolved = match app_server.list_models() {
                Ok(models) => resolve_effective_model_effort(state, config, session_id, &models),
                Err(err) => {
                    warn!(
                        session_id,
                        error = %err,
                        "failed to list models for /effort"
                    );
                    EffectiveModelEffort {
                        model: model_label,
                        model_source,
                        effort: effort_label,
                        effort_source,
                    }
                }
            };
            outbound.send_text(
                session_id,
                &format!(
                    "Current reasoning effort: {} ({}). Use /effort <none|minimal|low|medium|high|xhigh> to change it.",
                    resolved.effort, resolved.effort_source
                ),
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        if matches!(args, "reset" | "default" | "clear") {
            state.effort_by_chat_id.remove(session_id);
            outbound.send_text(
                session_id,
                "Reasoning effort override cleared for this chat. Default effort will be used.",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        }

        let Some(effort) = parse_reasoning_effort(args) else {
            outbound.send_text(
                session_id,
                "Invalid effort. Use one of: none, minimal, low, medium, high, xhigh.",
            )?;
            return Ok(ProcessMessageOutcome::NoTurnStarted);
        };

        state
            .effort_by_chat_id
            .insert(session_id.to_string(), effort.clone());
        outbound.send_text(
            session_id,
            &format!("Reasoning effort set to {effort}. It will apply to the next turn."),
        )?;
        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    if command_args(text, "/reset").is_some() {
        state.thread_by_chat_id.remove(session_id);
        outbound.send_text(
            session_id,
            "Conversation reset. The next message will start a new thread.",
        )?;
        return Ok(ProcessMessageOutcome::NoTurnStarted);
    }

    outbound.send_text(
        session_id,
        &format!(
            "Processing your message (cwd: {cwd_label}, model: {model_label}, effort: {effort_label})..."
        ),
    )?;

    let thread_id = ensure_thread_for_chat(app_server, state, session_id, &thread_overrides)?;
    let turn_id = app_server.start_turn_with_overrides(&thread_id, text, &thread_overrides)?;
    Ok(ProcessMessageOutcome::TurnStarted { thread_id, turn_id })
}

pub fn complete_started_turn(
    app_server: &mut AppServerClient,
    outbound: &mut dyn OutboundSender,
    session_id: &str,
    thread_id: &str,
    turn_id: &str,
    config: &BridgeRuntimeConfig,
) -> Result<()> {
    if config.stream_responses {
        if outbound.supports_stream_preview() {
            outbound.begin_stream(session_id)?;
            let mut stream_delta_buffer = StreamDeltaBuffer::default();
            let run_result = app_server.run_turn_until_complete_with_agent_message_deltas(
                thread_id,
                turn_id,
                config.approval_policy(),
                |delta| {
                    stream_delta_buffer.push(delta);
                    if stream_delta_buffer.should_flush_after_delta(delta)
                        && let Some(chunk) = stream_delta_buffer.take_chunk()
                    {
                        outbound.send_stream_preview(session_id, &chunk)?;
                    }
                    Ok(())
                },
            );
            if let Some(chunk) = stream_delta_buffer.take_chunk() {
                outbound.send_stream_preview(session_id, &chunk)?;
            }
            outbound.complete_stream(session_id)?;
            let (response_text, _saw_agent_deltas) = run_result?;
            outbound.send_text(session_id, &response_text)?;
            return Ok(());
        }

        let mut stream_delta_buffer = StreamDeltaBuffer::default();
        let mut pushed_stream_chunk = false;
        let mut streamed_output = String::new();
        let (response_text, _saw_agent_deltas) = app_server
            .run_turn_until_complete_with_agent_message_deltas(
                thread_id,
                turn_id,
                config.approval_policy(),
                |delta| {
                    streamed_output.push_str(delta);
                    stream_delta_buffer.push(delta);
                    if stream_delta_buffer.should_flush_after_delta(delta)
                        && let Some(chunk) = stream_delta_buffer.take_chunk()
                    {
                        outbound.send_text(session_id, &chunk)?;
                        pushed_stream_chunk = true;
                    }
                    Ok(())
                },
            )?;

        if let Some(chunk) = stream_delta_buffer.take_chunk() {
            outbound.send_text(session_id, &chunk)?;
            pushed_stream_chunk = true;
        }

        if !pushed_stream_chunk || streamed_output.trim() != response_text.trim() {
            outbound.send_text(session_id, &response_text)?;
        }
        return Ok(());
    }

    let response_text =
        app_server.run_turn_until_complete(thread_id, turn_id, config.approval_policy())?;
    outbound.send_text(session_id, &response_text)?;
    Ok(())
}

pub fn process_message(
    app_server: &mut AppServerClient,
    outbound: &mut dyn OutboundSender,
    state: &mut BridgeState,
    inbound: &BridgeInboundMessage,
    config: &BridgeRuntimeConfig,
) -> Result<()> {
    match process_message_until_turn_start(app_server, outbound, state, inbound, config)? {
        ProcessMessageOutcome::NoTurnStarted => {}
        ProcessMessageOutcome::TurnStarted { thread_id, turn_id } => {
            complete_started_turn(
                app_server,
                outbound,
                &inbound.session_id,
                &thread_id,
                &turn_id,
                config,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_THREAD_LIST_LIMIT;
    use super::MAX_THREAD_LIST_LIMIT;
    use super::STREAM_DELTA_FLUSH_CHARS;
    use super::STREAM_DELTA_NEWLINE_FLUSH_CHARS;
    use super::StreamDeltaBuffer;
    use super::THREAD_LIST_TITLE_LIMIT;
    use super::command_args;
    use super::format_thread_label;
    use super::is_stale_thread_error;
    use super::parse_reasoning_effort;
    use super::parse_thread_list_limit;
    use anyhow::anyhow;
    use codex_app_server_protocol::SessionSource;
    use codex_app_server_protocol::Thread;
    use codex_app_server_protocol::ThreadStatus;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn stale_thread_error_matches_thread_not_found() {
        let err =
            anyhow!("app-server `turn/start` failed with code -32600: thread not found: 019abc");
        assert!(is_stale_thread_error(&err));
    }

    #[test]
    fn stale_thread_error_matches_missing_rollout() {
        let err = anyhow!(
            "app-server `thread/resume` failed with code -32600: no rollout found for thread id 019abc"
        );
        assert!(is_stale_thread_error(&err));
    }

    #[test]
    fn stale_thread_error_ignores_other_failures() {
        let err = anyhow!("app-server `turn/start` failed with code -32603: websocket closed");
        assert!(!is_stale_thread_error(&err));
    }

    #[test]
    fn command_args_handles_plain_command() {
        let args = command_args("/cwd /tmp/path", "/cwd");
        assert_eq!(args, Some("/tmp/path"));
    }

    #[test]
    fn command_args_handles_telegram_bot_mention() {
        let args = command_args("/cwd@codex111_bot /tmp/path", "/cwd");
        assert_eq!(args, Some("/tmp/path"));
    }

    #[test]
    fn command_args_rejects_non_matching_command() {
        let args = command_args("/status", "/cwd");
        assert_eq!(args, None);
    }

    #[test]
    fn parse_reasoning_effort_accepts_expected_values() {
        assert_eq!(parse_reasoning_effort("none"), Some("none".to_string()));
        assert_eq!(
            parse_reasoning_effort("minimal"),
            Some("minimal".to_string())
        );
        assert_eq!(parse_reasoning_effort("low"), Some("low".to_string()));
        assert_eq!(parse_reasoning_effort("medium"), Some("medium".to_string()));
        assert_eq!(parse_reasoning_effort("high"), Some("high".to_string()));
        assert_eq!(parse_reasoning_effort("xhigh"), Some("xhigh".to_string()));
        assert_eq!(
            parse_reasoning_effort("extra-high"),
            Some("xhigh".to_string())
        );
    }

    #[test]
    fn parse_reasoning_effort_rejects_unknown_value() {
        assert_eq!(parse_reasoning_effort("ultra"), None);
    }

    #[test]
    fn parse_thread_list_limit_defaults_and_clamps() {
        assert_eq!(
            parse_thread_list_limit("").expect("default list limit"),
            DEFAULT_THREAD_LIST_LIMIT
        );
        assert_eq!(
            parse_thread_list_limit("999").expect("clamped list limit"),
            MAX_THREAD_LIST_LIMIT
        );
    }

    #[test]
    fn parse_thread_list_limit_rejects_invalid_values() {
        assert!(parse_thread_list_limit("0").is_err());
        assert!(parse_thread_list_limit("abc").is_err());
    }

    #[test]
    fn format_thread_label_prefers_name_and_truncates() {
        let thread = Thread {
            id: "thread-1".to_string(),
            preview: "preview text".to_string(),
            ephemeral: false,
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at: 0,
            status: ThreadStatus::Idle,
            path: None,
            cwd: PathBuf::from("/tmp"),
            cli_version: "test".to_string(),
            source: SessionSource::AppServer,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: Some("x".repeat(THREAD_LIST_TITLE_LIMIT + 8)),
            turns: Vec::new(),
        };

        assert_eq!(
            format_thread_label(&thread),
            format!("{}…", "x".repeat(THREAD_LIST_TITLE_LIMIT - 1))
        );
    }

    #[test]
    fn format_thread_label_falls_back_to_preview() {
        let thread = Thread {
            id: "thread-1".to_string(),
            preview: "preview\nwith   spacing".to_string(),
            ephemeral: false,
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at: 0,
            status: ThreadStatus::Idle,
            path: None,
            cwd: PathBuf::from("/tmp"),
            cli_version: "test".to_string(),
            source: SessionSource::AppServer,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: None,
            turns: Vec::new(),
        };

        assert_eq!(format_thread_label(&thread), "preview with spacing");
    }

    #[test]
    fn stream_delta_buffer_flushes_on_char_threshold() {
        let mut buffer = StreamDeltaBuffer::default();
        let delta = "x".repeat(STREAM_DELTA_FLUSH_CHARS);
        buffer.push(&delta);

        assert!(buffer.should_flush_after_delta(&delta));
        assert_eq!(buffer.take_chunk(), Some(delta));
        assert_eq!(buffer.take_chunk(), None);
    }

    #[test]
    fn stream_delta_buffer_flushes_on_newline_threshold() {
        let mut buffer = StreamDeltaBuffer::default();
        let delta = format!("{}\n", "a".repeat(STREAM_DELTA_NEWLINE_FLUSH_CHARS));
        buffer.push(&delta);

        assert!(buffer.should_flush_after_delta(&delta));
        assert_eq!(buffer.take_chunk(), Some(delta));
    }

    #[test]
    fn stream_delta_buffer_keeps_small_non_newline_chunks() {
        let mut buffer = StreamDeltaBuffer::default();
        let delta = "tiny";
        buffer.push(delta);

        assert!(!buffer.should_flush_after_delta(delta));
        assert_eq!(buffer.take_chunk(), Some(delta.to_string()));
    }
}
