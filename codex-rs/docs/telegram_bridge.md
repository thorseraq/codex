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

Example `config.toml`:

```toml
token = "<telegram-bot-token>"
chat_id = 123456789
# allowed_chat_ids = [123456789, 987654321]
# allowed_user_ids = [111111111, 222222222]
poll_timeout_seconds = 30
auto_approve_commands = false
auto_approve_file_changes = false

[codex]
cwd = "/absolute/project/path"
# model = "gpt-5-codex"
# reasoning_effort = "medium"
# approval_policy = "never"
# sandbox = "workspace-write"
```

Environment overrides:

- `CODEX_TELEGRAM_BOT_TOKEN`
- `CODEX_TELEGRAM_CHAT_ID` (single ID, comma-separated accepted)
- `CODEX_TELEGRAM_CHAT_IDS` (comma-separated)
- `CODEX_TELEGRAM_ALLOWED_USER_ID` (single ID, comma-separated accepted)
- `CODEX_TELEGRAM_ALLOWED_USER_IDS` (comma-separated)
- `CODEX_TELEGRAM_BASE_URL`
- `CODEX_TELEGRAM_AUTO_APPROVE_COMMANDS` (`true/false`)
- `CODEX_TELEGRAM_AUTO_APPROVE_FILE_CHANGES` (`true/false`)

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
- `/status` shows bridge liveness, current mapped thread id, and effective CWD/model/reasoning effort for the chat (including resolved server defaults). If the chat has no mapped thread yet, `/status` creates one so a thread id is always returned.
- Messages are accepted when either the chat ID is allowlisted or the sender user ID is allowlisted.
- Messages from unauthorized sources (chat and user both not allowlisted) are ignored.
- Different chat groups are processed in parallel worker sessions, while preserving in-order processing per chat group.
- If a stored chat-to-thread mapping points to a missing thread, the bridge creates a new thread mapping automatically.
- For normal prompts, the bridge sends an immediate processing-state message (cwd/model/effort) before the final response.
- Long responses are chunked to satisfy Telegram message limits.
- Legacy app-server approval callbacks are rejected; v2 approvals are auto-accepted/declined based on config.

## Limitations

- Bridge currently handles text messages only.
- Telegram bridge uses long polling (`getUpdates`), not webhooks.
- Websocket app-server transport is still marked experimental in app-server docs.
