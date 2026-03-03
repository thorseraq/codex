# Telegram Bridge for `codex app-server`

This document describes how to run Codex through a Telegram bot while reusing the app-server JSON-RPC protocol.

## Overview

When enabled, the Telegram bridge:

1. Connects to `codex app-server` over websocket.
2. Performs normal JSON-RPC `initialize` + `initialized` handshake.
3. Polls Telegram updates and forwards text messages as `turn/start` input.
4. Streams app-server notifications and sends the assistant response back to Telegram.
5. Persists chat-to-thread mapping and Telegram update offsets in a local state file.

## Start Command

```bash
codex app-server \
  --listen ws://127.0.0.1:4222 \
  --telegram-bridge
```

`--telegram-bridge` requires websocket mode (`--listen ws://...`).

## Configuration

Default config file path:

- `${CODEX_HOME:-~/.codex}/telegram/config.toml`

Default state file path:

- `${CODEX_HOME:-~/.codex}/telegram/state.json`

You can override either path:

```bash
codex app-server \
  --listen ws://127.0.0.1:4222 \
  --telegram-bridge \
  --telegram-config /abs/path/telegram-config.toml \
  --telegram-state /abs/path/telegram-state.json
```

Complete `config.toml` template:

```toml
# Telegram bot token.
# Required unless CODEX_TELEGRAM_BOT_TOKEN is set in the environment.
token = "123456789:AAExampleBotToken"

# Single allowlisted chat ID for inbound bridge messages.
# Note: group/supergroup IDs are often negative (for example: -100...).
chat_id = -1001234567890

# Additional allowlisted chat IDs for inbound bridge messages.
allowed_chat_ids = [-1001234567890, -1009876543210]

# Optional allowlisted user IDs for inbound bridge messages.
# A message is accepted when either chat ID OR user ID is allowlisted.
allowed_user_ids = [111111111, 222222222]

# Optional single destination for outbound reply relay.
reply_chat_id = -1001234567890

# Optional multiple destinations for outbound reply relay.
reply_chat_ids = [-1001234567890, -1005555555555]

# Telegram getUpdates long-poll timeout (seconds, minimum effective value is 1).
poll_timeout_seconds = 30

# Optional Telegram API base URL.
# Use this only for custom/proxy Telegram endpoints.
base_url = "https://api.telegram.org"

# Auto-approve command approvals for bridge-initiated turns.
auto_approve_commands = false

# Auto-approve file-change approvals for bridge-initiated turns.
auto_approve_file_changes = false

# Stream assistant deltas via Telegram Bot API `sendMessageDraft` while a turn is running.
# When false, bridge only sends one final response message.
stream_responses = false

[codex]
# Default working directory used for new turns from Telegram bridge.
cwd = "/path/to/workspace"

# Optional default model for bridge-created turns.
model = "gpt-5-codex"

# Optional default reasoning effort.
# Accepted values: none, minimal, low, medium, high, xhigh.
reasoning_effort = "medium"

# Optional default approval policy.
# Accepted values: untrusted, on-failure, on-request, never.
approval_policy = "on-request"

# Optional default sandbox mode.
# Accepted values: read-only, workspace-write, danger-full-access.
sandbox = "workspace-write"

# Optional mock inputs used only with --telegram-bridge-mock.
[[mock_messages]]
chat_id = -1001234567890
text = "Summarize the latest git diff."

[[mock_messages]]
# chat_id omitted -> bridge uses mock fallback chat id.
text = "What should I test next?"
```

Field usage reference:

- `token`: Telegram bot token used by both bridge polling and (by fallback) reply relay.
- `chat_id`: Convenience single inbound allowlist chat id; merged with `allowed_chat_ids`.
- `allowed_chat_ids`: Inbound bridge allowlist by chat id.
- `allowed_user_ids`: Inbound bridge allowlist by sender user id.
- `reply_chat_id`: Convenience single outbound relay destination.
- `reply_chat_ids`: Outbound relay destinations for `CODEX_TELEGRAM_REPLY_RELAY=true`.
- `poll_timeout_seconds`: Long-poll timeout for `getUpdates`.
- `base_url`: Telegram API endpoint override for bridge and relay fallback.
- `auto_approve_commands`: Bridge runtime policy for command approvals.
- `auto_approve_file_changes`: Bridge runtime policy for file change approvals.
- `stream_responses`: Bridge runtime policy for `sendMessageDraft` streaming previews (with automatic fallback to final-only response when drafts are unavailable).
- `codex.cwd`: Default cwd override for bridge-created turns.
- `codex.model`: Default model override for bridge-created turns.
- `codex.reasoning_effort`: Default reasoning effort override for bridge-created turns.
- `codex.approval_policy`: Default approval policy override for bridge-created turns.
- `codex.sandbox`: Default sandbox override for bridge-created turns.
- `mock_messages`: Optional mock message queue consumed only in `--telegram-bridge-mock` mode.
- `mock_messages.chat_id`: Optional chat id for that mock message.
- `mock_messages.text`: Required message text for that mock message.

Environment overrides:

- `CODEX_TELEGRAM_BOT_TOKEN`
- `CODEX_TELEGRAM_CHAT_ID` (single ID, comma-separated accepted)
- `CODEX_TELEGRAM_CHAT_IDS` (comma-separated)
- `CODEX_TELEGRAM_ALLOWED_USER_ID` (single ID, comma-separated accepted)
- `CODEX_TELEGRAM_ALLOWED_USER_IDS` (comma-separated)
- `CODEX_TELEGRAM_BASE_URL`
- `CODEX_TELEGRAM_AUTO_APPROVE_COMMANDS` (`true/false`)
- `CODEX_TELEGRAM_AUTO_APPROVE_FILE_CHANGES` (`true/false`)
- `CODEX_TELEGRAM_STREAM_RESPONSES` (`true/false`)

## Reply Relay for Every Interface

You can relay completed assistant replies to Telegram from any Codex interface (`codex`, `codex exec`, and `codex app-server`) by enabling:

- `CODEX_TELEGRAM_REPLY_RELAY=true`

Relay-specific overrides:

- `CODEX_TELEGRAM_REPLY_BOT_TOKEN` (optional; falls back to `CODEX_TELEGRAM_BOT_TOKEN`)
- `CODEX_TELEGRAM_REPLY_CHAT_IDS` (optional; falls back to `CODEX_TELEGRAM_CHAT_IDS` / `CODEX_TELEGRAM_CHAT_ID` / config `reply_chat_ids` + `reply_chat_id` / config `chat_id` + `allowed_chat_ids`)
- `CODEX_TELEGRAM_REPLY_BASE_URL` (optional; falls back to `CODEX_TELEGRAM_BASE_URL`)

Relay destinations can also be configured in `${CODEX_HOME:-~/.codex}/telegram/config.toml`:

- `reply_chat_ids = [ ... ]`
- `reply_chat_id = ...`

Notes:

- Relay uses the same default config path: `${CODEX_HOME:-~/.codex}/telegram/config.toml`.
- Relay forwards final turn messages (`TurnComplete.last_agent_message`), not intermediate deltas.
- If you run `codex app-server --telegram-bridge` with relay enabled, turns created by Telegram bridge clients are automatically excluded from relay to avoid duplicate replies.

## Create a Telegram Bot

1. In Telegram, open `@BotFather`.
2. Run `/newbot` and finish bot setup.
3. Copy the bot token.
4. Send a message to your bot from the target chat.
5. Get your chat ID:
   - Temporarily run `getUpdates` via browser or curl:

```bash
curl "https://api.telegram.org/bot<token>/getUpdates"
```

6. Find `message.chat.id` in the JSON response and set it as `chat_id` (or in `CODEX_TELEGRAM_CHAT_ID`).

## Mock Mode (No Telegram Network Required)

Use mock mode to test the bridge loop and app-server protocol wiring without a real bot:

```bash
RUST_LOG=info codex app-server \
  --listen ws://127.0.0.1:4222 \
  --telegram-bridge \
  --telegram-bridge-mock \
  --telegram-bridge-mock-message "Summarize this repository"
```

Notes:

- You can repeat `--telegram-bridge-mock-message` multiple times.
- In mock mode, the command exits after all mock messages are processed.

You can also define mock inputs in config:

```toml
[[mock_messages]]
chat_id = 1
text = "Explain the latest git diff"

[[mock_messages]]
chat_id = 1
text = "What should I test next?"
```

## Runtime Behavior

- `/reset` from Telegram clears the mapped Codex thread for that chat.
- `/cwd <absolute-path>` sets a chat-scoped CWD override and resets that chat thread.
- `/cwd` shows the current effective CWD for the chat.
- `/cwd reset` clears the chat-scoped CWD override.
- `/model <model-id>` sets a chat-scoped model override.
- `/model` shows the current effective model for the chat and lists selectable model IDs.
- `/model reset` clears the chat-scoped model override.
- `/effort <none|minimal|low|medium|high|xhigh>` sets a chat-scoped reasoning effort override.
- `/effort` shows the current effective reasoning effort for the chat.
- `/effort reset` clears the chat-scoped reasoning effort override.
- `/interrupt` (aliases: `/stop`, `/cancel`) interrupts the currently running turn for the chat.
- `/status` shows bridge liveness, current mapped thread id, and effective CWD/model/reasoning effort for the chat (including resolved server defaults). If the chat has no mapped thread yet, `/status` creates one so a thread id is always returned.
- Messages are accepted when either the chat ID is allowlisted or the sender user ID is allowlisted.
- Messages from unauthorized sources (chat and user both not allowlisted) are ignored.
- Different chat groups are processed in parallel worker sessions, while preserving in-order processing per chat group.
- If a stored chat-to-thread mapping points to a missing thread, the bridge creates a new thread mapping automatically.
- For normal prompts, the bridge sends an immediate processing-state message (cwd/model/effort) before the final response.
- When `stream_responses=true`, the bridge sends incremental preview updates using Telegram `sendMessageDraft` (stable `draft_id` per turn) and always sends one final persisted message at turn completion.
- Long responses are chunked to satisfy Telegram message limits.
- Legacy app-server approval callbacks are rejected; v2 approvals are auto-accepted/declined based on config.

## Limitations

- Bridge currently handles text messages only.
- Telegram bridge uses long polling (`getUpdates`), not webhooks.
- Websocket app-server transport is still marked experimental in app-server docs.
