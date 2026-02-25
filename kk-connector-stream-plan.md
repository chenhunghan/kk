# Plan: Connector-Side Streaming via Edit-in-Place

## Context

The Gateway tails `response.jsonl` incrementally while an Agent Job is running. Streaming updates and final messages are delivered to the Connector, which sends/edits platform messages.

**Platform capabilities** (from research):
| Platform | Edit API | 2s interval safe? | Best approach |
|---|---|---|---|
| Telegram | `editMessageText` | Yes (~1/sec/chat) | Edit in place |
| Slack | `chat.startStream`/`appendStream`/`stopStream` (native, Oct 2025) | Yes (Tier 4: 100+/min on append) | Native streaming API |
| Discord | `PATCH .../messages/{id}` | Yes (5/5sec) | Edit in place |
| WhatsApp | **No edit API** | N/A | Skip intermediates, send final only |
| Signal | Unofficial only | N/A | Skip intermediates, send final only |

**Approach**: Two-tier streaming following the Vercel Chat SDK pattern (see `chat-streaming.md`):
1. **Native streaming** for platforms that support it (Slack `chat.startStream`/`appendStream`/`stopStream`)
2. **Fallback edit-in-place** for everything else — any provider implementing `send()` + `edit()` gets streaming for free

## Architecture

Streaming and non-streaming messages use **separate paths** on the PVC:

```
STREAMING: /data/stream/{channel}/{session-id}
  Gateway overwrites this file with latest text (atomic tmp+rename)
  meta.final=false for intermediates, meta.final=true when agent finishes
  Connector polls this directory, sends/edits platform messages
  Connector deletes file after processing final

NON-STREAMING: /data/outbox/{channel}/,{ts}.{id}.nq
  Gateway writes final messages here (error without prior streaming, etc.)
  Connector polls, calls sender.send(), deletes nq file
```

**Who deletes what**:
- Gateway writes/overwrites stream files
- Connector deletes stream file after processing `meta.final: true`
- Connector manages `.stream-{sid}` platform-msg-id state files (in stream dir)
- Connector deletes outbox nq files after successful send

**Fallback**: If the agent finishes before any streaming happened (no stream file created), Gateway writes to the outbox as a regular message.

## The ChatProvider Trait

The core abstraction — matches the two-tier pattern from `chat-streaming.md` Section 6. Any provider implementing `send()` + `edit()` gets fallback streaming for free. The `poll_stream()` function handles all streaming coordination via these three methods.

```rust
// crates/kk-connector/src/provider/mod.rs

#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Send a new message. May split long messages across multiple platform messages.
    async fn send(&self, msg: &OutboundMessage) -> Result<()>;

    /// Send a single message and return the platform-specific message ID.
    /// Used for streaming: the first partial response creates the message.
    async fn send_returning_id(&self, msg: &OutboundMessage) -> Result<String>;

    /// Edit a previously sent message by its platform-specific ID.
    /// Used for streaming: updates the same message with new partial content.
    async fn edit(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()>;
}
```

Each platform has a `*Provider` (lifecycle/bootstrap + inbound) and `*Outbound` (implements `ChatProvider`):

```
TelegramProvider::new(token)
    ├── .sender()        → TelegramOutbound (implements ChatProvider)
    └── .run_inbound()   → consumes self, runs teloxide dispatcher

SlackProvider::new(bot_token, app_token)
    ├── .sender()        → SlackOutbound (implements ChatProvider)
    └── .run_inbound()   → consumes self, runs Socket Mode
```

The outbound loop uses `&dyn ChatProvider` for open extension — adding a new provider requires only `impl ChatProvider for MyOutbound`, no match arms to update.

---

## Phase 1: ChatProvider Trait Refactor (Implemented)

> Status: **Done** — committed as `ae0a6dc`

Refactored `ProviderSender` enum to `ChatProvider` trait. Each provider has a `*Outbound` struct implementing the trait. `Box<dyn ChatProvider>` in `main.rs`, `&dyn ChatProvider` in outbound functions.

### Files Modified

| File | Change |
|---|---|
| `Cargo.toml` (workspace) | Added `async-trait = "0.1"` |
| `crates/kk-connector/Cargo.toml` | Added `async-trait` dependency |
| `crates/kk-connector/src/provider/mod.rs` | `ChatProvider` trait replacing `ProviderSender` enum |
| `crates/kk-connector/src/provider/telegram.rs` | `TelegramOutbound` struct + `impl ChatProvider` |
| `crates/kk-connector/src/provider/slack.rs` | `impl ChatProvider for SlackOutbound` |
| `crates/kk-connector/src/main.rs` | `Box<dyn ChatProvider>` construction |
| `crates/kk-connector/tests/` | Updated to use concrete `TelegramOutbound`/`SlackOutbound` types |

---

## Phase 2: Separate Stream Path (Implemented)

> Status: **Done** — committed as `b6b7044`

Moved streaming from the outbox to a dedicated `/data/stream/{channel}/{session-id}` path. The outbox is now purely for non-streaming final messages.

### Gateway Changes

**`try_stream_partial()`** — writes to stream file (atomic tmp+rename) instead of `nq::enqueue`:
```rust
let stream_path = state.paths.stream_file(&manifest.channel, session_id);
write_stream_file(&stream_path, &outbound)?;  // tmp + rename
```

**`process_done/stopped/overflow/error()`** — use `write_final_or_outbox()`:
- If stream file exists → overwrite with `meta.final: true` (Connector does final edit + cleanup)
- If no stream file → write to outbox as regular message (Connector does `sender.send()`)

### Connector Changes

**`poll_stream()`** — new function polling `/data/stream/{channel}/`:
```rust
pub async fn poll_stream(stream_dir: &str, sender: &dyn ChatProvider) -> Result<()>
```

Dispatch logic per stream file (named by session_id):
- No `.stream-{sid}` state file → `sender.send_returning_id()` → write state file
- `.stream-{sid}` exists + `meta.final: false` → `sender.edit()` (update in place)
- `.stream-{sid}` exists + `meta.final: true` → `sender.edit()` → delete both files
- No `.stream-{sid}` + `meta.final: true` → `sender.send()` → delete stream file

**`poll_outbound()`** — simplified to pure send loop (no streaming logic):
```rust
// Just: parse message → sender.send() → delete nq file
```

Both functions called from the same polling loop in `main.rs`.

### Tests

6 streaming tests with `MockChatProvider`:
- `stream_first_message_sends_new` — first file triggers `send_returning_id`, state file created
- `stream_subsequent_edits_existing` — subsequent file triggers `edit` with saved platform msg ID
- `stream_final_edits_and_cleans_up` — final file triggers `edit`, both files deleted
- `stream_final_without_prior_sends_new` — final without prior stream triggers `send`, file deleted
- `stream_full_lifecycle` — end-to-end: send → edit → final edit + cleanup across 3 polls
- `stream_malformed_file_removed` — bad JSON removed, no provider calls

### Files Modified

| File | Change |
|---|---|
| `crates/kk-core/src/paths.rs` | Added `stream_dir()`, `stream_file()`, updated `ensure_dirs()` |
| `crates/kk-gateway/src/loops/results.rs` | `try_stream_partial` writes stream file; terminal handlers use `write_final_or_outbox()` |
| `crates/kk-connector/src/config.rs` | Added `stream_dir` field (`STREAM_DIR` env, defaults `/data/stream/{channel}`) |
| `crates/kk-connector/src/main.rs` | Creates stream dir, calls `poll_stream()` alongside `poll_outbound()` |
| `crates/kk-connector/src/outbound.rs` | Added `poll_stream()` with MockChatProvider tests; simplified `poll_outbound()` |
| `crates/kk-connector/tests/common/mod.rs` | Added `stream_dir` to test config |

---

## Phase 3: Slack Native Streaming (Implemented)

> Status: **Done** — committed as `2b06c77`

Slack launched purpose-built LLM streaming APIs in October 2025. These bypass the edit-in-place pattern entirely — the client sees a real-time streaming animation, and `appendStream` has Tier 4 rate limits (100+/min), much higher than `chat.update` (Tier 3: 50+/min).

**Reference:** This is exactly how Vercel's Chat SDK (`vercel/chat`) handles Slack streaming — it's the only platform with native support. All other platforms fall back to post+edit. See `chat-streaming.md` for full analysis.

**API flow:**

```
1. POST chat.startStream  → returns { channel, ts }  (creates the streaming message)
2. POST chat.appendStream → appends markdown chunks  (can call many times, Tier 4)
3. POST chat.stopStream   → finalizes the message     (sets final content, optional Block Kit)
```

**Required Slack scopes:** `chat:write`, `assistant:write` (the streaming APIs are part of the "AI in Slack" feature set).

**Required fields:** `recipient_user_id` and `recipient_team_id` must be passed on `startStream`. These identify who triggered the AI response.

### ChatProvider Trait Extension

Added optional native streaming methods with default implementations that return errors. Only `SlackOutbound` overrides them:

```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    // ... existing required methods (send, send_returning_id, edit) ...

    fn supports_native_stream(&self) -> bool { false }

    async fn stream_start(&self, _msg: &OutboundMessage) -> Result<String> {
        bail!("native streaming not supported")
    }

    async fn stream_append(&self, _msg: &OutboundMessage, _platform_msg_id: &str) -> Result<()> {
        bail!("native streaming not supported")
    }

    async fn stream_stop(&self, _msg: &OutboundMessage, _platform_msg_id: &str) -> Result<()> {
        bail!("native streaming not supported")
    }
}
```

### Outbound Dispatch Changes

`poll_stream()` checks `sender.supports_native_stream()` and dispatches accordingly:

- First message: `stream_start()` instead of `send_returning_id()`
- Subsequent: `stream_append()` instead of `edit()`
- Final: `stream_stop()` instead of `edit()`
- Final without prior streaming: `send()` (same for both paths)

### Slack Inbound: `team_id` Captured

The Slack inbound handler now captures `team_id` from the event payload:

```rust
meta: serde_json::json!({
    "channel_id": channel_id,
    "message_ts": ts,
    "user_id": user,
    "team_id": event["team"].as_str().unwrap_or_default(),
}),
```

### Tests

5 native streaming tests with `NativeStreamMock`:
- `native_stream_start` — first file triggers `stream_start`, state file created
- `native_stream_append` — subsequent file triggers `stream_append` with saved msg ID
- `native_stream_stop_and_cleanup` — final file triggers `stream_stop`, both files deleted
- `native_stream_final_without_prior_sends_new` — final without prior triggers `send`, file deleted
- `native_stream_full_lifecycle` — end-to-end: start → append → stop across 3 polls

### Files Modified

| File | Change |
|---|---|
| `crates/kk-connector/src/provider/mod.rs` | Added `supports_native_stream`, `stream_start/append/stop` with default impls to `ChatProvider` |
| `crates/kk-connector/src/provider/slack.rs` | Override native streaming methods using `chat.startStream/appendStream/stopStream`; capture `team_id` in inbound meta |
| `crates/kk-connector/src/outbound.rs` | `poll_stream()` prefers native streaming when `supports_native_stream()` returns true; 5 new native streaming tests |

---

## Verification

1. `cargo build` — all crates compile
2. `cargo test --workspace -- --list` — 153 tests discovered across workspace (including streaming tests)
3. `cargo clippy` — no warnings
