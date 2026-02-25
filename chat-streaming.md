# Vercel Chat SDK: Streaming Analysis

> How `vercel/chat` delivers LLM streaming responses across chat platforms,
> and what kk can learn for a Rust implementation.

**Repository:** https://github.com/vercel/chat
**Package:** `chat` on npm (v4.14.0)
**Language:** TypeScript (pnpm monorepo with Turborepo)

---

## 1. What Is It?

A **server-side multi-platform chat bot SDK** — not a UI library, not related to `useChat`/frontend hooks. It normalizes the different APIs of chat platforms behind a single `Adapter` interface.

Supported platforms (via adapter packages):

| Adapter | Platform | Edit API |
|---|---|---|
| `@chat-adapter/slack` | Slack | `chat.startStream`/`appendStream`/`stopStream` (native) + `chat.update` (fallback) |
| `@chat-adapter/teams` | Microsoft Teams | `updateActivity` |
| `@chat-adapter/gchat` | Google Chat | `spaces.messages.update` |
| `@chat-adapter/discord` | Discord | `PATCH /channels/{id}/messages/{id}` |
| `@chat-adapter/github` | GitHub Issues/PRs | Octokit comment PATCH |
| `@chat-adapter/linear` | Linear | Comment update |

**No Telegram, WhatsApp, or Signal adapters.** Focused on workplace/developer platforms.

---

## 2. The Two-Tier Streaming Strategy

The SDK accepts `AsyncIterable<string>` (compatible with Vercel AI SDK's `textStream` output) and delivers it to the platform using one of two strategies:

### Tier 1: Native Streaming (Slack only)

If the adapter implements the optional `stream()` method, use the platform's native streaming API.

**Slack uses `chat.startStream`/`appendStream`/`stopStream`** — purpose-built APIs for LLM streaming announced October 2025:

```typescript
// packages/adapter-slack/src/index.ts
async stream(threadId, textStream, options) {
  const streamer = this.client.chatStream({
    channel, thread_ts: threadTs,
    recipient_user_id: options.recipientUserId,
    recipient_team_id: options.recipientTeamId,
  });

  for await (const chunk of textStream) {
    await streamer.append({ markdown_text: chunk });
  }

  const result = await streamer.stop(
    options?.stopBlocks ? { blocks: options.stopBlocks } : undefined
  );
  return { id: result.message.ts, threadId, raw: result };
}
```

Rate limits: `appendStream` is Tier 4 (100+/min) — designed for high-frequency appends.

### Tier 2: Fallback Post+Edit (all other platforms)

Post a placeholder `"..."` message, then repeatedly edit it on a timer with accumulated text:

```typescript
// packages/chat/src/thread.ts
private async fallbackStream(textStream, options) {
  const intervalMs = options?.updateIntervalMs ?? this._streamingUpdateIntervalMs;
  const msg = await this.adapter.postMessage(this.id, "...");

  let accumulated = "";
  let lastEditContent = "...";
  let stopped = false;

  const doEditAndReschedule = async () => {
    if (stopped) return;
    if (accumulated !== lastEditContent) {
      await this.adapter.editMessage(threadId, msg.id, accumulated);
      lastEditContent = accumulated;
    }
    if (!stopped) {
      timerId = setTimeout(() => { pendingEdit = doEditAndReschedule(); }, intervalMs);
    }
  };

  timerId = setTimeout(() => { pendingEdit = doEditAndReschedule(); }, intervalMs);

  for await (const chunk of textStream) {
    accumulated += chunk;
  }

  stopped = true;
  clearTimeout(timerId);

  // Final edit to ensure all content is shown
  if (accumulated !== lastEditContent) {
    await this.adapter.editMessage(threadId, msg.id, accumulated);
  }

  return createSentMessage(msg.id, accumulated, threadId);
}
```

**Key design choices:**
- Uses `setTimeout` recursion, not `setInterval` — prevents edit pile-up on slow APIs
- Default interval: **500ms** (configurable via `StreamOptions.updateIntervalMs`)
- Edit errors are **silently ignored** (only the final edit is guaranteed)
- Placeholder is `"..."` — always replaced by first real content
- **One message per stream** — never sends multiple messages

---

## 3. The Adapter Interface

Every adapter MUST implement `postMessage()` and `editMessage()`, which gives fallback streaming for free. Adapters can optionally implement `stream()` for native streaming.

```typescript
interface Adapter {
  // Required — gives fallback streaming for free
  postMessage(threadId: string, message: PostableMessage): Promise<RawMessage>;
  editMessage(threadId: string, messageId: string, message: PostableMessage): Promise<RawMessage>;

  // Optional — platform-native streaming
  stream?(threadId: string, textStream: AsyncIterable<string>, options?: StreamOptions): Promise<RawMessage>;
}

interface StreamOptions {
  recipientUserId?: string;    // Slack: user ID
  recipientTeamId?: string;    // Slack: workspace ID
  stopBlocks?: unknown[];      // Slack: Block Kit elements for final message
  updateIntervalMs?: number;   // Fallback: edit interval (default 500ms)
}

// Input type: either static content or a stream
type PostableMessage = AdapterPostableMessage | AsyncIterable<string>;
```

The decision logic in `Thread.handleStream()`:

```
if adapter.stream exists → use native streaming
else → post("...") + edit on timer (fallback)
```

---

## 4. Streaming by Platform

| Platform | Method | Native? | Interval | How |
|---|---|---|---|---|
| **Slack** | `startStream`/`appendStream`/`stopStream` | Yes | Per chunk | Real-time append, finalize with Block Kit |
| **Teams** | Post + `updateActivity` | No | 500ms | Edit placeholder repeatedly |
| **Google Chat** | Post + `spaces.messages.update` | No | 500ms | Edit placeholder repeatedly |
| **Discord** | Post + `PATCH .../messages/{id}` | No | 500ms | Edit placeholder repeatedly |
| **GitHub** | Post + comment PATCH | No | 500ms | Edit placeholder repeatedly |
| **Linear** | Post + comment update | No | 500ms | Edit placeholder repeatedly |
| **Telegram** | _(no adapter)_ | — | — | Would use `editMessageText` (same fallback pattern) |
| **WhatsApp** | _(no adapter)_ | — | — | No edit API — would need skip-intermediates approach |

---

## 5. Comparison with kk's Current Plan

| Aspect | Vercel Chat SDK | kk (current plan) |
|---|---|---|
| **Architecture** | In-process: adapter calls platform API directly | File-based IPC: Gateway → outbox → Connector → platform |
| **Stream source** | `AsyncIterable<string>` from AI SDK | `response.jsonl` byte offsets on PVC |
| **Edit coordination** | In-process: adapter holds message ID in memory | Cross-process: `.stream-{session_id}` state file on PVC |
| **Fallback strategy** | Post `"..."` + edit on timer (500ms) | Post first chunk + edit subsequent (2s poll) |
| **Native streaming** | Slack `chatStream` API | Not yet |
| **Platforms without edit** | Not handled (no WhatsApp/Telegram adapters) | Skip intermediates, send final only |
| **Error handling** | Silently ignore edit failures, guarantee final edit | TBD |

### What kk Should Adopt

**1. The two-tier pattern is correct.** Vercel validates our edit-in-place approach. Their fallback is identical to our plan: post one message, edit it repeatedly, final edit on completion.

**2. The Adapter trait design is clean.** Every adapter gets fallback streaming for free by implementing `postMessage()` + `editMessage()`. Only platforms with native APIs need to override `stream()`. kk should follow this pattern.

**3. Slack native streaming is worth supporting eventually.** `chat.startStream`/`appendStream`/`stopStream` have the highest rate limits (Tier 4: 100+/min on append) and the best UX (real-time, with Block Kit finalization). This should be a Phase 2 improvement.

**4. Timer-based editing is better than poll-based.** Vercel uses 500ms `setTimeout` recursion, which self-adjusts if the API is slow. kk's 2s poll cycle is coarser but acceptable for file-based IPC.

**5. Silent error handling on edits is pragmatic.** Vercel ignores intermediate edit failures and only guarantees the final edit. kk should do the same — if an `editMessageText` fails, log and move on. The final message delivery is what matters.

### What's Different for kk

**1. Cross-process state.** Vercel holds the message ID in memory within a single process. kk must persist it to a file (`.stream-{session_id}`) because Gateway and Connector are separate processes. This adds a failure mode (stale files) that Vercel doesn't have.

**2. No direct stream access.** Vercel adapters receive the `AsyncIterable<string>` directly. kk's Connector only sees outbox nq files — it doesn't know if more are coming. The `meta.streaming` flag and `meta.session_id` are our equivalent of the stream interface.

**3. Poll interval is coarser.** Vercel edits at 500ms. kk polls at 2s (Gateway results loop). For a file-based system this is fine, but means the user sees less frequent updates.

---

## 6. Proposed kk Rust Equivalent

A Rust port of the streaming pattern for kk's connector:

```rust
/// Trait that every platform provider must implement.
/// `post` + `edit` gives fallback streaming for free.
trait ChatProvider: Send + Sync {
    /// Send a new message, return platform-specific message ID.
    async fn post(&self, target: &Target, text: &str) -> Result<String>;

    /// Edit a previously sent message by platform ID.
    async fn edit(&self, target: &Target, msg_id: &str, text: &str) -> Result<()>;

    /// Optional: native streaming for platforms that support it (e.g. Slack).
    async fn stream(&self, _target: &Target, _chunks: Receiver<String>) -> Result<String> {
        Err(anyhow!("native streaming not supported"))
    }

    /// Whether this provider supports native streaming.
    fn supports_native_stream(&self) -> bool { false }
}

/// Target contains platform-specific routing info (chat_id, channel_id, thread_ts, etc.)
struct Target {
    chat_id: String,
    thread_id: Option<String>,
    meta: serde_json::Value,
}
```

The outbound loop would then be:

```rust
if is_streaming {
    let state_path = stream_state_path(outbox_dir, session_id);
    if state_path.exists() {
        // Edit existing
        let msg_id = fs::read_to_string(&state_path)?;
        provider.edit(&target, msg_id.trim(), &text).await?;
    } else {
        // First streaming message
        let msg_id = provider.post(&target, &text).await?;
        fs::write(&state_path, &msg_id)?;
    }
} else if let Some(state_path) = stream_state_for(session_id) && state_path.exists() {
    // Final edit
    let msg_id = fs::read_to_string(&state_path)?;
    provider.edit(&target, msg_id.trim(), &text).await?;
    fs::remove_file(&state_path)?;
} else {
    provider.post(&target, &text).await?;
}
```

This matches what we already have in `kk-connector/src/outbound.rs`, validating our approach.

---

## 7. Sources

- Vercel Chat SDK: https://github.com/vercel/chat
- Slack `chat.startStream`: https://docs.slack.dev/reference/methods/chat.startStream/
- Slack `chat.appendStream`: https://docs.slack.dev/reference/methods/chat.appendStream/
- Slack `chat.stopStream`: https://docs.slack.dev/reference/methods/chat.stopStream/
- Slack AI streaming announcement (Oct 2025): https://docs.slack.dev/changelog/2025/10/7/chat-streaming/
- Slack rate limits: https://docs.slack.dev/apis/web-api/rate-limits/
