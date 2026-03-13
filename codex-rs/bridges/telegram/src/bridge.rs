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
use codex_bridge_core::process_message_until_turn_start;
use reqwest::StatusCode;
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tracing::info;
use tracing::warn;

const APP_SERVER_CONNECTION_TTL: Duration = Duration::from_secs(30);
const TELEGRAM_POLL_RETRY_MAX_DELAY: Duration = Duration::from_secs(16);

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

#[derive(Debug)]
struct CachedAppServerClient {
    client: AppServerClient,
    last_used_at: Instant,
}

#[derive(Debug)]
struct SessionAppServerClientManager {
    app_server_url: String,
    connect_retry_attempts: u32,
    idle_ttl: Duration,
    cached: Option<CachedAppServerClient>,
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

fn connect_initialized_app_server(
    app_server_url: &str,
    connect_retry_attempts: u32,
) -> Result<AppServerClient> {
    let mut app_server =
        AppServerClient::connect_with_retry(app_server_url, connect_retry_attempts)?;
    app_server.initialize()?;
    Ok(app_server)
}

fn app_server_connection_is_stale(last_used_at: Instant, now: Instant, idle_ttl: Duration) -> bool {
    now.duration_since(last_used_at) >= idle_ttl
}

fn is_app_server_transport_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("send websocket payload")
            || message.contains("send websocket pong")
            || message.contains("read websocket frame")
            || message.contains("app-server websocket closed the connection")
            || message.contains("Connection reset without closing handshake")
            || message.contains("WebSocket protocol error")
            || message.contains("Broken pipe")
    })
}

fn is_retryable_telegram_poll_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let message = cause.to_string();
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|reqwest_err| {
                reqwest_err.is_timeout()
                    || reqwest_err.is_connect()
                    || (reqwest_err.is_status()
                        && reqwest_err.status().is_some_and(|status| {
                            status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
                        }))
            })
            || message.contains("error sending request")
            || message.contains("client error (SendRequest)")
            || message.contains("connection closed before message completed")
            || message.contains("error reading a body from connection")
            || message.contains("end of file before message length reached")
    })
}

fn get_updates_with_retry(
    telegram: &mut TelegramApiClient,
    offset: Option<i64>,
    consecutive_failures: &mut u32,
) -> Result<Vec<crate::types::TelegramUpdate>> {
    loop {
        match telegram.get_updates(offset) {
            Ok(updates) => {
                *consecutive_failures = 0;
                return Ok(updates);
            }
            Err(err) => {
                if !is_retryable_telegram_poll_error(&err) {
                    return Err(err);
                }

                *consecutive_failures = consecutive_failures.saturating_add(1);
                let retry_delay = Duration::from_secs(
                    (1_u64 << (*consecutive_failures).saturating_sub(1).min(4))
                        .min(TELEGRAM_POLL_RETRY_MAX_DELAY.as_secs()),
                );
                warn!(
                    error = %err,
                    offset = ?offset,
                    consecutive_failures = *consecutive_failures,
                    retry_delay_secs = retry_delay.as_secs(),
                    "Telegram getUpdates failed; recreating client and retrying"
                );
                thread::sleep(retry_delay);
                let reconnected = telegram
                    .reconnect()
                    .context("recreate telegram http client after polling failure")?;
                *telegram = reconnected;
            }
        }
    }
}

impl SessionAppServerClientManager {
    fn new(app_server_url: String, connect_retry_attempts: u32, idle_ttl: Duration) -> Self {
        Self {
            app_server_url,
            connect_retry_attempts,
            idle_ttl,
            cached: None,
        }
    }

    fn client_for_new_message(&mut self) -> Result<&mut AppServerClient> {
        let now = Instant::now();
        let should_refresh = self.cached.as_ref().is_none_or(|cached| {
            app_server_connection_is_stale(cached.last_used_at, now, self.idle_ttl)
        });

        if should_refresh {
            self.cached = Some(CachedAppServerClient {
                client: connect_initialized_app_server(
                    &self.app_server_url,
                    self.connect_retry_attempts,
                )?,
                last_used_at: now,
            });
        }

        let cached = self
            .cached
            .as_mut()
            .context("cached app-server client missing after refresh")?;

        Ok(&mut cached.client)
    }

    fn client_for_active_turn(&mut self) -> Result<&mut AppServerClient> {
        if self.cached.is_none() {
            self.cached = Some(CachedAppServerClient {
                client: connect_initialized_app_server(
                    &self.app_server_url,
                    self.connect_retry_attempts,
                )?,
                last_used_at: Instant::now(),
            });
        }

        let cached = self
            .cached
            .as_mut()
            .context("cached app-server client missing for active turn")?;

        Ok(&mut cached.client)
    }

    fn mark_used_now(&mut self) {
        if let Some(cached) = self.cached.as_mut() {
            cached.last_used_at = Instant::now();
        }
    }

    fn invalidate(&mut self) {
        self.cached = None;
    }
}

fn process_message_until_turn_start_with_app_server(
    app_server_manager: &mut SessionAppServerClientManager,
    outbound: &mut dyn OutboundSender,
    state: &mut BridgeState,
    inbound: &BridgeInboundMessage,
    runtime: &BridgeRuntimeConfig,
) -> Result<ProcessMessageOutcome> {
    let result = {
        let app_server = app_server_manager.client_for_new_message()?;
        process_message_until_turn_start(app_server, outbound, state, inbound, runtime)
    };

    match result {
        Ok(outcome) => {
            app_server_manager.mark_used_now();
            Ok(outcome)
        }
        Err(err) => {
            if is_app_server_transport_error(&err) {
                app_server_manager.invalidate();
            }
            Err(err)
        }
    }
}

fn complete_started_turn_with_app_server(
    app_server_manager: &mut SessionAppServerClientManager,
    outbound: &mut dyn OutboundSender,
    session_id: &str,
    thread_id: &str,
    turn_id: &str,
    runtime: &BridgeRuntimeConfig,
) -> Result<()> {
    let result = {
        let app_server = app_server_manager.client_for_active_turn()?;
        complete_started_turn(
            app_server, outbound, session_id, thread_id, turn_id, runtime,
        )
    };

    match result {
        Ok(()) => {
            app_server_manager.mark_used_now();
            Ok(())
        }
        Err(err) => {
            if is_app_server_transport_error(&err) {
                app_server_manager.invalidate();
            }
            Err(err)
        }
    }
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
            let mut app_server_manager = SessionAppServerClientManager::new(
                app_server_url,
                connect_retry_attempts,
                APP_SERVER_CONNECTION_TTL,
            );

            while let Ok(inbound) = rx.recv() {
                match process_message_until_turn_start_with_app_server(
                    &mut app_server_manager,
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

                        if let Err(err) = complete_started_turn_with_app_server(
                            &mut app_server_manager,
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

fn session_id_for_telegram_chat(chat_id: i64, message_thread_id: Option<i64>) -> String {
    if let Some(message_thread_id) = message_thread_id.filter(|thread_id| *thread_id > 0) {
        return format!("{chat_id}:{message_thread_id}");
    }

    chat_id.to_string()
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

fn is_restart_command(text: &str) -> bool {
    command_args(text, "/restart").is_some()
}

fn run_mock_bridge(
    config: &BridgeConfig,
    app_server_url: &str,
    connect_retry_attempts: u32,
) -> Result<MockOutput> {
    run_mock_bridge_with_connection_ttl(
        config,
        app_server_url,
        connect_retry_attempts,
        APP_SERVER_CONNECTION_TTL,
    )
}

fn run_mock_bridge_with_connection_ttl(
    config: &BridgeConfig,
    app_server_url: &str,
    connect_retry_attempts: u32,
    connection_ttl: Duration,
) -> Result<MockOutput> {
    if config.mock_messages.is_empty() {
        bail!(
            "mock mode requires at least one message via --telegram-bridge-mock-message or [[mock_messages]] in config"
        );
    }

    let mut state = BridgeState::default();
    let mut outbound = MockOutput::default();
    let mut app_server_manager = SessionAppServerClientManager::new(
        app_server_url.to_string(),
        connect_retry_attempts,
        connection_ttl,
    );

    for inbound in &config.mock_messages {
        if is_restart_command(&inbound.text) {
            app_server_manager.invalidate();
            outbound.send_text(
                &inbound.session_id,
                "Bridge restart complete. Session workers and app-server connections were refreshed. Current chat state was kept.",
            )?;
            continue;
        }

        match process_message_until_turn_start_with_app_server(
            &mut app_server_manager,
            &mut outbound,
            &mut state,
            inbound,
            &config.runtime,
        )? {
            ProcessMessageOutcome::NoTurnStarted => {}
            ProcessMessageOutcome::TurnStarted { thread_id, turn_id } => {
                complete_started_turn_with_app_server(
                    &mut app_server_manager,
                    &mut outbound,
                    &inbound.session_id,
                    &thread_id,
                    &turn_id,
                    &config.runtime,
                )?;
            }
        }
    }

    Ok(outbound)
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
    let mut telegram = TelegramApiClient::new(
        token.clone(),
        config.api_base_url.clone(),
        config.poll_timeout_seconds,
    )?;
    let mut telegram_poll_failures = 0_u32;

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
        let mut updates =
            get_updates_with_retry(&mut telegram, offset, &mut telegram_poll_failures)?;
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
            let message_thread_id = message.message_thread_id.filter(|thread_id| *thread_id > 0);
            let sender_user_id = message.from.map(|user| user.id);
            if !is_authorized_source(config, chat_id, sender_user_id) {
                warn!(
                    chat_id,
                    sender_user_id = ?sender_user_id,
                    "ignoring message from unauthorized source"
                );
                continue;
            }

            let session_id = session_id_for_telegram_chat(chat_id, message_thread_id);

            if is_interrupt_command(&text) {
                let Some(thread_id) = state.thread_by_chat_id.get(&session_id).cloned() else {
                    let _ignored = telegram.send_message(
                        chat_id,
                        message_thread_id,
                        "No active conversation for this chat. Send a prompt first.",
                    );
                    continue;
                };

                let Some(turn_id) = active_turn_by_session.get(&session_id).cloned() else {
                    let _ignored = telegram.send_message(
                        chat_id,
                        message_thread_id,
                        "No turn is currently running to interrupt.",
                    );
                    continue;
                };

                let mut interrupt_client =
                    match connect_initialized_app_server(app_server_url, connect_retry_attempts) {
                        Ok(client) => client,
                        Err(err) => {
                            warn!(
                                chat_id,
                                error = %err,
                                "failed to prepare app-server for interrupt request"
                            );
                            let _ignored = telegram.send_message(
                                chat_id,
                                message_thread_id,
                                &format!("Error: {err}"),
                            );
                            continue;
                        }
                    };

                if let Err(err) = interrupt_client.request_turn_interrupt(&thread_id, &turn_id) {
                    warn!(
                        chat_id,
                        error = %err,
                        thread_id = %thread_id,
                        turn_id = %turn_id,
                        "failed to send turn interrupt request"
                    );
                    let _ignored =
                        telegram.send_message(chat_id, message_thread_id, &format!("Error: {err}"));
                    continue;
                }

                let _ignored = telegram.send_message(
                    chat_id,
                    message_thread_id,
                    &format!("Interrupt requested for turn {turn_id}."),
                );
                continue;
            }

            if is_restart_command(&text) {
                workers.clear();
                active_turn_by_session.clear();
                telegram_poll_failures = 0;

                let reconnected = match telegram.reconnect() {
                    Ok(client) => client,
                    Err(err) => {
                        warn!(
                            chat_id,
                            error = %err,
                            "failed to recreate telegram client during restart"
                        );
                        let _ignored = telegram.send_message(
                            chat_id,
                            message_thread_id,
                            &format!("Restart failed while recreating Telegram client: {err}"),
                        );
                        continue;
                    }
                };
                telegram = reconnected;

                match state_store.load_state() {
                    Ok(reloaded_state) => {
                        state = reloaded_state;
                        let _ignored = telegram.send_message(
                            chat_id,
                            message_thread_id,
                            "Bridge restart complete. Session workers and app-server connections were refreshed. Persisted chat state was reloaded.",
                        );
                    }
                    Err(err) => {
                        warn!(
                            chat_id,
                            error = %err,
                            "failed to reload bridge state during restart"
                        );
                        let _ignored = telegram.send_message(
                            chat_id,
                            message_thread_id,
                            &format!("Restart failed while reloading bridge state: {err}"),
                        );
                    }
                }
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
                        let _ignored = telegram.send_message(
                            chat_id,
                            message_thread_id,
                            &format!("Error: {err}"),
                        );
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
                                message_thread_id,
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
                        let _ignored = telegram.send_message(
                            chat_id,
                            message_thread_id,
                            &format!("Error: {err}"),
                        );
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
        let outbound = run_mock_bridge(&config, &args.app_server_url, connect_retry_attempts)?;
        for response in outbound.responses {
            info!(
                session_id = response.session_id,
                text = %response.text,
                "mock bridge reply"
            );
        }
        return Ok(());
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
    use super::app_server_connection_is_stale;
    use super::command_args;
    use super::get_updates_with_retry;
    use super::is_app_server_transport_error;
    use super::is_authorized_source;
    use super::is_interrupt_command;
    use super::is_restart_command;
    use super::is_retryable_telegram_poll_error;
    use super::run_mock_bridge;
    use super::run_mock_bridge_with_connection_ttl;
    use super::session_id_for_telegram_chat;
    use crate::types::BridgeConfig;
    use crate::types::TelegramApiClient;
    use codex_app_server_protocol::AskForApproval;
    use codex_app_server_protocol::InitializeResponse;
    use codex_app_server_protocol::ItemCompletedNotification;
    use codex_app_server_protocol::JSONRPCMessage;
    use codex_app_server_protocol::JSONRPCNotification;
    use codex_app_server_protocol::JSONRPCRequest;
    use codex_app_server_protocol::JSONRPCResponse;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::SandboxPolicy;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::SessionSource;
    use codex_app_server_protocol::Thread;
    use codex_app_server_protocol::ThreadForkResponse;
    use codex_app_server_protocol::ThreadItem;
    use codex_app_server_protocol::ThreadListResponse;
    use codex_app_server_protocol::ThreadResumeResponse;
    use codex_app_server_protocol::ThreadStartResponse;
    use codex_app_server_protocol::ThreadStatus;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnStartResponse;
    use codex_app_server_protocol::TurnStatus;
    use codex_bridge_core::BridgeInboundMessage;
    use codex_bridge_core::BridgeRuntimeConfig;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;
    use std::io::ErrorKind;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;
    use tungstenite::Message;
    use tungstenite::WebSocket;
    use tungstenite::accept;

    enum MockTelegramPollResponse {
        Hang(Duration),
        Response {
            status_line: &'static str,
            body: String,
        },
        TruncatedResponse {
            status_line: &'static str,
            body_prefix: String,
            declared_content_length: usize,
        },
    }

    fn build_thread(thread_id: &str) -> Thread {
        build_thread_with_metadata_and_cwd(thread_id, "", None, 0, "/tmp")
    }

    fn build_thread_with_metadata(
        thread_id: &str,
        preview: &str,
        name: Option<&str>,
        updated_at: i64,
    ) -> Thread {
        build_thread_with_metadata_and_cwd(thread_id, preview, name, updated_at, "/tmp")
    }

    fn build_thread_with_metadata_and_cwd(
        thread_id: &str,
        preview: &str,
        name: Option<&str>,
        updated_at: i64,
        cwd: &str,
    ) -> Thread {
        Thread {
            id: thread_id.to_string(),
            preview: preview.to_string(),
            ephemeral: false,
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at,
            status: ThreadStatus::Idle,
            path: None,
            cwd: PathBuf::from(cwd),
            cli_version: "test".to_string(),
            source: SessionSource::AppServer,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: name.map(str::to_string),
            turns: Vec::new(),
        }
    }

    fn read_jsonrpc_message(socket: &mut WebSocket<TcpStream>) -> JSONRPCMessage {
        loop {
            match socket.read().expect("read websocket message") {
                Message::Text(text) => {
                    return serde_json::from_str(&text).expect("decode JSON-RPC message");
                }
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .expect("reply to ping from bridge client");
                }
                Message::Close(frame) => panic!("bridge client closed websocket: {frame:?}"),
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }

    fn write_jsonrpc_message(socket: &mut WebSocket<TcpStream>, message: JSONRPCMessage) {
        let payload = serde_json::to_string(&message).expect("serialize JSON-RPC message");
        socket
            .send(Message::Text(payload.into()))
            .expect("send JSON-RPC message");
    }

    fn send_response<T>(socket: &mut WebSocket<TcpStream>, request_id: RequestId, response: T)
    where
        T: serde::Serialize,
    {
        write_jsonrpc_message(
            socket,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: request_id,
                result: serde_json::to_value(response).expect("serialize JSON-RPC response"),
            }),
        );
    }

    fn send_notification(socket: &mut WebSocket<TcpStream>, notification: ServerNotification) {
        let notification: JSONRPCNotification = serde_json::from_value(
            serde_json::to_value(notification).expect("serialize notification"),
        )
        .expect("decode JSONRPCNotification");
        write_jsonrpc_message(socket, JSONRPCMessage::Notification(notification));
    }

    fn expect_request(socket: &mut WebSocket<TcpStream>, method: &str) -> JSONRPCRequest {
        match read_jsonrpc_message(socket) {
            JSONRPCMessage::Request(request) => {
                assert_eq!(request.method, method);
                request
            }
            other => panic!("expected JSON-RPC request `{method}`, got {other:?}"),
        }
    }

    fn complete_turn_exchange(
        socket: &mut WebSocket<TcpStream>,
        expect_initialize: bool,
        expected_thread_id: Option<&str>,
        thread_id: &str,
        turn_id: &str,
        reply_text: &str,
    ) {
        if expect_initialize {
            let initialize = expect_request(socket, "initialize");
            send_response(
                socket,
                initialize.id,
                InitializeResponse {
                    user_agent: "test-agent".to_string(),
                },
            );

            match read_jsonrpc_message(socket) {
                JSONRPCMessage::Notification(notification) => {
                    assert_eq!(notification.method, "initialized");
                }
                other => panic!("expected initialized notification, got {other:?}"),
            }
        }

        if let Some(expected_thread_id) = expected_thread_id {
            let request = expect_request(socket, "thread/resume");
            let params = request.params.expect("thread/resume params");
            assert_eq!(params["threadId"], expected_thread_id);
            send_response(
                socket,
                request.id,
                ThreadResumeResponse {
                    thread: build_thread(thread_id),
                    model: "gpt-5-codex".to_string(),
                    model_provider: "openai".to_string(),
                    service_tier: None,
                    cwd: PathBuf::from("/tmp"),
                    approval_policy: AskForApproval::OnRequest,
                    sandbox: SandboxPolicy::DangerFullAccess,
                    reasoning_effort: None,
                },
            );
        } else {
            let request = expect_request(socket, "thread/start");
            send_response(
                socket,
                request.id,
                ThreadStartResponse {
                    thread: build_thread(thread_id),
                    model: "gpt-5-codex".to_string(),
                    model_provider: "openai".to_string(),
                    service_tier: None,
                    cwd: PathBuf::from("/tmp"),
                    approval_policy: AskForApproval::OnRequest,
                    sandbox: SandboxPolicy::DangerFullAccess,
                    reasoning_effort: None,
                },
            );
        }

        let request = expect_request(socket, "turn/start");
        let params = request.params.expect("turn/start params");
        assert_eq!(params["threadId"], thread_id);
        send_response(
            socket,
            request.id,
            TurnStartResponse {
                turn: Turn {
                    id: turn_id.to_string(),
                    items: Vec::new(),
                    status: TurnStatus::InProgress,
                    error: None,
                },
            },
        );

        send_notification(
            socket,
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                item: ThreadItem::AgentMessage {
                    id: format!("message-{turn_id}"),
                    text: reply_text.to_string(),
                    phase: None,
                },
            }),
        );
        send_notification(
            socket,
            ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: thread_id.to_string(),
                turn: Turn {
                    id: turn_id.to_string(),
                    items: Vec::new(),
                    status: TurnStatus::Completed,
                    error: None,
                },
            }),
        );
    }

    fn spawn_mock_telegram_poll_server(
        responses: Vec<MockTelegramPollResponse>,
    ) -> (String, thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock telegram poll server");
        listener
            .set_nonblocking(true)
            .expect("set mock telegram poll listener nonblocking");
        let base_url = format!(
            "http://127.0.0.1:{}",
            listener
                .local_addr()
                .expect("read mock telegram poll local addr")
                .port()
        );

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut handled = 0usize;

            while handled < responses.len() && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(100)))
                            .expect("set mock telegram poll stream read timeout");

                        match &responses[handled] {
                            MockTelegramPollResponse::Hang(duration) => {
                                thread::sleep(*duration);
                            }
                            MockTelegramPollResponse::Response { status_line, body } => {
                                let mut request = [0_u8; 2048];
                                let _ignored = stream.read(&mut request);
                                let response = format!(
                                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                stream
                                    .write_all(response.as_bytes())
                                    .expect("write mock telegram poll response");
                            }
                            MockTelegramPollResponse::TruncatedResponse {
                                status_line,
                                body_prefix,
                                declared_content_length,
                            } => {
                                let mut request = [0_u8; 2048];
                                let _ignored = stream.read(&mut request);
                                let response = format!(
                                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {declared_content_length}\r\nconnection: close\r\n\r\n{body_prefix}",
                                );
                                stream
                                    .write_all(response.as_bytes())
                                    .expect("write truncated mock telegram poll response");
                            }
                        }

                        handled += 1;
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept mock telegram poll connection: {err}"),
                }
            }

            handled
        });

        (base_url, server)
    }

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

    #[test]
    fn restart_command_is_supported() {
        assert!(is_restart_command("/restart"));
        assert!(is_restart_command("/restart@codex_bot"));
        assert!(!is_restart_command("/reset"));
    }

    #[test]
    fn session_id_for_telegram_chat_without_thread_uses_chat_only() {
        assert_eq!(session_id_for_telegram_chat(-100123, None), "-100123");
        assert_eq!(session_id_for_telegram_chat(-100123, Some(0)), "-100123");
    }

    #[test]
    fn session_id_for_telegram_chat_with_thread_includes_suffix() {
        assert_eq!(
            session_id_for_telegram_chat(-100123, Some(42)),
            "-100123:42"
        );
    }

    #[test]
    fn app_server_connection_stale_after_ttl() {
        let now = Instant::now();
        assert!(!app_server_connection_is_stale(
            now - Duration::from_secs(1),
            now,
            Duration::from_secs(5)
        ));
        assert!(app_server_connection_is_stale(
            now - Duration::from_secs(6),
            now,
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn app_server_transport_errors_are_detected() {
        let err = anyhow::anyhow!("send websocket payload");
        assert!(is_app_server_transport_error(&err));

        let err = anyhow::anyhow!("app-server websocket closed the connection");
        assert!(is_app_server_transport_error(&err));

        let err = anyhow::anyhow!("thread not found");
        assert!(!is_app_server_transport_error(&err));
    }

    #[test]
    fn mock_bridge_reuses_app_server_connection_within_ttl() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock app-server");
        listener
            .set_nonblocking(true)
            .expect("set mock app-server listener nonblocking");
        let app_server_url = format!(
            "ws://127.0.0.1:{}",
            listener
                .local_addr()
                .expect("read mock app-server local addr")
                .port()
        );

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut accepted_connections = 0usize;

            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted_connections += 1;
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted app-server stream blocking");
                        let mut socket = accept(stream).expect("accept websocket connection");
                        complete_turn_exchange(
                            &mut socket,
                            true,
                            None,
                            "thread-1",
                            "turn-1",
                            "First response",
                        );
                        complete_turn_exchange(
                            &mut socket,
                            false,
                            Some("thread-1"),
                            "thread-1",
                            "turn-2",
                            "Second response",
                        );
                        break;
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept websocket connection: {err}"),
                }
            }

            accepted_connections
        });

        let mut config = test_config();
        config.mock_messages = vec![
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "First prompt".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "Second prompt".to_string(),
            },
        ];

        let outbound = run_mock_bridge(&config, &app_server_url, 1)
            .expect("run mock bridge end-to-end with reused connection");
        let accepted_connections = server.join().expect("join mock app-server thread");

        assert_eq!(accepted_connections, 1);
        assert_eq!(
            outbound
                .responses
                .into_iter()
                .map(|response| response.text)
                .collect::<Vec<_>>(),
            vec![
                "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                    .to_string(),
                "First response".to_string(),
                "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                    .to_string(),
                "Second response".to_string(),
            ]
        );
    }

    #[test]
    fn mock_bridge_restart_refreshes_app_server_connection_and_keeps_thread_mapping() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock app-server");
        listener
            .set_nonblocking(true)
            .expect("set mock app-server listener nonblocking");
        let app_server_url = format!(
            "ws://127.0.0.1:{}",
            listener
                .local_addr()
                .expect("read mock app-server local addr")
                .port()
        );

        let server = thread::spawn(move || {
            let connections = [
                (None, "thread-1", "turn-1", "First response"),
                (Some("thread-1"), "thread-1", "turn-2", "Second response"),
            ];
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut accepted_connections = 0usize;

            while Instant::now() < deadline && accepted_connections < connections.len() {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (expected_thread_id, thread_id, turn_id, reply_text) =
                            connections[accepted_connections];
                        accepted_connections += 1;
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted app-server stream blocking");
                        let mut socket = accept(stream).expect("accept websocket connection");
                        complete_turn_exchange(
                            &mut socket,
                            true,
                            expected_thread_id,
                            thread_id,
                            turn_id,
                            reply_text,
                        );
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept websocket connection: {err}"),
                }
            }

            accepted_connections
        });

        let mut config = test_config();
        config.mock_messages = vec![
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "First prompt".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "/restart".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "Second prompt".to_string(),
            },
        ];

        let outbound = run_mock_bridge(&config, &app_server_url, 1)
            .expect("run mock bridge end-to-end with restart");
        let accepted_connections = server.join().expect("join mock app-server thread");

        assert_eq!(accepted_connections, 2);
        assert_eq!(
            outbound
                .responses
                .into_iter()
                .map(|response| response.text)
                .collect::<Vec<_>>(),
            vec![
                "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                    .to_string(),
                "First response".to_string(),
                "Bridge restart complete. Session workers and app-server connections were refreshed. Current chat state was kept."
                    .to_string(),
                "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                    .to_string(),
                "Second response".to_string(),
            ]
        );
    }

    #[test]
    fn mock_bridge_reconnects_app_server_between_messages_when_ttl_expires() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock app-server");
        listener
            .set_nonblocking(true)
            .expect("set mock app-server listener nonblocking");
        let app_server_url = format!(
            "ws://127.0.0.1:{}",
            listener
                .local_addr()
                .expect("read mock app-server local addr")
                .port()
        );

        let server = thread::spawn(move || {
            let connections = [
                (None, "thread-1", "turn-1", "First response"),
                (Some("thread-1"), "thread-1", "turn-2", "Second response"),
            ];
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut handled = 0usize;

            while handled < connections.len() && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted app-server stream blocking");
                        let mut socket = accept(stream).expect("accept websocket connection");
                        let (expected_thread_id, thread_id, turn_id, reply_text) =
                            connections[handled];
                        complete_turn_exchange(
                            &mut socket,
                            true,
                            expected_thread_id,
                            thread_id,
                            turn_id,
                            reply_text,
                        );
                        handled += 1;
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept websocket connection: {err}"),
                }
            }

            handled
        });

        let mut config = test_config();
        config.mock_messages = vec![
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "First prompt".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "Second prompt".to_string(),
            },
        ];

        let outbound =
            run_mock_bridge_with_connection_ttl(&config, &app_server_url, 1, Duration::ZERO)
                .expect("run mock bridge end-to-end with forced reconnect");
        let handled = server.join().expect("join mock app-server thread");

        assert_eq!(handled, 2);
        assert_eq!(
            outbound
                .responses
                .into_iter()
                .map(|response| response.text)
                .collect::<Vec<_>>(),
            vec![
                "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                    .to_string(),
                "First response".to_string(),
                "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                    .to_string(),
                "Second response".to_string(),
            ]
        );
    }

    #[test]
    fn mock_bridge_keeps_distinct_thread_mappings_per_session() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock app-server");
        listener
            .set_nonblocking(true)
            .expect("set mock app-server listener nonblocking");
        let app_server_url = format!(
            "ws://127.0.0.1:{}",
            listener
                .local_addr()
                .expect("read mock app-server local addr")
                .port()
        );

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut accepted_connections = 0usize;

            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted_connections += 1;
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted app-server stream blocking");
                        let mut socket = accept(stream).expect("accept websocket connection");
                        complete_turn_exchange(
                            &mut socket,
                            true,
                            None,
                            "thread-1",
                            "turn-1",
                            "Session one first response",
                        );
                        complete_turn_exchange(
                            &mut socket,
                            false,
                            None,
                            "thread-2",
                            "turn-2",
                            "Session two first response",
                        );
                        complete_turn_exchange(
                            &mut socket,
                            false,
                            Some("thread-1"),
                            "thread-1",
                            "turn-3",
                            "Session one second response",
                        );
                        complete_turn_exchange(
                            &mut socket,
                            false,
                            Some("thread-2"),
                            "thread-2",
                            "turn-4",
                            "Session two second response",
                        );
                        break;
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept websocket connection: {err}"),
                }
            }

            accepted_connections
        });

        let mut config = test_config();
        config.mock_messages = vec![
            BridgeInboundMessage {
                session_id: "-100111".to_string(),
                text: "Session one prompt".to_string(),
            },
            BridgeInboundMessage {
                session_id: "-100222".to_string(),
                text: "Session two prompt".to_string(),
            },
            BridgeInboundMessage {
                session_id: "-100111".to_string(),
                text: "Session one follow-up".to_string(),
            },
            BridgeInboundMessage {
                session_id: "-100222".to_string(),
                text: "Session two follow-up".to_string(),
            },
        ];

        let outbound = run_mock_bridge(&config, &app_server_url, 1)
            .expect("run mock bridge end-to-end with distinct sessions");
        let accepted_connections = server.join().expect("join mock app-server thread");

        assert_eq!(accepted_connections, 1);
        assert_eq!(
            outbound
                .responses
                .into_iter()
                .map(|response| (response.session_id, response.text))
                .collect::<Vec<_>>(),
            vec![
                (
                    "-100111".to_string(),
                    "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                        .to_string(),
                ),
                (
                    "-100111".to_string(),
                    "Session one first response".to_string(),
                ),
                (
                    "-100222".to_string(),
                    "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                        .to_string(),
                ),
                (
                    "-100222".to_string(),
                    "Session two first response".to_string(),
                ),
                (
                    "-100111".to_string(),
                    "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                        .to_string(),
                ),
                (
                    "-100111".to_string(),
                    "Session one second response".to_string(),
                ),
                (
                    "-100222".to_string(),
                    "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                        .to_string(),
                ),
                (
                    "-100222".to_string(),
                    "Session two second response".to_string(),
                ),
            ]
        );
    }

    #[test]
    fn mock_bridge_forks_current_thread_and_rebinds_session() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock app-server");
        listener
            .set_nonblocking(true)
            .expect("set mock app-server listener nonblocking");
        let app_server_url = format!(
            "ws://127.0.0.1:{}",
            listener
                .local_addr()
                .expect("read mock app-server local addr")
                .port()
        );

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut accepted_connections = 0usize;

            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted_connections += 1;
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted app-server stream blocking");
                        let mut socket = accept(stream).expect("accept websocket connection");

                        complete_turn_exchange(
                            &mut socket,
                            true,
                            None,
                            "thread-1",
                            "turn-1",
                            "First response",
                        );

                        let request = expect_request(&mut socket, "thread/fork");
                        let params = request.params.expect("thread/fork params");
                        assert_eq!(params["threadId"], "thread-1");
                        send_response(
                            &mut socket,
                            request.id,
                            ThreadForkResponse {
                                thread: build_thread("thread-2"),
                                model: "gpt-5-codex".to_string(),
                                model_provider: "openai".to_string(),
                                service_tier: None,
                                cwd: PathBuf::from("/tmp"),
                                approval_policy: AskForApproval::OnRequest,
                                sandbox: SandboxPolicy::DangerFullAccess,
                                reasoning_effort: None,
                            },
                        );

                        complete_turn_exchange(
                            &mut socket,
                            false,
                            Some("thread-2"),
                            "thread-2",
                            "turn-2",
                            "Forked response",
                        );
                        break;
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept websocket connection: {err}"),
                }
            }

            accepted_connections
        });

        let mut config = test_config();
        config.mock_messages = vec![
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "First prompt".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "/fork".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "Follow-up on fork".to_string(),
            },
        ];

        let outbound = run_mock_bridge(&config, &app_server_url, 1)
            .expect("run mock bridge end-to-end with fork");
        let accepted_connections = server.join().expect("join mock app-server thread");

        assert_eq!(accepted_connections, 1);
        assert_eq!(
            outbound
                .responses
                .into_iter()
                .map(|response| response.text)
                .collect::<Vec<_>>(),
            vec![
                "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                    .to_string(),
                "First response".to_string(),
                "Conversation forked.\nOld thread: thread-1\nNew thread: thread-2\nThis chat now continues on the new fork."
                    .to_string(),
                "Processing your message (cwd: (server default), model: (server default), effort: (server default))..."
                    .to_string(),
                "Forked response".to_string(),
            ]
        );
    }

    #[test]
    fn mock_bridge_lists_recent_threads_in_current_cwd_and_marks_current_thread() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock app-server");
        listener
            .set_nonblocking(true)
            .expect("set mock app-server listener nonblocking");
        let app_server_url = format!(
            "ws://127.0.0.1:{}",
            listener
                .local_addr()
                .expect("read mock app-server local addr")
                .port()
        );

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut accepted_connections = 0usize;

            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted_connections += 1;
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted app-server stream blocking");
                        let mut socket = accept(stream).expect("accept websocket connection");

                        complete_turn_exchange(
                            &mut socket,
                            true,
                            None,
                            "thread-1",
                            "turn-1",
                            "First response",
                        );

                        let request = expect_request(&mut socket, "thread/list");
                        let params = request.params.expect("thread/list params");
                        assert_eq!(params["limit"], 2);
                        assert_eq!(params["sortKey"], "updated_at");
                        assert_eq!(params["archived"], false);
                        assert_eq!(params["cwd"], "/workspace/demo");
                        send_response(
                            &mut socket,
                            request.id,
                            ThreadListResponse {
                                data: vec![
                                    build_thread_with_metadata(
                                        "thread-1",
                                        "Telegram current thread preview",
                                        Some("Current thread title"),
                                        20,
                                    ),
                                    build_thread_with_metadata(
                                        "thread-9",
                                        "Another saved thread preview",
                                        None,
                                        10,
                                    ),
                                ],
                                next_cursor: Some("ignored-cursor".to_string()),
                            },
                        );
                        break;
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept websocket connection: {err}"),
                }
            }

            accepted_connections
        });

        let mut config = test_config();
        config.runtime.thread.cwd = Some("/workspace/demo".to_string());
        config.mock_messages = vec![
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "First prompt".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "/list 2".to_string(),
            },
        ];

        let outbound = run_mock_bridge(&config, &app_server_url, 1)
            .expect("run mock bridge end-to-end with list");
        let accepted_connections = server.join().expect("join mock app-server thread");

        assert_eq!(accepted_connections, 1);
        assert_eq!(
            outbound
                .responses
                .into_iter()
                .map(|response| response.text)
                .collect::<Vec<_>>(),
            vec![
                "Processing your message (cwd: /workspace/demo, model: (server default), effort: (server default))..."
                    .to_string(),
                "First response".to_string(),
                "Current chat thread: thread-1\nCWD scope: /workspace/demo (config default)\nRecent threads in this CWD (latest 2):\n- [current] thread-1 | Current thread title\n- thread-9 | Another saved thread preview"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn mock_bridge_resumes_previous_thread_and_scopes_list_to_resumed_cwd() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock app-server");
        listener
            .set_nonblocking(true)
            .expect("set mock app-server listener nonblocking");
        let app_server_url = format!(
            "ws://127.0.0.1:{}",
            listener
                .local_addr()
                .expect("read mock app-server local addr")
                .port()
        );

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut accepted_connections = 0usize;

            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted_connections += 1;
                        stream
                            .set_nonblocking(false)
                            .expect("set accepted app-server stream blocking");
                        let mut socket = accept(stream).expect("accept websocket connection");

                        let initialize = expect_request(&mut socket, "initialize");
                        send_response(
                            &mut socket,
                            initialize.id,
                            InitializeResponse {
                                user_agent: "test-agent".to_string(),
                            },
                        );

                        match read_jsonrpc_message(&mut socket) {
                            JSONRPCMessage::Notification(notification) => {
                                assert_eq!(notification.method, "initialized");
                            }
                            other => panic!("expected initialized notification, got {other:?}"),
                        }

                        let request = expect_request(&mut socket, "thread/resume");
                        let params = request.params.expect("thread/resume params");
                        assert_eq!(params["threadId"], "thread-9");
                        let mut response = serde_json::to_value(ThreadResumeResponse {
                            thread: build_thread_with_metadata_and_cwd(
                                "thread-9",
                                "Resumed preview",
                                Some("Prior session"),
                                30,
                                "/workspace/prior",
                            ),
                            model: "gpt-5-codex".to_string(),
                            model_provider: "openai".to_string(),
                            service_tier: None,
                            cwd: PathBuf::from("/workspace/prior"),
                            approval_policy: AskForApproval::OnRequest,
                            sandbox: SandboxPolicy::DangerFullAccess,
                            reasoning_effort: None,
                        })
                        .expect("serialize thread/resume response");
                        response["reasoningEffort"] =
                            serde_json::Value::String("medium".to_string());
                        send_response(&mut socket, request.id, response);

                        let request = expect_request(&mut socket, "thread/list");
                        let params = request.params.expect("thread/list params");
                        assert_eq!(params["limit"], 2);
                        assert_eq!(params["sortKey"], "updated_at");
                        assert_eq!(params["archived"], false);
                        assert_eq!(params["cwd"], "/workspace/prior");
                        send_response(
                            &mut socket,
                            request.id,
                            ThreadListResponse {
                                data: vec![
                                    build_thread_with_metadata_and_cwd(
                                        "thread-9",
                                        "Resumed preview",
                                        Some("Prior session"),
                                        30,
                                        "/workspace/prior",
                                    ),
                                    build_thread_with_metadata_and_cwd(
                                        "thread-8",
                                        "Neighbor session",
                                        None,
                                        20,
                                        "/workspace/prior",
                                    ),
                                ],
                                next_cursor: None,
                            },
                        );

                        complete_turn_exchange(
                            &mut socket,
                            false,
                            Some("thread-9"),
                            "thread-9",
                            "turn-2",
                            "Resumed response",
                        );
                        break;
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept websocket connection: {err}"),
                }
            }

            accepted_connections
        });

        let mut config = test_config();
        config.mock_messages = vec![
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "/resume thread-9".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "/list 2".to_string(),
            },
            BridgeInboundMessage {
                session_id: "chat-1".to_string(),
                text: "Follow-up after resume".to_string(),
            },
        ];

        let outbound = run_mock_bridge(&config, &app_server_url, 1)
            .expect("run mock bridge end-to-end with resume");
        let accepted_connections = server.join().expect("join mock app-server thread");

        assert_eq!(accepted_connections, 1);
        assert_eq!(
            outbound
                .responses
                .into_iter()
                .map(|response| response.text)
                .collect::<Vec<_>>(),
            vec![
                "Conversation resumed.\nThread: thread-9\nCWD: /workspace/prior\nModel: gpt-5-codex\nReasoning effort: medium\nThis chat now continues on the selected thread."
                    .to_string(),
                "Current chat thread: thread-9\nCWD scope: /workspace/prior (chat override)\nRecent threads in this CWD (latest 2):\n- [current] thread-9 | Prior session\n- thread-8 | Neighbor session"
                    .to_string(),
                "Processing your message (cwd: /workspace/prior, model: gpt-5-codex, effort: medium)..."
                    .to_string(),
                "Resumed response".to_string(),
            ]
        );
    }

    #[test]
    fn telegram_poll_timeout_errors_are_retryable() {
        let (base_url, server) =
            spawn_mock_telegram_poll_server(vec![MockTelegramPollResponse::Hang(
                Duration::from_millis(80),
            )]);
        let telegram = TelegramApiClient::new_with_request_timeout(
            "test-token".to_string(),
            base_url,
            1,
            Duration::from_millis(20),
        )
        .expect("create telegram client with short timeout");

        let err = telegram
            .get_updates(None)
            .expect_err("first request should time out");
        let handled = server.join().expect("join mock telegram poll server");

        assert_eq!(handled, 1);
        assert!(is_retryable_telegram_poll_error(&err));
    }

    #[test]
    fn telegram_poll_http_status_retries_only_on_retryable_statuses() {
        let (retryable_url, retryable_server) =
            spawn_mock_telegram_poll_server(vec![MockTelegramPollResponse::Response {
                status_line: "502 Bad Gateway",
                body: String::new(),
            }]);
        let retryable_telegram = TelegramApiClient::new("test-token".to_string(), retryable_url, 1)
            .expect("create telegram client for retryable status");

        let retryable_err = retryable_telegram
            .get_updates(None)
            .expect_err("502 should fail");
        let retryable_handled = retryable_server
            .join()
            .expect("join retryable mock telegram poll server");

        let (non_retryable_url, non_retryable_server) =
            spawn_mock_telegram_poll_server(vec![MockTelegramPollResponse::Response {
                status_line: "401 Unauthorized",
                body: String::new(),
            }]);
        let non_retryable_telegram =
            TelegramApiClient::new("test-token".to_string(), non_retryable_url, 1)
                .expect("create telegram client for non-retryable status");

        let non_retryable_err = non_retryable_telegram
            .get_updates(None)
            .expect_err("401 should fail");
        let non_retryable_handled = non_retryable_server
            .join()
            .expect("join non-retryable mock telegram poll server");

        assert_eq!(retryable_handled, 1);
        assert!(is_retryable_telegram_poll_error(&retryable_err));
        assert_eq!(non_retryable_handled, 1);
        assert!(!is_retryable_telegram_poll_error(&non_retryable_err));
    }

    #[test]
    fn telegram_poll_truncated_response_errors_are_retryable() {
        let (base_url, server) =
            spawn_mock_telegram_poll_server(vec![MockTelegramPollResponse::TruncatedResponse {
                status_line: "200 OK",
                body_prefix: r#"{"ok":true,"result":"#.to_string(),
                declared_content_length: 256,
            }]);
        let telegram = TelegramApiClient::new("test-token".to_string(), base_url, 1)
            .expect("create telegram client for truncated response");

        let err = telegram
            .get_updates(None)
            .expect_err("truncated response should fail");
        let handled = server
            .join()
            .expect("join truncated mock telegram poll server");

        assert_eq!(handled, 1);
        assert!(is_retryable_telegram_poll_error(&err));
    }

    #[test]
    fn telegram_poll_send_request_disconnect_messages_are_retryable() {
        let err = anyhow::anyhow!(
            "error sending request for url (https://api.telegram.org/botTOKEN/getUpdates)"
        )
        .context("client error (SendRequest)")
        .context("connection closed before message completed");

        assert!(is_retryable_telegram_poll_error(&err));
    }

    #[test]
    fn telegram_poll_retries_with_recreated_client_after_transient_failure() {
        let (base_url, server) = spawn_mock_telegram_poll_server(vec![
            MockTelegramPollResponse::Response {
                status_line: "502 Bad Gateway",
                body: String::new(),
            },
            MockTelegramPollResponse::Response {
                status_line: "200 OK",
                body: r#"{"ok":true,"result":[{"update_id":7,"message":{"chat":{"id":1},"text":"hello"}}]}"#
                    .to_string(),
            },
        ]);
        let mut telegram = TelegramApiClient::new("test-token".to_string(), base_url, 1)
            .expect("create telegram client");
        let mut consecutive_failures = 0_u32;

        let updates = get_updates_with_retry(&mut telegram, None, &mut consecutive_failures)
            .expect("retryable error should recover");
        let handled = server.join().expect("join mock telegram poll server");

        assert_eq!(handled, 2);
        assert_eq!(consecutive_failures, 0);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 7);
    }

    #[test]
    fn telegram_poll_retries_after_truncated_response_disconnect() {
        let (base_url, server) = spawn_mock_telegram_poll_server(vec![
            MockTelegramPollResponse::TruncatedResponse {
                status_line: "200 OK",
                body_prefix: r#"{"ok":true,"result":"#.to_string(),
                declared_content_length: 256,
            },
            MockTelegramPollResponse::Response {
                status_line: "200 OK",
                body: r#"{"ok":true,"result":[{"update_id":9,"message":{"chat":{"id":1},"text":"hello again"}}]}"#
                    .to_string(),
            },
        ]);
        let mut telegram = TelegramApiClient::new("test-token".to_string(), base_url, 1)
            .expect("create telegram client");
        let mut consecutive_failures = 0_u32;

        let updates = get_updates_with_retry(&mut telegram, None, &mut consecutive_failures)
            .expect("truncated response should recover after reconnect");
        let handled = server
            .join()
            .expect("join truncated+recovery mock telegram poll server");

        assert_eq!(handled, 2);
        assert_eq!(consecutive_failures, 0);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 9);
    }
}
