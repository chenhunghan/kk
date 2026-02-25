# kk: Connector

> **Cross-references:**
> [Communication Protocol](kk-protocol.md) — message formats §4.1, §4.2; nq conventions §2; polling intervals §7
> [Controller](kk-controller.md) — creates Connector Deployments
> [Gateway](kubeclaw-plan-gateway.md) — reads inbound queue, writes outbox queue
> [Agent Job](kubeclaw-plan-agent-job.md) ·
> [Skill](kubeclaw-plan-skill.md)

---

## Summary

A **Provider** is an external messaging service (Telegram, Slack, WhatsApp, Discord, Signal). Each provider has a corresponding implementation in `provider/` (e.g. `TelegramProvider`) that handles API-specific inbound and outbound logic. The term "provider" is scoped to the Connector — other kk components don't interact with providers directly.

A Connector is a single-replica Deployment created by the Controller (one per Channel CR). It bridges a provider to the internal nq file queues on the shared PVC. A single Rust binary handles all provider types — behavior is selected by the `CHANNEL_TYPE` env var.

The Connector runs concurrent tasks:
1. **Provider dispatcher** — receives events from the provider API, sends `ConnectorEvent` via mpsc channel (messages + new chat events)
2. **Inbound processor** — receives `ConnectorEvent`, resolves groups (auto-registers unknown chats), normalizes to `InboundMessage`, writes to `/data/inbound/`
3. **Outbound poller** — polls `/data/outbox/{channel-name}/`, sends responses back via provider API

The Connector has **no knowledge** of the Gateway, Agent Jobs, Skills, or any other kk internals. It only knows about its provider and the two queue directories.

---

## Image

```
kk-connector:latest

Base:    rust:1-slim (build) → debian:bookworm-slim (runtime)
Runtime: Single static binary (kk-connector)
Size:    ~30MB
```

---

## Environment Variables

Set by Controller:

| Var | Source | Example |
|---|---|---|
| `CHANNEL_TYPE` | Channel CR `spec.type` | `telegram` |
| `CHANNEL_NAME` | Channel CR `metadata.name` | `telegram-bot-1` |
| `INBOUND_DIR` | Hardcoded by Controller | `/data/inbound` |
| `OUTBOX_DIR` | Built from CHANNEL_NAME | `/data/outbox/telegram-bot-1` |
| `GROUPS_D_FILE` | Built from CHANNEL_NAME | `/data/state/groups.d/telegram-bot-1.json` |

Provider credentials come from the Secret via `envFrom`:

| Provider | Required Env Vars |
|---|---|
| Telegram | `TELEGRAM_BOT_TOKEN` |
| Slack | `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN` |

Tuning vars (optional):

| Var | Default | Description |
|---|---|---|
| `OUTBOUND_POLL_INTERVAL_MS` | `1000` | Outbound queue poll interval |

---

## Crate Structure

```
crates/kk-connector/
├── Cargo.toml
└── src/
    ├── main.rs              ← entry point, 3 tokio tasks via tokio::select!
    ├── lib.rs               ← re-exports modules
    ├── config.rs            ← ConnectorConfig from env vars
    ├── inbound.rs           ← inbound processor (ConnectorEvent → nq files)
    ├── outbound.rs          ← outbound poller (nq files → provider API)
    ├── groups.rs            ← GroupMap: chat_id → group slug, auto-registration
    └── provider/
        ├── mod.rs           ← InboundRaw, ConnectorEvent, ProviderSender enum
        ├── telegram.rs      ← TelegramProvider (teloxide)
        └── slack.rs         ← SlackProvider (Socket Mode + Web API)
```

Shared code in `kk-core`:

```
crates/kk-core/src/
├── text.rs                  ← split_text() for message chunking
├── nq.rs                    ← file-based queue operations
├── types.rs                 ← InboundMessage, OutboundMessage, GroupsConfig
└── logging.rs               ← structured JSON logging init
```

---

## Startup Sequence

```
1. Read env vars via ConnectorConfig::from_env()
2. Validate CHANNEL_TYPE is a supported provider (currently: "telegram", "slack")
3. Validate provider-specific credentials present
4. Ensure directories: mkdir -p INBOUND_DIR, OUTBOX_DIR, groups.d parent
5. Initialize provider (e.g. TelegramProvider validates token via get_me())
6. Load initial GroupMap from groups.d/{channel}.json
7. Create mpsc channel for ConnectorEvent messages (buffer=256)
8. Spawn 3 concurrent tasks:
   a. Provider inbound dispatcher
   b. Inbound processor (mpsc → nq files)
   c. Outbound poller (nq files → provider API)
9. tokio::select! — exit if any task fails
```

---

## Group Mapping

The Connector translates provider-specific chat IDs into kk group slugs. The mapping is stored in `/data/state/groups.d/{channel}.json`.

### GroupMap

```rust
// provider/mod.rs
pub struct InboundRaw {
    pub chat_id: String,
    pub sender_name: String,
    pub text: String,
    pub timestamp: u64,
    pub thread_id: Option<String>,   // thread ID (None if not threaded)
    pub meta: serde_json::Value,
}

pub enum ConnectorEvent {
    Message(InboundRaw),
    NewChat { chat_id: String, chat_title: Option<String> },
}
```

```rust
// groups.rs
pub struct GroupMap {
    map: HashMap<String, String>,  // chat_id → group slug
}

impl GroupMap {
    pub fn load(groups_d_file: &str, channel_name: &str) -> Self;
    pub fn resolve(&self, chat_id: &str) -> Option<String>;
    pub fn register(&mut self, chat_id: &str, channel_name: &str) -> String;
    pub fn persist(&self, groups_d_file: &str, channel_name: &str) -> Result<()>;
}
```

- **load**: Loads `groups.d/{channel}.json`, builds reverse map for this channel
- **resolve**: Returns the mapped group slug, or `None` for unmapped chat IDs
- **register**: Auto-registers unmapped chat_id → `tg-{abs(chat_id)}` slug, updates in-memory map
- **persist**: Atomically writes auto-registered mappings to `groups.d/{channel}.json`

Owned by the inbound processor task (no shared locks needed).

---

## Auto-Registration

When the bot encounters an unknown chat (no mapping in `groups.d/`), the Connector auto-registers it instead of dropping the message.

**Triggers:**
1. **Bot added to group** — provider notifies the Connector of a new group membership (e.g. Telegram `my_chat_member` update). Sends `ConnectorEvent::NewChat`.
2. **Message in unmapped group** — A message arrives from a chat_id with no mapping. The inbound processor auto-registers before processing.

**Slug format:** `tg-{abs(chat_id)}` — strips leading `-` from negative chat IDs. E.g. `-1001234567890` → `tg-1001234567890`.

**Persistence:** Auto-registered mappings are written to `/data/state/groups.d/{channel-name}.json` (atomic write via tmp+rename). Each Connector instance writes only its own file, avoiding concurrent writer conflicts. Default trigger mode for auto-registered groups is `mention`.

---

## Inbound Flow (Provider → PVC)

```
Provider API (long-poll / webhooks / event stream)
  │
  │  Provider dispatcher (provider-specific)
  ▼
mpsc::Sender<ConnectorEvent>
  │
  │  Inbound processor task
  │  1. Match ConnectorEvent:
  │     - NewChat → auto-register + persist
  │     - Message → resolve group (auto-register if unknown)
  │  2. Build InboundMessage (Protocol §4.1)
  │  3. nq::enqueue() to /data/inbound/
  ▼
/data/inbound/,{ts}.{id}.nq
```

### Provider Dispatcher

Each provider implementation extracts a common set of fields from its API into `InboundRaw`:

| Field | Description |
|---|---|
| `chat_id` | Provider-specific chat/group identifier (as string) |
| `sender_name` | Human-readable sender name |
| `text` | Message text (or media caption) |
| `timestamp` | Unix seconds |
| `thread_id` | Thread/topic ID if threaded, `None` otherwise |
| `meta` | Provider-specific metadata (e.g. `chat_id`, `message_id`) |

Provider-specific filters (e.g. skip bot messages, skip empty text) are applied before sending `ConnectorEvent::Message`.

### Inbound Processor

Receives `ConnectorEvent` via mpsc, resolves group (auto-registers if unknown), builds `InboundMessage`, writes atomically via `nq::enqueue()`.

---

## Outbound Flow (PVC → Provider)

```
/data/outbox/{channel-name}/,{ts}.{id}.nq
  │
  │  Outbound poller (every 1s)
  │  1. nq::list_pending() — sorted FIFO
  │  2. Read OutboundMessage (Protocol §4.2)
  │  3. Validate channel matches CHANNEL_NAME
  │  4. Provider::send() — split long messages per provider limit
  │  5. nq::delete() on success
  ▼
Provider API → User sees response
```

### Outbound Thread Routing

When `OutboundMessage.thread_id` is present, the provider routes the response to the correct thread. Non-threaded messages (`thread_id: None`) are sent normally.

### Text Splitting

Shared `kk_core::text::split_text(text, max_len)`:
- Prefer split at last newline before limit
- Fall back to last space
- Hard cut if no good break point
- Max length is provider-specific (e.g. Telegram: 4096 characters)

---

## Error Handling

### Inbound Errors

| Error | Handling |
|---|---|
| Provider connection lost | Provider handles reconnect (e.g. teloxide auto-reconnects). Log warning. |
| PVC write fails (disk full) | Log error. Message lost. Connector stays running. |
| groups.d file missing/corrupt | Start with empty mappings. Auto-registration builds map over time. |
| Non-text message | Silently skip (text is empty). |

### Outbound Errors

| Error | Handling |
|---|---|
| Provider API error (transient) | Leave nq file → retry next poll (1s). |
| Provider API error (permanent) | Log error. File remains. Warn if stuck >5min. |
| Malformed outbound message | Log error, delete file. |
| Channel mismatch | Log error, delete file. |

---

## Graceful Shutdown

tokio runtime handles SIGTERM → tasks drop → provider dispatcher stops → process exits.

K8s sends SIGTERM, waits `terminationGracePeriodSeconds` (default 30s), then SIGKILL.

---

## Dependencies

| Crate | Purpose |
|---|---|
| `teloxide` 0.13 | Telegram provider (long-poll dispatcher) |
| `reqwest` 0.12 | Slack Web API HTTP client |
| `tokio-tungstenite` 0.26 | Slack Socket Mode WebSocket client |
| `futures-util` 0.3 | Stream/Sink extensions for WebSocket |
| `tokio` | Async runtime, mpsc channels, timers |
| `serde` / `serde_json` | Message serialization |
| `tracing` | Structured JSON logging |
| `kk-core` | Shared types, nq queue ops, text splitting |

---

## Providers

### ProviderSender (`provider/mod.rs`)

Enum dispatching outbound sends to the correct provider:

```rust
pub enum ProviderSender {
    Telegram(teloxide::Bot),
    Slack(SlackSender),
}
```

### Telegram (`provider/telegram.rs`)

Uses `teloxide` with a long-poll dispatcher. Two handler branches:

1. **Message handler** — filters bot messages, extracts `InboundRaw` fields, sends `ConnectorEvent::Message`
2. **Chat member handler** — detects bot added to group (`my_chat_member` status transition: not-present → present), sends `ConnectorEvent::NewChat`

Outbound: parses `meta.chat_id` as `ChatId`, optionally sets `message_thread_id` for Forum Topics, splits at 4096 chars, sets `reply_parameters` on first chunk.

Auto-registration slug: `tg-{abs(chat_id)}` (e.g. `-1001234567890` → `tg-1001234567890`).

### Slack (`provider/slack.rs`)

Uses **Socket Mode** (WebSocket) for inbound — no HTTP server needed, connection is connector-initiated. Uses Slack **Web API** (`chat.postMessage`) for outbound.

**Credentials:**
- `SLACK_BOT_TOKEN` (`xoxb-...`) — for Web API calls
- `SLACK_APP_TOKEN` (`xapp-...`) — for Socket Mode WebSocket connection

**Inbound (Socket Mode):**
1. Calls `apps.connections.open` with app token → gets `wss://` URL
2. Connects via WebSocket, receives envelope payloads
3. Acknowledges each envelope with `{"envelope_id": "..."}`
4. Handles `events_api` envelopes:
   - **`message` event** — filters bot messages and subtypes (edits, deletes), extracts `InboundRaw`, sends `ConnectorEvent::Message`
   - **`member_joined_channel` event** — detects bot added to channel, sends `ConnectorEvent::NewChat`
5. Auto-reconnects on disconnect or error (5s backoff)

**Outbound:**
- Parses `meta.channel_id` to target the Slack channel
- Splits at 4000 chars (Slack practical limit)
- Sets `thread_ts` for threaded replies when `thread_id` is present
- Supports `meta.reply_to_ts` to start a thread from a specific message

**User name resolution:**
- Calls `users.info` to resolve user IDs to display names
- Results cached in memory (RwLock HashMap) to avoid repeated API calls

**Metadata fields:**
```json
{
    "channel_id": "C0123456789",
    "message_ts": "1234567890.123456",
    "user_id": "U0123456789"
}
```

Auto-registration slug: `slack-{channel_id}` (e.g. `C0123456789` → `slack-c0123456789`).

---

## Testing Checklist

### Inbound
- [x] Message → `InboundMessage` written to `/data/inbound/`
- [ ] Bot messages filtered out
- [ ] Non-text messages filtered out
- [x] Group mapping resolves chat ID → group slug
- [x] Unmapped chat ID → auto-registered with `{prefix}-{id}` slug
- [x] Atomic write: no partial files in inbound dir
- [x] groups.d file missing → connector starts with empty mappings

### Outbound
- [ ] Outbound nq file → message sent via provider API
- [ ] File deleted after successful send
- [x] File retained on transient error → retried next poll
- [x] Channel mismatch → file discarded with error log
- [x] Malformed JSON → file discarded with error log
- [ ] Long message split per provider limit
- [ ] Reply threading works

### Threads
- [x] Threaded message → `thread_id` extracted and round-trips to InboundMessage on disk
- [ ] Non-threaded message → `thread_id` is `None`, omitted from JSON
- [ ] Outbound with `thread_id` → routed to correct thread
- [ ] Outbound without `thread_id` → sent normally

### Auto-Registration
- [x] Unmapped chat_id from message → auto-register → groups.d file written → message enqueued with `{prefix}-{id}` slug
- [x] Bot added to new group → groups.d file written
- [x] Already-mapped chat_id → existing slug preserved, no overwrite
- [x] Second message reuses slug from in-memory map
- [x] groups.d file persists across restarts (load picks it up)

### Slack-Specific
- [ ] Socket Mode connects and receives `hello`
- [ ] Socket Mode auto-reconnects on disconnect
- [ ] Bot messages (`bot_id` present) filtered out
- [ ] Message subtypes (edits, deletes) filtered out
- [ ] `member_joined_channel` for bot → `ConnectorEvent::NewChat`
- [ ] User name resolved via `users.info` and cached
- [ ] Outbound `thread_ts` set for threaded replies
- [ ] Auto-registration slug uses `slack-{channel_id}` (lowercased)

### Unit Tests (in code)
- [x] `text::split_text` — empty, under limit, newline split, space split, hard cut, max limit
- [x] `groups::GroupMap` — load from file, resolve known/unknown, missing file fallback, auto_slug, register, persist
- [x] `groups::auto_group_slug` — Telegram strips negative, Slack lowercases
- [x] `slack::parse_slack_ts` — normal, no decimal, empty, invalid
