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
use codex_bridge_core::process_message;
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

fn snapshot_from_session_state(session_id: &str, state: &BridgeState) -> SessionStateSnapshot {
    SessionStateSnapshot {
        thread_id: state.thread_by_chat_id.get(session_id).cloned(),
        cwd: state.cwd_by_chat_id.get(session_id).cloned(),
        model: state.model_by_chat_id.get(session_id).cloned(),
        effort: state.effort_by_chat_id.get(session_id).cloned(),
    }
}

fn apply_session_snapshot(
    state: &mut BridgeState,
    session_id: &str,
    snapshot: SessionStateSnapshot,
) {
    if let Some(thread_id) = snapshot.thread_id {
        state
            .thread_by_chat_id
            .insert(session_id.to_string(), thread_id);
    } else {
        state.thread_by_chat_id.remove(session_id);
    }

    if let Some(cwd) = snapshot.cwd {
        state.cwd_by_chat_id.insert(session_id.to_string(), cwd);
    } else {
        state.cwd_by_chat_id.remove(session_id);
    }

    if let Some(model) = snapshot.model {
        state.model_by_chat_id.insert(session_id.to_string(), model);
    } else {
        state.model_by_chat_id.remove(session_id);
    }

    if let Some(effort) = snapshot.effort {
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
) -> Result<()> {
    let mut changed = false;

    while let Ok(update) = worker_state_rx.try_recv() {
        apply_session_snapshot(state, &update.session_id, update.snapshot);
        changed = true;
    }

    if changed {
        state_store.persist_state(state)?;
    }

    Ok(())
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
                if let Err(err) = process_message(
                    &mut app_server,
                    &mut outbound,
                    &mut state,
                    &inbound,
                    &runtime,
                ) {
                    warn!(
                        session_id = session_id_for_thread,
                        error = %err,
                        "session worker failed to process message"
                    );
                    let _ignored =
                        outbound.send_text(&session_id_for_thread, &format!("Error: {err}"));
                }

                let snapshot = snapshot_from_session_state(&session_id_for_thread, &state);
                if worker_state_tx
                    .send(WorkerStateUpdate {
                        session_id: session_id_for_thread.clone(),
                        snapshot,
                    })
                    .is_err()
                {
                    break;
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
        apply_worker_state_updates(&mut state, state_store, &worker_state_rx)?;

        let offset = state
            .last_update_id
            .map(|last_update_id| last_update_id + 1);
        let mut updates = telegram.get_updates(offset)?;
        updates.sort_by_key(|update| update.update_id);

        for update in updates {
            state.last_update_id = Some(update.update_id);
            state_store.persist_state(&state)?;

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
    use super::is_authorized_source;
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
}
