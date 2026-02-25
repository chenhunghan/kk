# Agent Spawning: A Comparative Study

> How messaging-to-LLM bridges spawn code agents, maintain session continuity,
> handle errors, and stream responses back to users.

**Projects studied (by reading source code):**

| Project | Language | Stars | Execution Model |
|---------|----------|-------|-----------------|
| [OpenClaw](https://github.com/openclaw/openclaw) | TypeScript | 200K+ | In-process SDK + CLI subprocess |
| [NanoClaw](https://github.com/qwibitai/nanoclaw) | TypeScript | — | Docker containers running Claude Agent SDK |
| [IronClaw](https://github.com/nearai/ironclaw) | Rust | — | In-process loop + Docker sandbox + Claude CLI bridge |
| [PicoClaw](https://github.com/sipeed/picoclaw) | Go | — | In-process loop (CLI providers optional) |
| [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) | Rust | — | In-process loop (no CLI spawning) |
| **kk** (ours) | Rust | — | Ephemeral K8s Jobs running Claude Code CLI |

---

## Table of Contents

1. [Execution Models Taxonomy](#1-execution-models-taxonomy)
2. [Agent Spawning Deep Dive](#2-agent-spawning-deep-dive)
3. [Session Continuity](#3-session-continuity)
4. [Conversation Lifecycle](#4-conversation-lifecycle)
5. [Follow-Up / Hot Path Handling](#5-follow-up--hot-path-handling)
6. [Error Handling](#6-error-handling)
7. [Streaming Responses](#7-streaming-responses)
8. [Interruption Handling](#8-interruption-handling)
9. [Comparison Matrix](#9-comparison-matrix)
10. [Lessons for kk](#10-lessons-for-kk)

---

## 1. Execution Models Taxonomy

There are three fundamental approaches to "spawning an agent":

### Model A: In-Process LLM API Loop
The gateway process itself calls LLM APIs directly, manages tool execution, and iterates until a final response. No subprocess or container is created per message.

**Used by:** ZeroClaw, PicoClaw (primary), IronClaw (primary), OpenClaw (embedded PI agent)

```
User Message → Gateway Process → [LLM API call → Tool Exec → LLM API call → ...] → Response
```

### Model B: CLI Subprocess
The gateway spawns the `claude` (or `codex`) CLI binary as a child process, passing the prompt via CLI args or stdin, and capturing structured JSON/JSONL output.

**Used by:** OpenClaw (CLI runner), IronClaw (Claude bridge), PicoClaw (optional providers)

```
User Message → Gateway → spawn("claude -p <prompt> --output-format json") → parse stdout → Response
```

### Model C: SDK in Isolated Container
A Docker/K8s container is created with the Claude Agent SDK (or CLI) installed. The container stays alive across multiple messages for the same conversation, communicating via file-based IPC or HTTP.

**Used by:** NanoClaw (Docker + SDK), IronClaw (Docker sandbox), kk (K8s Jobs + CLI)

```
User Message → Orchestrator → Docker/K8s Container → [SDK query() or claude CLI] → Response
```

---

## 2. Agent Spawning Deep Dive

### OpenClaw

OpenClaw has **two modes** selected at runtime:

**Embedded PI Agent (primary):**
- `src/agents/pi-embedded-runner/run.ts` → `runEmbeddedPiAgent()`
- Uses `@mariozechner/pi-coding-agent` SDK in-process
- `createAgentSession()` builds an agent with tools, streaming function, and session manager
- Session transcript stored as JSONL files on disk

**CLI Agent Runner (Claude Code, Codex):**
- `src/agents/cli-runner.ts` → `runCliAgent()`
- CLI backends configured in `src/agents/cli-backends.ts`
- Spawned via a **process supervisor** with scope-based deduplication:

```typescript
// src/agents/cli-runner.ts
const supervisor = getProcessSupervisor();
const managedRun = await supervisor.spawn({
  sessionId, backendId, scopeKey,
  mode: "child",
  argv: [backend.command, ...args],  // e.g. ["claude", "-p", "--output-format", "json"]
  timeoutMs, noOutputTimeoutMs,
  cwd: workspaceDir, env,
  input: stdinPayload,
});
const result = await managedRun.wait();
```

Claude Code CLI backend config:
```typescript
const DEFAULT_CLAUDE_BACKEND = {
  command: "claude",
  args: ["-p", "--output-format", "json", "--dangerously-skip-permissions"],
  resumeArgs: ["--resume", "{sessionId}"],
  sessionMode: "always",
  sessionIdFields: ["session_id", "sessionId", "conversation_id", "conversationId"],
  systemPromptWhen: "first",  // only on first message
  serialize: true,             // queue CLI calls sequentially per session
};
```

### NanoClaw

Spawns **Docker containers** running the Claude Agent SDK:

```typescript
// src/container-runner.ts
const container = spawn(CONTAINER_RUNTIME_BIN, containerArgs, {
  stdio: ['pipe', 'pipe', 'pipe'],
});
```

Docker invocation:
```
docker run -i --rm --name nanoclaw-{group}-{timestamp} \
  -v {groupDir}:/workspace/group \
  -v {sessionsDir}:/home/node/.claude \
  -v {ipcDir}:/workspace/ipc \
  nanoclaw-agent:latest
```

Inside the container, `container/agent-runner/src/index.ts` uses the SDK:
```typescript
for await (const message of query({
  prompt: stream,
  options: {
    cwd: '/workspace/group',
    resume: sessionId,
    permissionMode: 'bypassPermissions',
    allowedTools: ['Bash', 'Read', 'Write', 'Edit', 'Glob', 'Grep', ...],
    mcpServers: { nanoclaw: { command: 'node', args: [mcpServerPath] } },
  }
})) { /* process streamed messages */ }
```

Key design: **one container per group**, stays alive across multiple messages via an internal query loop. Container input is JSON over stdin (initial config only), follow-up messages via file-based IPC.

### IronClaw

Two execution paths:

**In-process agentic loop** (`src/agent/dispatcher.rs`):
- `Agent::run_agentic_loop()` iterates LLM API calls with tool execution
- Tools executed in parallel via `JoinSet`
- Preflight hooks for approval checks

**Claude Code Bridge** (`src/worker/claude_bridge.rs`):
- Spawns `claude` CLI inside a Docker container
- Writes `.claude/settings.json` with tool allowlist
- Credentials injected via environment variables (never written to disk)
- Streams NDJSON output, capturing session_id from the stream
- Follow-up messages delivered via HTTP polling (`GET /worker/{job_id}/prompt`)

Container security model:
- Memory: 2GB, CPU: 1024 shares
- Non-root UID 1000, drop ALL capabilities
- Read-only root filesystem
- Network through HTTP proxy (allowlist enforcement)

### PicoClaw

**No external agent process by default.** The agent is a Go struct:

```go
// pkg/agent/instance.go
type AgentInstance struct {
    Provider    providers.LLMProvider
    Sessions    *session.SessionManager
    Tools       *tools.ToolRegistry
    MaxIterations int  // default 20
}
```

Optional CLI providers spawn subprocess **per LLM call** (not per conversation):
```go
// pkg/providers/claude_cli_provider.go
cmd := exec.CommandContext(ctx, p.command, args...)
cmd.Dir = p.workspace
cmd.Stdin = bytes.NewReader([]byte(prompt))
cmd.Run()
```

No session resume at the CLI level — PicoClaw manages its own history and reconstructs the full prompt each time.

### ZeroClaw

**Pure in-process model.** No subprocess spawning at all.

```rust
// src/agent/agent.rs
pub async fn turn(&mut self, user_message: &str) -> Result<String> {
    // Build system prompt, load memory, run tool-call loop, return text
    for _ in 0..self.config.max_tool_iterations {
        let response = self.provider.chat(request).await?;
        if response.tool_calls.is_empty() { return Ok(response.text); }
        let results = self.execute_tools(&response.tool_calls).await;
        // append results, loop
    }
}
```

Multi-agent delegation via in-process `delegate` tool (not external process):
- Recursion depth limit (default 3)
- 300-second timeout for agentic delegates
- `delegate` tool excluded from sub-agent registries to prevent infinite loops

### kk (ours)

Ephemeral **K8s Jobs** running `claude` CLI:

```rust
// crates/kk-agent/src/claude.rs
// Initial:
Command::new(&config.claude_bin)
    .args(["-p", &prompt, "--output-format", "stream-json", "--max-turns", &max_turns, "--verbose"])
    .current_dir(&session_dir)
    .stdout(File::create(response_jsonl))
    .stderr(File::create(agent_log))
    .status()

// Follow-up resume:
Command::new(&config.claude_bin)
    .args(["-p", &prompt, "--resume", "--output-format", "stream-json", "--max-turns", &max_turns, "--verbose"])
    .current_dir(&session_dir)
    .stdout(OpenOptions::append(response_jsonl))
    .stderr(OpenOptions::append(agent_log))
    .status()
```

All I/O via files on shared RWX PVC at `/data`. Zero network calls between components.

---

## 3. Session Continuity

How each project continues a conversation when the next message arrives.

### OpenClaw

**Embedded mode:** JSONL transcript files on disk. `SessionManager.open(sessionFile)` loads full history. On new message:
1. Open existing transcript file
2. Sanitize history (validate turn structure, repair broken tool pairs)
3. Truncate to fit context via `limitHistoryTurns()`
4. Call `activeSession.prompt(newMessage)` with full history loaded

**CLI mode:** Session ID captured from Claude's JSON output and stored in session entry:
```typescript
sessionIdFields: ["session_id", "sessionId", "conversation_id", "conversationId"]
```
On next message, `--resume {sessionId}` flag is used. System prompt only sent on first message (`systemPromptWhen: "first"`).

### NanoClaw

**Claude Agent SDK's built-in `resume` option:**
```typescript
query({ prompt, options: { resume: sessionId, resumeSessionAt: resumeAt } })
```

Session ID flow:
1. SDK emits `system/init` event with `session_id`
2. Container passes it back to host via stdout markers
3. Host persists to SQLite: `sessions(group_folder TEXT PRIMARY KEY, session_id TEXT)`
4. Next message passes stored `sessionId` to container input

Per-group isolation: each group gets its own `.claude/` directory mounted at `/home/node/.claude`, so session transcripts are physically separated on the host at `data/sessions/{groupFolder}/.claude/`.

### IronClaw

**In-process mode:** `SessionManager::resolve_thread()` maps `(user_id, channel, external_thread_id)` → internal UUID. History kept in-memory in `Session → Thread → Turn` hierarchy. Optional PostgreSQL persistence.

**Claude bridge mode:** Session ID captured from NDJSON stream output, then reused with `--resume` flag on follow-up spawns. Follow-up prompts queued in `VecDeque<PendingPrompt>` and served via HTTP polling.

### PicoClaw

JSON files on disk at `{workspace}/sessions/{sanitized_key}.json`:
```go
type Session struct {
    Key      string              `json:"key"`
    Messages []providers.Message `json:"messages"`
    Summary  string              `json:"summary,omitempty"`
}
```

Full message history loaded and sent with each LLM call. No CLI-level session resume — PicoClaw reconstructs the prompt from its own store. Auto-summarization when history exceeds 20 messages or 75% of context window.

### ZeroClaw

**In-memory only** — `Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>` keyed by sender+channel+thread. **Does not survive restarts.** Long-term recall via semantic memory (SQLite with vector embeddings, cosine similarity search).

On each message:
1. Retrieve in-memory history for sender
2. Enrich with semantic memory search results (up to 4 entries)
3. Send full history to LLM
4. Append response
5. Compact if over 50 messages (keep last 12, truncate to 600 chars)

### kk (ours)

**Within a single Job (hot path):** `--resume` flag on `claude` CLI, working directory persists.

**Across Jobs (cold path):** Session directory `/data/sessions/{group}/` persists on PVC. `.claude/` state survives. However, `--resume` does **not** work across separate Jobs — each new Job starts a fresh conversation. Cross-Job memory relies on curated `SOUL.md` and per-group `CLAUDE.md` files.

---

## 4. Conversation Lifecycle

### OpenClaw — Full Path

```
1. [Telegram/Slack/etc] → WebSocket → Gateway Server
2. Gateway resolves session key: "agent:{agentId}:{channel}:{chatType}:{peerId}"
3. Gateway responds immediately: { status: "accepted", runId }  (non-blocking)
4. Fires agentCommand() asynchronously
5. isCliProvider? → runCliAgent() or runEmbeddedPiAgent()
6. For embedded: createAgentSession() → session.prompt() → tool loop
7. For CLI: supervisor.spawn(["claude", "-p", ...]) → wait for exit → parse JSON
8. Response sent back through WebSocket → channel
```

### NanoClaw — Full Path

```
1. [WhatsApp/Telegram/etc] → storeMessage() in SQLite
2. Message loop (every 2s) → getNewMessages()
3. Trigger check: @mention required for non-main groups
4. Active container exists? → write IPC file to data/ipc/{group}/input/
5. No container? → GroupQueue → spawn Docker container
6. Container reads stdin JSON (sessionId, prompt, secrets)
7. agent-runner calls query() with resume: sessionId
8. SDK streams results → stdout markers → host parses → sends to chat
9. Container stays alive, polls IPC for next message (up to 30min idle)
10. Close sentinel → container exits gracefully
```

### IronClaw — Full Path

```
1. Channel.start() → MessageStream
2. Agent::handle_message() receives IncomingMessage
3. SessionManager::resolve_thread(user_id, channel, external_thread_id)
4. SubmissionParser::parse() — handle /interrupt, /undo, yes/no, or user input
5a. In-process: run_agentic_loop() → [LLM call → tool exec → repeat]
5b. Claude bridge: create Docker container → ClaudeBridgeRuntime
    → spawn claude CLI → stream NDJSON → post events to orchestrator
    → poll for follow-ups every 2s → --resume with captured session_id
6. Response delivered via channel.respond()
```

### PicoClaw — Full Path

```
1. Channel adapter → HandleMessage() → bus.PublishInbound()
2. AgentLoop.Run() → bus.ConsumeInbound() (sequential, one at a time)
3. Load session history from JSON file
4. ContextBuilder.BuildMessages(history, summary, newMessage)
5. runLLMIteration() loop (up to MaxIterations=20):
   - callLLM() → HTTP API call
   - If tool calls: execute sequentially → append results → loop
   - If no tool calls: break with final text
6. Save assistant response to session
7. maybeSummarize() if >20 messages or >75% context
8. Publish outbound → channel adapter → edit placeholder message
```

### ZeroClaw — Full Path

```
1. Channel.listen() → MPSC sender
2. run_message_dispatch_loop() → semaphore (8-64 concurrent)
3. Per-sender cancellation (Telegram): newer message cancels older
4. process_channel_message():
   a. Retrieve ConversationHistoryMap[sender]
   b. Build system prompt (identity + tools + channel instructions)
   c. Load semantic memory context (cosine similarity search)
   d. run_tool_call_loop() (up to max_tool_iterations=10)
   e. Stream draft updates to channel
   f. Append response to history, auto-save to memory
5. Compact history if over limit
```

### kk (ours) — Full Path

```
Cold path (no running Job):
1. Connector polls platform → writes inbound nq file to /data/inbox/{channel}/
2. Gateway inbound loop (2s) → reads nq → resolves group/thread
3. Gateway writes /data/results/{session-id}/request.json
4. Gateway creates K8s Job with env: SESSION_ID, GROUP, THREAD_ID, etc.
5. Agent Phase 0: symlink skills from /data/skills/
6. Agent Phase 1: read request.json → build prompt → spawn claude -p
7. Agent Phase 2: poll /data/groups/{group}/ for follow-up nq files (2s)
   → spawn claude -p --resume for each
8. Agent Phase 3: write "done" to status file
9. Gateway results loop → read response.jsonl → write to /data/outbox/
10. Connector polls outbox → delivers to platform

Hot path (Job already running):
1-2. Same as above
3. Gateway detects active Job → writes FollowUpMessage nq to queue dir
4. Agent Phase 2 picks it up within 2s → claude --resume
```

---

## 5. Follow-Up / Hot Path Handling

How each project handles a new message arriving while the agent is already working.

| Project | Mechanism | Latency |
|---------|-----------|---------|
| **OpenClaw** (embedded) | `queueEmbeddedPiMessage()` → `activeSession.steer(text)` — injects into running stream | ~instant |
| **OpenClaw** (CLI) | `serialize: true` — queued, next CLI spawn after current finishes | Seconds |
| **NanoClaw** | IPC file → `MessageStream.push()` → injected into active SDK query | <500ms |
| **IronClaw** (in-process) | Thread state machine — waits for current turn to complete | Seconds |
| **IronClaw** (bridge) | HTTP prompt queue → polled every 2s by bridge → `--resume` | 2-4s |
| **PicoClaw** | Sequential bus consumption — next message waits in channel buffer | Seconds-minutes |
| **ZeroClaw** | Per-sender cancellation (Telegram) — old request cancelled, new one starts | ~instant (destructive) |
| **kk** | nq file written to queue dir → Agent Phase 2 polls every 2s → `--resume` | 2-4s |

Notable approaches:

**OpenClaw's "steering"** is the most elegant — it injects a follow-up into the *currently running* LLM stream without restarting anything:
```typescript
export function queueEmbeddedPiMessage(sessionId: string, text: string): boolean {
  const handle = ACTIVE_EMBEDDED_RUNS.get(sessionId);
  if (!handle || !handle.isStreaming()) return false;
  void handle.queueMessage(text);  // calls activeSession.steer(text)
  return true;
}
```

**NanoClaw's MessageStream** is similar — an async iterator that allows pushing messages into an active query:
```typescript
class MessageStream {
  push(text: string): void {
    this.queue.push({ type: 'user', message: { role: 'user', content: text } });
    this.waiting?.();  // wake up async iterator
  }
  async *[Symbol.asyncIterator](): AsyncGenerator<SDKUserMessage> { ... }
}
```

**ZeroClaw's cancellation** is the most aggressive — it kills the in-flight request entirely:
```rust
// Per-sender CancellationToken tracked in in_flight_by_sender HashMap
// Newer message cancels the older one, old turn gets "[Task timed out]" marker
```

---

## 6. Error Handling

### Context Window Overflow

| Project | Strategy |
|---------|----------|
| **OpenClaw** | Auto-compaction (summarize old turns), up to 3 attempts. Tool result truncation. |
| **NanoClaw** | SDK handles internally. PreCompact hook archives transcripts. |
| **IronClaw** | `compact_messages_for_retry()` — strip oldest messages, keep system + last turn. |
| **PicoClaw** | Emergency compression: drop oldest 50% of conversation. Up to 2 retries. |
| **ZeroClaw** | `compact_sender_history()` — keep last 12 messages, truncate to 600 chars. |
| **kk** | Not handled at agent level — relies on Claude CLI's own limits (`--max-turns`). |

### Agent/Process Crash

| Project | Strategy |
|---------|----------|
| **OpenClaw** | Retry loop (up to 160 iterations), auth profile failover, rate limit cooldowns. |
| **NanoClaw** | Message cursor rollback (if no output sent to user). Exponential backoff: 5s→10s→20s→40s→80s. Max 5 retries. Orphan container cleanup on startup. |
| **IronClaw** | Thread state → `Interrupted`. Self-repair module detects stuck jobs. Container auto-removed on failure. |
| **PicoClaw** | Provider fallback chain with cooldown tracking. Tool errors fed back to LLM. |
| **ZeroClaw** | Component supervisor with exponential backoff. Per-sender cancellation prevents stuck states. |
| **kk** | Exit code 1 = fatal (write "error" status). Exit code 2+ = non-fatal (continue to Phase 2). Gateway cleanup loop detects crashed K8s Jobs and writes error status. K8s `ttlSecondsAfterFinished: 300` for automatic cleanup. |

### Auth / Rate Limiting

| Project | Strategy |
|---------|----------|
| **OpenClaw** | Multiple auth profiles with cycling. Cooldown per profile. Up to `PER_PROFILE * 8` retries. |
| **NanoClaw** | Secrets passed via stdin, deleted immediately. `PreToolUse` hook strips secrets from Bash. |
| **IronClaw** | Credentials fetched via authenticated HTTP API. Never written to disk. Container network through proxy. |
| **PicoClaw** | Failure classification (auth, rate_limit, billing, timeout, overloaded). Fallback chain. |
| **ZeroClaw** | Basic — propagates errors up. No multi-provider failover. |
| **kk** | `ANTHROPIC_API_KEY` from K8s Secret mounted as env var. No failover. |

### Timeout Handling

| Project | Overall | Per-Tool | Idle |
|---------|---------|----------|------|
| **OpenClaw** | `timeoutMs` per run | Tool-level timeouts | — |
| **NanoClaw** | `CONTAINER_TIMEOUT` (30min) | — | `IDLE_TIMEOUT` (30min) |
| **IronClaw** | 600s default | Per-tool `execution_timeout()` | — |
| **PicoClaw** | Via `exec.CommandContext` | Shell: 60s | — |
| **ZeroClaw** | `max_tool_iterations * 4x` | Shell: 60s, delegate: 120-300s | — |
| **kk** | K8s `activeDeadlineSeconds: 300` | `--max-turns` (25) | `IDLE_TIMEOUT` (120s) |

---

## 7. Streaming Responses

How each project delivers responses back to users in real-time (or not).

### Real-Time Token Streaming

**OpenClaw (embedded):** Full token-by-token streaming via WebSocket broadcast, throttled to 150ms:
```typescript
const emitChatDelta = (sessionKey, clientRunId, sourceRunId, seq, text) => {
  const now = Date.now();
  if (now - last < 150) return;  // throttle
  broadcast("chat", { ... });
};
```

Events: `onPartialReply` (deltas), `onBlockReply` (complete blocks), `onReasoningStream` (thinking tokens), `onToolResult` (tool outputs).

**ZeroClaw:** Progressive draft updates to Telegram/channels that support it:
```rust
pub trait Channel {
    fn supports_draft_updates(&self) -> bool;
    async fn send_draft(&self, message: &SendMessage) -> Result<Option<String>>;
    async fn update_draft(&self, recipient: &str, message_id: &str, text: &str) -> Result<()>;
    async fn finalize_draft(&self, recipient: &str, message_id: &str, text: &str) -> Result<()>;
}
```
Minimum 80 chars per update. Final message gets full Markdown formatting.

**IronClaw:** SSE event pipeline from workers/bridge to web gateway:
```rust
enum SseEvent {
    JobMessage(text),     // assistant/user messages
    JobToolUse(name),     // tool invocations
    JobToolResult(result),// tool outputs
    JobResult(session_id),// completion
    JobStatus(status),    // generic status
}
```
Broadcast via `tokio::sync::broadcast::Sender`.

### Store-and-Forward (No Real-Time Streaming)

**OpenClaw (CLI):** No streaming. Waits for process exit, then parses output:
```typescript
const result = await managedRun.wait();
const stdout = result.stdout.trim();
```

**NanoClaw:** Marker-based stdout protocol. Each complete response wrapped in sentinels:
```
---NANOCLAW_OUTPUT_START---
{"status":"success","result":"...","newSessionId":"abc123"}
---NANOCLAW_OUTPUT_END---
```
Parsed incrementally as data arrives, but each marker represents a complete response — no token-level streaming to end users. Typing indicator shown while agent works.

**PicoClaw:** Full buffered response. Shows "Thinking..." placeholder, then edits it with the final answer.

**kk (ours):** Store-and-forward via files. Claude CLI writes JSONL to `response.jsonl` incrementally, but Gateway waits for `status=done` before reading:
```
Agent writes stream-json → response.jsonl (real-time on disk)
Gateway polls status file every 2s
On "done": reads last "type":"result" line → writes to outbox
Connector polls outbox every 1s → delivers to platform
```

The JSONL file *does* contain real-time streaming data — the Gateway just doesn't read it until completion.

---

## 8. Interruption Handling

### User-Initiated Stop

| Project | Mechanism |
|---------|-----------|
| **OpenClaw** | `/stop` command → `isChatStopCommandText()` → `AbortController.abort()` → `activeSession.abort()` |
| **NanoClaw** | `_close` sentinel file written to IPC dir → container exits query loop |
| **IronClaw** | `/interrupt` command → `ThreadState::Interrupted` → checked at each loop iteration |
| **PicoClaw** | No explicit mechanism — messages processed sequentially |
| **ZeroClaw** | Per-sender `CancellationToken` — new message cancels in-flight request |
| **kk** | No user-initiated stop — relies on K8s `activeDeadlineSeconds` |

### Graceful Shutdown

| Project | Mechanism |
|---------|-----------|
| **OpenClaw** | AbortController propagation, timeout timer cleanup |
| **NanoClaw** | `shuttingDown = true`, containers finish via their own timeouts (not killed) |
| **IronClaw** | Tokio task cancellation, container auto-remove |
| **PicoClaw** | Context cancellation via Go context |
| **ZeroClaw** | Component supervisor stops, tasks cancelled |
| **kk** | K8s sends SIGTERM to pod, `activeDeadlineSeconds` hard limit |

---

## 9. Comparison Matrix

### Spawning

| Feature | OpenClaw | NanoClaw | IronClaw | PicoClaw | ZeroClaw | kk |
|---------|----------|----------|----------|----------|----------|------|
| Spawns external process | Yes (CLI mode) | Yes (Docker) | Yes (Docker) | Optional | No | Yes (K8s Job) |
| Process per message | No (per session) | No (per group) | No (per job) | Yes (CLI mode) | No | No (per session) |
| Container isolation | No | Docker | Docker | No | No | K8s Pod |
| Concurrency limit | Lane-based serialization | MAX_CONCURRENT=5 | Semaphore | Sequential (1) | Semaphore (8-64) | K8s resources |
| Subagent support | Yes (`sessions_spawn`) | No | Yes (in-process) | Yes (goroutines) | Yes (`delegate` tool) | No |

### Session Persistence

| Feature | OpenClaw | NanoClaw | IronClaw | PicoClaw | ZeroClaw | kk |
|---------|----------|----------|----------|----------|----------|------|
| Storage | JSONL files | SQLite + .claude/ dir | In-memory + PostgreSQL | JSON files | In-memory + SQLite vectors | PVC files |
| Survives restart | Yes | Yes | Yes (with DB) | Yes | Partial (memory only) | Yes |
| Cross-session resume | `--resume {id}` | SDK `resume: id` | `--resume` (bridge) | No (full replay) | No | `--resume` (within Job) |
| Auto-compaction | Yes (LLM summary) | SDK internal | Yes (strip oldest) | Yes (LLM summary) | Yes (keep last 12) | No |

### Streaming

| Feature | OpenClaw | NanoClaw | IronClaw | PicoClaw | ZeroClaw | kk |
|---------|----------|----------|----------|----------|----------|------|
| Token streaming | Yes (embedded) | No | Yes (SSE) | No | Yes (draft updates) | No |
| Typing indicator | Via channel | Yes | Via StatusUpdate | Placeholder msg | Via draft | No |
| Partial results | Every 150ms | Marker-based chunks | Per-event | No | 80-char minimum | No |

---

## 10. Lessons for kk

### What We Could Adopt

#### 1. Streaming: Read response.jsonl Incrementally
kk already has real-time JSONL data — Claude CLI writes `stream-json` output line by line. The Gateway currently waits for `status=done`. We could:
- **Tail `response.jsonl`** and forward partial `assistant` blocks to the outbox as they appear
- Use a simple "last seen offset" to avoid re-reading
- This is essentially what NanoClaw's stdout marker parsing does, but we'd read from a file instead of stdout

#### 2. Session Resume Across Jobs
Currently `--resume` only works within a single Job's Phase 2. To enable cross-Job resume:
- **Capture session_id** from Claude's `stream-json` output (the `system/init` event contains it)
- **Store it** in a file at `/data/sessions/{group}/session_id`
- **Pass `--resume {id}`** on the next Job's Phase 1 spawn
- This is exactly how OpenClaw (CLI mode), NanoClaw, and IronClaw (bridge) all do it

#### 3. Message Injection During Active Query
OpenClaw's "steering" and NanoClaw's `MessageStream` both allow injecting messages into a running LLM query without restarting. This is a limitation of using `claude -p` — the CLI doesn't support mid-query injection. Options:
- **Switch to Claude Agent SDK** (like NanoClaw) for programmatic control
- **Accept the 2s poll latency** in Phase 2 (current approach, works fine)

#### 4. Context Overflow Handling
All other projects handle context overflow at the orchestrator level. kk relies entirely on Claude CLI's `--max-turns`. We could:
- Monitor `response.jsonl` for context length warnings
- Implement compaction at the Gateway level before creating follow-up prompts

#### 5. User-Initiated Stop
Every project except kk supports user-initiated interruption. Options:
- Write a `_stop` sentinel file to the queue directory (like NanoClaw's `_close`)
- Agent Phase 2 detects it and exits immediately
- Gateway marks the Job for deletion

#### 6. Error Recovery with Cursor Rollback
NanoClaw's pattern of rolling back the message cursor on error (only if no output was sent to the user) prevents message loss:
```typescript
if (hadError && !outputSentToUser) {
  lastAgentTimestamp[chatJid] = previousCursor;  // rollback
}
```
kk could implement similar logic in the Gateway's results loop.

### What We Do Better

#### 1. K8s-Native Isolation
kk's ephemeral K8s Jobs provide stronger isolation than Docker containers. Each conversation gets its own pod with resource limits, network policies, and service account scoping. No shared process memory between conversations.

#### 2. File-Based Communication
The shared PVC approach is simpler and more debuggable than HTTP APIs (IronClaw), WebSocket events (OpenClaw), or in-memory channels (PicoClaw/ZeroClaw). Every message is a file you can `cat`.

#### 3. Zero Network Dependencies
kk components have no network coupling. NanoClaw/IronClaw/OpenClaw all require HTTP/WebSocket connectivity between components. kk's PVC-only communication survives network partitions.

#### 4. Deterministic Replay
Because everything is files, kk conversations can be replayed by re-creating the same file structure. Other projects would need database dumps or event log replay.

### Priority Improvements

1. **High: Cross-Job session resume** — capture and persist session_id from stream-json output
2. **High: Streaming responses** — tail response.jsonl incrementally instead of waiting for "done"
3. **Medium: User-initiated stop** — `_stop` sentinel file pattern
4. **Medium: Context overflow handling** — detect and compact at Gateway level
5. **Low: Error cursor rollback** — prevent message loss on agent failures
