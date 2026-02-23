use crate::types::AppServerClient;
use crate::types::BridgeInboundMessage;
use crate::types::BridgeRuntimeConfig;
use crate::types::BridgeState;
use crate::types::EffectiveModelEffort;
use crate::types::OutboundSender;
use crate::types::ThreadOverrides;
use anyhow::Result;
use codex_app_server_protocol::Model;
use std::path::Path;
use tracing::warn;

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
            Ok(()) => return Ok(thread_id),
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

pub fn process_message(
    app_server: &mut AppServerClient,
    outbound: &mut dyn OutboundSender,
    state: &mut BridgeState,
    inbound: &BridgeInboundMessage,
    config: &BridgeRuntimeConfig,
) -> Result<()> {
    let text = inbound.text.trim();
    if text.is_empty() {
        return Ok(());
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
        return Ok(());
    }

    if let Some(args) = command_args(text, "/cwd") {
        if args.is_empty() {
            outbound.send_text(
                session_id,
                &format!(
                    "Current CWD: {cwd_label} ({cwd_source}). Use /cwd <absolute-path> to change it."
                ),
            )?;
            return Ok(());
        }

        if matches!(args, "reset" | "default" | "clear") {
            state.cwd_by_chat_id.remove(session_id);
            state.thread_by_chat_id.remove(session_id);
            outbound.send_text(
                session_id,
                "Chat CWD override cleared. Next message will use default CWD and start a new thread.",
            )?;
            return Ok(());
        }

        let path = Path::new(args);
        if !path.is_absolute() {
            outbound.send_text(
                session_id,
                "CWD must be an absolute path. Example: /cwd /Users/titanma/develop/github/codex-fork",
            )?;
            return Ok(());
        }
        if !path.is_dir() {
            outbound.send_text(
                session_id,
                &format!("CWD not found or not a directory: {args}"),
            )?;
            return Ok(());
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
        return Ok(());
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
            return Ok(());
        }

        if matches!(args, "reset" | "default" | "clear") {
            state.model_by_chat_id.remove(session_id);
            outbound.send_text(
                session_id,
                "Model override cleared for this chat. Default model will be used.",
            )?;
            return Ok(());
        }

        state
            .model_by_chat_id
            .insert(session_id.to_string(), args.to_string());
        outbound.send_text(
            session_id,
            &format!("Model set to {args}. It will apply to the next turn."),
        )?;
        return Ok(());
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
            return Ok(());
        }

        if matches!(args, "reset" | "default" | "clear") {
            state.effort_by_chat_id.remove(session_id);
            outbound.send_text(
                session_id,
                "Reasoning effort override cleared for this chat. Default effort will be used.",
            )?;
            return Ok(());
        }

        let Some(effort) = parse_reasoning_effort(args) else {
            outbound.send_text(
                session_id,
                "Invalid effort. Use one of: none, minimal, low, medium, high, xhigh.",
            )?;
            return Ok(());
        };

        state
            .effort_by_chat_id
            .insert(session_id.to_string(), effort.clone());
        outbound.send_text(
            session_id,
            &format!("Reasoning effort set to {effort}. It will apply to the next turn."),
        )?;
        return Ok(());
    }

    if command_args(text, "/reset").is_some() {
        state.thread_by_chat_id.remove(session_id);
        outbound.send_text(
            session_id,
            "Conversation reset. The next message will start a new thread.",
        )?;
        return Ok(());
    }

    outbound.send_text(
        session_id,
        &format!(
            "Processing your message (cwd: {cwd_label}, model: {model_label}, effort: {effort_label})..."
        ),
    )?;

    let thread_id = ensure_thread_for_chat(app_server, state, session_id, &thread_overrides)?;
    let turn_id = app_server.start_turn_with_overrides(&thread_id, text, &thread_overrides)?;
    let response_text =
        app_server.run_turn_until_complete(&thread_id, &turn_id, config.approval_policy())?;
    outbound.send_text(session_id, &response_text)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::command_args;
    use super::is_stale_thread_error;
    use super::parse_reasoning_effort;
    use anyhow::anyhow;
    use pretty_assertions::assert_eq;

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
}
