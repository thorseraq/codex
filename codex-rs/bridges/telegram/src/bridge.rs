use crate::state::JsonFileStateStore;
use crate::state::default_state_path;
use crate::types::BridgeConfig;
use crate::types::MockOutput;
use crate::types::TelegramApiClient;
use crate::types::TelegramBridgeArgs;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_bridge_core::AppServerClient;
use codex_bridge_core::BridgeInboundMessage;
use codex_bridge_core::BridgeRuntimeConfig;
use codex_bridge_core::BridgeState;
use codex_bridge_core::BridgeStateStore;
use codex_bridge_core::DEFAULT_CONNECT_RETRY_ATTEMPTS;
use codex_bridge_core::OutboundSender;
use codex_bridge_core::ProcessMessageOutcome;
use codex_bridge_core::complete_started_turn;
use codex_bridge_core::process_message;
use codex_bridge_core::process_message_until_turn_start;
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use tracing::info;
use tracing::warn;

#[derive(Debug, Clone, Default)]
struct SessionStateSnapshot {
    thread_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    active_turn_id: Option<String>,
}

#[derive(Debug)]
struct WorkerStateUpdate {
    session_id: String,
    snapshot: SessionStateSnapshot,
}

#[derive(Debug)]
struct SessionWorkerHandle {
    tx: mpsc::Sender<BridgeInboundMessage>,
}

#[derive(Debug, Clone)]
struct SessionWorkerInit {
    app_server_url: String,
    connect_retry_attempts: u32,
    runtime: BridgeRuntimeConfig,
    token: String,
    api_base_url: String,
    poll_timeout_seconds: u32,
    worker_state_tx: mpsc::Sender<WorkerStateUpdate>,
}

fn session_snapshot_from_global(state: &BridgeState, session_id: &str) -> SessionStateSnapshot {
    SessionStateSnapshot {
        thread_id: state.thread_by_chat_id.get(session_id).cloned(),
        cwd: state.cwd_by_chat_id.get(session_id).cloned(),
        model: state.model_by_chat_id.get(session_id).cloned(),
        effort: state.effort_by_chat_id.get(session_id).cloned(),
        active_turn_id: None,
    }
}

fn session_state_from_snapshot(session_id: &str, snapshot: &SessionStateSnapshot) -> BridgeState {
    let mut state = BridgeState::default();

    if let Some(thread_id) = snapshot.thread_id.clone() {
        state
            .thread_by_chat_id
            .insert(session_id.to_string(), thread_id);
    }
    if let Some(cwd) = snapshot.cwd.clone() {
        state.cwd_by_chat_id.insert(session_id.to_string(), cwd);
    }
    if let Some(model) = snapshot.model.clone() {
        state.model_by_chat_id.insert(session_id.to_string(), model);
    }
    if let Some(effort) = snapshot.effort.clone() {
        state
            .effort_by_chat_id
            .insert(session_id.to_string(), effort);
    }

    state
}

fn snapshot_from_session_state(
    session_id: &str,
    state: &BridgeState,
    active_turn_id: Option<&str>,
) -> SessionStateSnapshot {
    SessionStateSnapshot {
        thread_id: state.thread_by_chat_id.get(session_id).cloned(),
        cwd: state.cwd_by_chat_id.get(session_id).cloned(),
        model: state.model_by_chat_id.get(session_id).cloned(),
        effort: state.effort_by_chat_id.get(session_id).cloned(),
        active_turn_id: active_turn_id.map(str::to_string),
    }
}

fn apply_session_snapshot(
    state: &mut BridgeState,
    session_id: &str,
    snapshot: SessionStateSnapshot,
) {
    let SessionStateSnapshot {
        thread_id,
        cwd,
        model,
        effort,
        ..
    } = snapshot;

    if let Some(thread_id) = thread_id {
        state
            .thread_by_chat_id
            .insert(session_id.to_string(), thread_id);
    } else {
        state.thread_by_chat_id.remove(session_id);
    }

    if let Some(cwd) = cwd {
        state.cwd_by_chat_id.insert(session_id.to_string(), cwd);
    } else {
        state.cwd_by_chat_id.remove(session_id);
    }

    if let Some(model) = model {
        state.model_by_chat_id.insert(session_id.to_string(), model);
    } else {
        state.model_by_chat_id.remove(session_id);
    }

    if let Some(effort) = effort {
        state
            .effort_by_chat_id
            .insert(session_id.to_string(), effort);
    } else {
        state.effort_by_chat_id.remove(session_id);
    }
}

fn apply_worker_state_updates(
    state: &mut BridgeState,
    state_store: &impl BridgeStateStore,
    worker_state_rx: &mpsc::Receiver<WorkerStateUpdate>,
    active_turn_by_session: &mut HashMap<String, String>,
) -> Result<()> {
    let mut changed = false;

    while let Ok(update) = worker_state_rx.try_recv() {
        if let Some(turn_id) = update.snapshot.active_turn_id.clone() {
            active_turn_by_session.insert(update.session_id.clone(), turn_id);
        } else {
            active_turn_by_session.remove(&update.session_id);
        }
        apply_session_snapshot(state, &update.session_id, update.snapshot);
        changed = true;
    }

    if changed {
        state_store.persist_state(state)?;
    }

    Ok(())
}

fn send_worker_state_snapshot(
    worker_state_tx: &mpsc::Sender<WorkerStateUpdate>,
    session_id: &str,
    state: &BridgeState,
    active_turn_id: Option<&str>,
) -> bool {
    let snapshot = snapshot_from_session_state(session_id, state, active_turn_id);
    worker_state_tx
        .send(WorkerStateUpdate {
            session_id: session_id.to_string(),
            snapshot,
        })
        .is_ok()
}

fn spawn_session_worker(
    session_id: String,
    initial_snapshot: SessionStateSnapshot,
    init: SessionWorkerInit,
) -> Result<SessionWorkerHandle> {
    let (tx, rx) = mpsc::channel::<BridgeInboundMessage>();

    let thread_name = format!("telegram-bridge-session-{session_id}");
    let session_id_for_thread = session_id.clone();
    let app_server_url = init.app_server_url;
    let connect_retry_attempts = init.connect_retry_attempts;
    let runtime = init.runtime;
    let token = init.token;
    let api_base_url = init.api_base_url;
    let poll_timeout_seconds = init.poll_timeout_seconds;
    let worker_state_tx = init.worker_state_tx;

    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let mut app_server = match AppServerClient::connect_with_retry(
                &app_server_url,
                connect_retry_attempts,
            ) {
                Ok(client) => client,
                Err(err) => {
                    warn!(
                        session_id = session_id_for_thread,
                        error = %err,
                        "failed to connect app-server for session worker"
                    );
                    return;
                }
            };

            if let Err(err) = app_server.initialize() {
                warn!(
                    session_id = session_id_for_thread,
                    error = %err,
                    "failed to initialize app-server for session worker"
                );
                return;
            }

            let mut outbound =
                match TelegramApiClient::new(token, api_base_url, poll_timeout_seconds) {
                    Ok(client) => client,
                    Err(err) => {
                        warn!(
                            session_id = session_id_for_thread,
                            error = %err,
                            "failed to create telegram client for session worker"
                        );
                        return;
                    }
                };

            let mut state = session_state_from_snapshot(&session_id_for_thread, &initial_snapshot);

            while let Ok(inbound) = rx.recv() {
                match process_message_until_turn_start(
                    &mut app_server,
                    &mut outbound,
                    &mut state,
                    &inbound,
                    &runtime,
                ) {
                    Ok(ProcessMessageOutcome::NoTurnStarted) => {
                        if !send_worker_state_snapshot(
                            &worker_state_tx,
                            &session_id_for_thread,
                            &state,
                            None,
                        ) {
                            break;
                        }
                    }
                    Ok(ProcessMessageOutcome::TurnStarted { thread_id, turn_id }) => {
                        if !send_worker_state_snapshot(
                            &worker_state_tx,
                            &session_id_for_thread,
                            &state,
                            Some(turn_id.as_str()),
                        ) {
                            break;
                        }

                        if let Err(err) = complete_started_turn(
                            &mut app_server,
                            &mut outbound,
                            &session_id_for_thread,
                            &thread_id,
                            &turn_id,
                            &runtime,
                        ) {
                            warn!(
                                session_id = session_id_for_thread,
                                error = %err,
                                "session worker failed to complete started turn"
                            );
                            let _ignored = outbound
                                .send_text(&session_id_for_thread, &format!("Error: {err}"));
                        }

                        if !send_worker_state_snapshot(
                            &worker_state_tx,
                            &session_id_for_thread,
                            &state,
                            None,
                        ) {
                            break;
                        }
                    }
                    Err(err) => {
                        warn!(
                            session_id = session_id_for_thread,
                            error = %err,
                            "session worker failed to process message"
                        );
                        let _ignored =
                            outbound.send_text(&session_id_for_thread, &format!("Error: {err}"));
                        if !send_worker_state_snapshot(
                            &worker_state_tx,
                            &session_id_for_thread,
                            &state,
                            None,
                        ) {
                            break;
                        }
                    }
                }
            }
        })
        .with_context(|| format!("spawn session worker for session `{session_id}`"))?;

    Ok(SessionWorkerHandle { tx })
}

fn is_authorized_source(config: &BridgeConfig, chat_id: i64, sender_user_id: Option<i64>) -> bool {
    if config.allowed_chat_ids.contains(&chat_id) {
        return true;
    }
    sender_user_id.is_some_and(|user_id| config.allowed_user_ids.contains(&user_id))
}

fn command_args<'a>(text: &'a str, command: &str) -> Option<&'a str> {
    let trimmed = text.trim();
    let first_token = trimmed.split_whitespace().next()?;
    if first_token == command || first_token.starts_with(&format!("{command}@")) {
        return Some(trimmed[first_token.len()..].trim());
    }
    None
}

fn is_interrupt_command(text: &str) -> bool {
    command_args(text, "/interrupt").is_some()
        || command_args(text, "/stop").is_some()
        || command_args(text, "/cancel").is_some()
}

fn run_mock_bridge(app_server: &mut AppServerClient, config: &BridgeConfig) -> Result<()> {
    if config.mock_messages.is_empty() {
        bail!(
            "mock mode requires at least one message via --telegram-bridge-mock-message or [[mock_messages]] in config"
        );
    }

    let mut state = BridgeState::default();
    let mut outbound = MockOutput::default();

    for inbound in &config.mock_messages {
        process_message(
            app_server,
            &mut outbound,
            &mut state,
            inbound,
            &config.runtime,
        )?;
    }

    for response in outbound.responses {
        info!(
            session_id = response.session_id,
            text = %response.text,
            "mock bridge reply"
        );
    }

    Ok(())
}

fn run_live_bridge(
    config: &BridgeConfig,
    state_store: &impl BridgeStateStore,
    app_server_url: &str,
    connect_retry_attempts: u32,
) -> Result<()> {
    let token = config
        .token
        .clone()
        .context("missing Telegram bot token in resolved configuration")?;

    let mut state = state_store.load_state()?;
    let telegram = TelegramApiClient::new(
        token.clone(),
        config.api_base_url.clone(),
        config.poll_timeout_seconds,
    )?;

    let (worker_state_tx, worker_state_rx) = mpsc::channel::<WorkerStateUpdate>();
    let mut workers: HashMap<String, SessionWorkerHandle> = HashMap::new();
    let mut active_turn_by_session: HashMap<String, String> = HashMap::new();
    let worker_init = SessionWorkerInit {
        app_server_url: app_server_url.to_string(),
        connect_retry_attempts,
        runtime: config.runtime.clone(),
        token,
        api_base_url: config.api_base_url.clone(),
        poll_timeout_seconds: config.poll_timeout_seconds,
        worker_state_tx,
    };

    loop {
        apply_worker_state_updates(
            &mut state,
            state_store,
            &worker_state_rx,
            &mut active_turn_by_session,
        )?;

        let offset = state
            .last_update_id
            .map(|last_update_id| last_update_id + 1);
        let mut updates = telegram.get_updates(offset)?;
        updates.sort_by_key(|update| update.update_id);

        for update in updates {
            state.last_update_id = Some(update.update_id);
            state_store.persist_state(&state)?;

            apply_worker_state_updates(
                &mut state,
                state_store,
                &worker_state_rx,
                &mut active_turn_by_session,
            )?;

            let Some(message) = update.message else {
                continue;
            };
            let Some(text) = message.text else {
                continue;
            };

            let chat_id = message.chat.id;
            let sender_user_id = message.from.map(|user| user.id);
            if !is_authorized_source(config, chat_id, sender_user_id) {
                warn!(
                    chat_id,
                    sender_user_id = ?sender_user_id,
                    "ignoring message from unauthorized source"
                );
                continue;
            }

            let session_id = chat_id.to_string();

            if is_interrupt_command(&text) {
                let Some(thread_id) = state.thread_by_chat_id.get(&session_id).cloned() else {
                    let _ignored = telegram.send_message(
                        chat_id,
                        "No active conversation for this chat. Send a prompt first.",
                    );
                    continue;
                };

                let Some(turn_id) = active_turn_by_session.get(&session_id).cloned() else {
                    let _ignored = telegram
                        .send_message(chat_id, "No turn is currently running to interrupt.");
                    continue;
                };

                let mut interrupt_client = match AppServerClient::connect_with_retry(
                    app_server_url,
                    connect_retry_attempts,
                ) {
                    Ok(client) => client,
                    Err(err) => {
                        warn!(
                            chat_id,
                            error = %err,
                            "failed to connect app-server for interrupt request"
                        );
                        let _ignored = telegram.send_message(chat_id, &format!("Error: {err}"));
                        continue;
                    }
                };

                if let Err(err) = interrupt_client.initialize() {
                    warn!(
                        chat_id,
                        error = %err,
                        "failed to initialize app-server for interrupt request"
                    );
                    let _ignored = telegram.send_message(chat_id, &format!("Error: {err}"));
                    continue;
                }

                if let Err(err) = interrupt_client.request_turn_interrupt(&thread_id, &turn_id) {
                    warn!(
                        chat_id,
                        error = %err,
                        thread_id = %thread_id,
                        turn_id = %turn_id,
                        "failed to send turn interrupt request"
                    );
                    let _ignored = telegram.send_message(chat_id, &format!("Error: {err}"));
                    continue;
                }

                let _ignored = telegram
                    .send_message(chat_id, &format!("Interrupt requested for turn {turn_id}."));
                continue;
            }

            if !workers.contains_key(&session_id) {
                let initial_snapshot = session_snapshot_from_global(&state, &session_id);
                match spawn_session_worker(
                    session_id.clone(),
                    initial_snapshot,
                    worker_init.clone(),
                ) {
                    Ok(worker) => {
                        workers.insert(session_id.clone(), worker);
                    }
                    Err(err) => {
                        warn!(
                            chat_id,
                            error = %err,
                            "failed to create session worker"
                        );
                        let _ignored = telegram.send_message(chat_id, &format!("Error: {err}"));
                        continue;
                    }
                }
            }

            let inbound = BridgeInboundMessage {
                session_id: session_id.clone(),
                text,
            };

            let mut delivered = false;
            if let Some(worker) = workers.get(&session_id)
                && worker.tx.send(inbound.clone()).is_ok()
            {
                delivered = true;
            }

            if !delivered {
                workers.remove(&session_id);
                let initial_snapshot = session_snapshot_from_global(&state, &session_id);
                match spawn_session_worker(
                    session_id.clone(),
                    initial_snapshot,
                    worker_init.clone(),
                ) {
                    Ok(worker) => {
                        if worker.tx.send(inbound).is_ok() {
                            workers.insert(session_id, worker);
                        } else {
                            warn!(
                                chat_id,
                                "failed to deliver message to restarted session worker"
                            );
                            let _ignored = telegram.send_message(
                                chat_id,
                                "Error: session worker unavailable. Please retry.",
                            );
                        }
                    }
                    Err(err) => {
                        warn!(
                            chat_id,
                            error = %err,
                            "failed to restart session worker"
                        );
                        let _ignored = telegram.send_message(chat_id, &format!("Error: {err}"));
                    }
                }
            }
        }
    }
}

pub fn run(args: TelegramBridgeArgs) -> Result<()> {
    let state_path = args
        .state_path
        .clone()
        .unwrap_or_else(|| default_state_path(&args.config_path));
    let connect_retry_attempts = args
        .connect_retry_attempts
        .unwrap_or(DEFAULT_CONNECT_RETRY_ATTEMPTS);

    let config = BridgeConfig::load(&args)?;

    if args.mock_mode {
        let mut app_server =
            AppServerClient::connect_with_retry(&args.app_server_url, connect_retry_attempts)?;
        app_server.initialize()?;
        return run_mock_bridge(&mut app_server, &config);
    }

    let state_store = JsonFileStateStore::new(state_path);
    run_live_bridge(
        &config,
        &state_store,
        &args.app_server_url,
        connect_retry_attempts,
    )
}

#[cfg(test)]
mod tests {
    use super::command_args;
    use super::is_authorized_source;
    use super::is_interrupt_command;
    use crate::types::BridgeConfig;
    use codex_bridge_core::BridgeRuntimeConfig;
    use std::collections::HashSet;

    fn test_config() -> BridgeConfig {
        BridgeConfig {
            token: None,
            allowed_chat_ids: HashSet::new(),
            allowed_user_ids: HashSet::new(),
            poll_timeout_seconds: 30,
            api_base_url: "https://api.telegram.org".to_string(),
            runtime: BridgeRuntimeConfig::default(),
            mock_messages: Vec::new(),
        }
    }

    #[test]
    fn authorization_accepts_allowed_chat() {
        let mut config = test_config();
        config.allowed_chat_ids.insert(42);

        assert!(is_authorized_source(&config, 42, None));
    }

    #[test]
    fn authorization_accepts_allowed_user_across_chats() {
        let mut config = test_config();
        config.allowed_user_ids.insert(1001);

        assert!(is_authorized_source(&config, -12345, Some(1001)));
    }

    #[test]
    fn authorization_rejects_unknown_chat_and_user() {
        let config = test_config();

        assert!(!is_authorized_source(&config, -12345, Some(1001)));
        assert!(!is_authorized_source(&config, -12345, None));
    }

    #[test]
    fn command_args_handles_bot_mention() {
        assert_eq!(command_args("/interrupt@codex_bot", "/interrupt"), Some(""));
        assert_eq!(
            command_args("/interrupt@codex_bot now", "/interrupt"),
            Some("now")
        );
    }

    #[test]
    fn interrupt_aliases_are_supported() {
        assert!(is_interrupt_command("/interrupt"));
        assert!(is_interrupt_command("/stop"));
        assert!(is_interrupt_command("/cancel"));
        assert!(!is_interrupt_command("/status"));
    }
}
