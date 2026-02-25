# Plan: Connector-Side Streaming via Edit-in-Place

## Context

The Gateway already tails `response.jsonl` incrementally and writes streaming `OutboundMessage` entries to the outbox with `meta.streaming: true`. But the Connector has no concept of message editing — it calls `send_message()` for every outbox file, spamming users with ~30 separate messages during a 60-second agent run.

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

The key challenge: the **Connector** sends a message and gets a platform message ID back, but the **Gateway** is the one writing subsequent streaming messages. These are separate processes communicating only via files on the PVC.

**Solution**: Use a per-session "stream state" file on the PVC that the Connector writes after the first send (with the platform message ID), and reads on subsequent streaming messages for the same session.

```
Gateway writes: /data/outbox/{channel}/,{ts}.{id}.nq  (OutboundMessage with meta.streaming=true, meta.session_id="abc")
Connector reads nq → sees streaming=true, session_id="abc"
  → First time: send_message() → capture msg_id → write /data/outbox/{channel}/.stream-abc (msg_id)
  → Subsequent: read .stream-abc → edit_message(msg_id, new_text)
  → Final (streaming=false): read .stream-abc → edit_message(msg_id, final_text) → delete .stream-abc
```

## The ChatProvider Trait

The core abstraction — matches the two-tier pattern from `chat-streaming.md` Section 6. Any provider implementing `send()` + `edit()` gets fallback streaming for free. The outbound loop in `outbound.rs` handles all streaming coordination via these three methods.

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

## Phase 1: Edit-in-Place Fallback (Implemented)

> Status: **Done** — committed as `ae0a6dc`

### 1. Gateway: Include `session_id` in streaming meta

**File:** `crates/kk-gateway/src/loops/results.rs` — `try_stream_partial()`

Flatten `original_meta` into top-level meta and add `session_id`:
```rust
let mut meta = manifest.meta.clone();
if let Some(obj) = meta.as_object_mut() {
    obj.insert("streaming".into(), serde_json::Value::Bool(true));
    obj.insert("session_id".into(), serde_json::Value::String(session_id.to_string()));
}
```

This way `meta.chat_id`, `meta.channel_id`, etc. remain at the top level where providers expect them.

### 2. Connector: Streaming dispatch in `outbound.rs`

**File:** `crates/kk-connector/src/outbound.rs`

```rust
pub async fn poll_outbound(
    outbox_dir: &str,
    channel_name: &str,
    sender: &dyn ChatProvider,  // trait object, not enum
) -> Result<()> { ... }
```

The dispatch logic:

- `send_streaming()`: If `.stream-{sid}` exists → `sender.edit()`. Otherwise → `sender.send_returning_id()` + write state file.
- `send_final()`: If `.stream-{sid}` exists → `sender.edit()` + delete state file. Otherwise → `sender.send()`.
- `cleanup_stale_stream_states()`: Remove `.stream-*` files older than 10 minutes.

### 3. Telegram: `impl ChatProvider for TelegramOutbound`

**File:** `crates/kk-connector/src/provider/telegram.rs`

- `send()` — splits at 4096 chars, sends chunks with reply parameters
- `send_returning_id()` — sends single message, returns `sent.id.0.to_string()`
- `edit()` — `bot.edit_message_text()`, ignores "message is not modified" error

### 4. Slack: `impl ChatProvider for SlackOutbound`

**File:** `crates/kk-connector/src/provider/slack.rs`

- `send()` — splits at 4000 chars, POSTs `chat.postMessage`
- `send_returning_id()` — POSTs `chat.postMessage`, returns `resp["ts"]`
- `edit()` — POSTs `chat.update` with `channel`, `ts`, `text`

### 5. Main: `Box<dyn ChatProvider>`

**File:** `crates/kk-connector/src/main.rs`

```rust
let (inbound_handle, sender): (JoinHandle<()>, Box<dyn ChatProvider>) =
    match config.channel_type.as_str() {
        "telegram" => {
            let telegram = TelegramProvider::new(token).await?;
            let sender = Box::new(telegram.sender());
            // ...
            (handle, sender)
        }
        "slack" => {
            let slack = SlackProvider::new(bot_token, app_token).await?;
            let sender = Box::new(slack.sender());
            // ...
            (handle, sender)
        }
    };
```

---

## Phase 2: Slack Native Streaming (Planned, Not Implemented)

Slack launched purpose-built LLM streaming APIs in October 2025. These bypass the edit-in-place pattern entirely — the client sees a real-time streaming animation, and `appendStream` has Tier 4 rate limits (100+/min), much higher than `chat.update` (Tier 3: 50+/min).

**Reference:** This is exactly how Vercel's Chat SDK (`vercel/chat`) handles Slack streaming — it's the only platform with native support. All other platforms fall back to post+edit. See `chat-streaming.md` for full analysis.

**API flow:**

```
1. POST chat.startStream  → returns { channel, ts }  (creates the streaming message)
2. POST chat.appendStream → appends markdown chunks  (can call many times, Tier 4)
3. POST chat.stopStream   → finalizes the message     (sets final content, optional Block Kit)
```

**Required Slack scopes:** `chat:write`, `assistant:write` (the streaming APIs are part of the "AI in Slack" feature set).

**Required fields:** `recipient_user_id` and `recipient_team_id` must be passed on `startStream`. These identify who triggered the AI response. kk's inbound Slack messages already capture `user_id` in meta — we also need to capture `team_id`.

### Approach: Extend `ChatProvider` Trait

Add optional native streaming methods with default implementations that return errors. Only `SlackOutbound` overrides them:

```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    // ... existing required methods ...

    /// Whether this provider supports native streaming (e.g. Slack).
    /// Default: false — use edit-in-place fallback.
    fn supports_native_stream(&self) -> bool { false }

    /// Start a native streaming session. Returns platform message ID.
    async fn stream_start(&self, _msg: &OutboundMessage) -> Result<String> {
        bail!("native streaming not supported")
    }

    /// Append text to an active native stream.
    async fn stream_append(&self, _msg: &OutboundMessage, _platform_msg_id: &str) -> Result<()> {
        bail!("native streaming not supported")
    }

    /// Finalize an active native stream.
    async fn stream_stop(&self, _msg: &OutboundMessage, _platform_msg_id: &str) -> Result<()> {
        bail!("native streaming not supported")
    }
}
```

### SlackOutbound Implementation

```rust
#[async_trait]
impl ChatProvider for SlackOutbound {
    // ... existing send/send_returning_id/edit ...

    fn supports_native_stream(&self) -> bool { true }

    async fn stream_start(&self, msg: &OutboundMessage) -> Result<String> {
        let channel_id = extract_channel_id(msg)?;
        let user_id = msg.meta.get("user_id").and_then(|v| v.as_str())
            .context("missing user_id for Slack streaming")?;
        let team_id = msg.meta.get("team_id").and_then(|v| v.as_str())
            .context("missing team_id for Slack streaming")?;

        let mut body = serde_json::json!({
            "channel": channel_id,
            "recipient_user_id": user_id,
            "recipient_team_id": team_id,
        });
        if let Some(ref thread_ts) = msg.thread_id {
            body["thread_ts"] = serde_json::Value::String(thread_ts.clone());
        }

        let resp = self.client
            .post("https://slack.com/api/chat.startStream")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send().await?.json::<serde_json::Value>().await?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            bail!("chat.startStream failed: {}", resp["error"].as_str().unwrap_or("unknown"));
        }
        resp["ts"].as_str().map(|s| s.to_string())
            .context("missing ts in chat.startStream response")
    }

    async fn stream_append(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let channel_id = extract_channel_id(msg)?;
        let body = serde_json::json!({
            "channel": channel_id,
            "ts": platform_msg_id,
            "markdown_text": &msg.text,
        });

        let resp = self.client
            .post("https://slack.com/api/chat.appendStream")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send().await?.json::<serde_json::Value>().await?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            let error = resp["error"].as_str().unwrap_or("unknown");
            if error != "stream_not_found" {
                bail!("chat.appendStream failed: {error}");
            }
        }
        Ok(())
    }

    async fn stream_stop(&self, msg: &OutboundMessage, platform_msg_id: &str) -> Result<()> {
        let channel_id = extract_channel_id(msg)?;
        let body = serde_json::json!({
            "channel": channel_id,
            "ts": platform_msg_id,
        });

        let resp = self.client
            .post("https://slack.com/api/chat.stopStream")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send().await?.json::<serde_json::Value>().await?;

        if !resp["ok"].as_bool().unwrap_or(false) {
            let error = resp["error"].as_str().unwrap_or("unknown");
            if error != "stream_not_found" {
                bail!("chat.stopStream failed: {error}");
            }
        }
        Ok(())
    }
}
```

### Outbound Dispatch Changes

Modify `send_streaming` and `send_final` in `outbound.rs` to prefer native streaming:

```rust
async fn send_streaming(
    outbox_dir: &Path,
    sender: &dyn ChatProvider,
    msg: &OutboundMessage,
    session_id: Option<&str>,
) -> Result<()> {
    let Some(sid) = session_id else {
        return sender.send(msg).await;
    };

    let state_path = stream_state_path(outbox_dir, sid);

    if state_path.exists() {
        let platform_msg_id = std::fs::read_to_string(&state_path)?;
        let platform_msg_id = platform_msg_id.trim();

        if sender.supports_native_stream() {
            sender.stream_append(msg, platform_msg_id).await
        } else {
            sender.edit(msg, platform_msg_id).await
        }
    } else {
        let msg_id = if sender.supports_native_stream() {
            sender.stream_start(msg).await?
        } else {
            sender.send_returning_id(msg).await?
        };
        std::fs::write(&state_path, &msg_id)?;
        Ok(())
    }
}

async fn send_final(
    outbox_dir: &Path,
    sender: &dyn ChatProvider,
    msg: &OutboundMessage,
    session_id: Option<&str>,
) -> Result<()> {
    if let Some(sid) = session_id {
        let state_path = stream_state_path(outbox_dir, sid);
        if state_path.exists() {
            let platform_msg_id = std::fs::read_to_string(&state_path)?;
            let platform_msg_id = platform_msg_id.trim();

            let result = if sender.supports_native_stream() {
                let _ = sender.stream_append(msg, platform_msg_id).await;
                sender.stream_stop(msg, platform_msg_id).await
            } else {
                sender.edit(msg, platform_msg_id).await
            };
            let _ = std::fs::remove_file(&state_path);
            return result;
        }
    }

    sender.send(msg).await
}
```

### Slack Inbound: Capture `team_id`

The Slack inbound handler needs to also capture `team_id` from the event payload so it's available for native streaming:

```rust
// In handle_event(), add to meta:
meta: serde_json::json!({
    "channel_id": channel_id,
    "message_ts": ts,
    "user_id": user,
    "team_id": team_id,  // NEW: needed for chat.startStream
}),
```

### When to Use Native vs Fallback

The decision is automatic via `supports_native_stream()`:
- **Slack** → native `startStream`/`appendStream`/`stopStream`
- **Telegram** → fallback `send_returning_id` + `edit`
- **Future providers** → override `supports_native_stream() → true` and the stream methods, or get fallback for free

---

## Files Modified

### Phase 1 (Done)

| File | Change |
|---|---|
| `Cargo.toml` (workspace) | Added `async-trait = "0.1"` |
| `crates/kk-connector/Cargo.toml` | Added `async-trait` dependency |
| `crates/kk-gateway/src/loops/results.rs` | Flatten `original_meta` into top-level meta, add `session_id` field |
| `crates/kk-connector/src/provider/mod.rs` | `ChatProvider` trait replacing `ProviderSender` enum |
| `crates/kk-connector/src/provider/telegram.rs` | `TelegramOutbound` struct + `impl ChatProvider` |
| `crates/kk-connector/src/provider/slack.rs` | `impl ChatProvider for SlackOutbound` |
| `crates/kk-connector/src/outbound.rs` | Streaming dispatch via `&dyn ChatProvider`, stream state files, stale cleanup |
| `crates/kk-connector/src/main.rs` | `Box<dyn ChatProvider>` construction |
| `crates/kk-connector/tests/` | Updated to use concrete `TelegramOutbound`/`SlackOutbound` types |

### Phase 2 (Planned)

| File | Change |
|---|---|
| `crates/kk-connector/src/provider/mod.rs` | Add `supports_native_stream`, `stream_start/append/stop` with default impls to `ChatProvider` |
| `crates/kk-connector/src/provider/slack.rs` | Override native streaming methods, capture `team_id` in inbound meta |
| `crates/kk-connector/src/outbound.rs` | Prefer native streaming when `supports_native_stream()` returns true |

## Verification

1. `cargo build` — all crates compile
2. `cargo test` — all 131 tests pass
3. `cargo clippy` — no warnings
4. Manual test flow:
   - Write a streaming OutboundMessage (with `meta.streaming=true, meta.session_id="test1"`) to outbox
   - Verify first message creates `.stream-test1` file with platform message ID
   - Write another streaming message with same session_id
   - Verify it edits the existing message (not a new one)
   - Write final message (no streaming flag) with same session_id
   - Verify final edit and `.stream-test1` file deleted
