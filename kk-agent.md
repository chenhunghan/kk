# kk: Agent Job

> **Cross-references:**
> [Communication Protocol](kk-protocol.md) — message formats §4.3, §4.5, §4.6; file paths §5; polling intervals §7
> [Controller](kk-controller.md) ·
> [Connector](kk-connector.md) ·
> [Gateway](kk-gateway.md) — creates Agent Jobs (cold path), writes follow-ups (hot path)
> [Skill](kubeclaw-plan-skill.md) — skills live on PVC, symlinked in Phase 0

---

## Summary

The Agent Job is an ephemeral K8s Job (one per invocation). It is created by the Gateway when a message triggers the cold path. The Job runs a single pod that executes the `claude` CLI, reads per-group context and skills, polls for follow-up messages, and writes results.

The Agent Job has **no knowledge** of the Gateway, Connectors, Channels, or the K8s API. It only interacts with the shared PVC via well-defined file paths.

### Implementation

The agent is implemented as a Rust crate (`crates/kk-agent`) in the kk monorepo. It reuses `kk-core` for queue operations, path conventions, message types, and structured logging. The agent is purely sequential — no async runtime, no tokio. It uses `std::process::Command` to spawn the claude CLI and `std::thread::sleep` for polling.

### Lifecycle

```
Gateway creates Job → Pod scheduled → Phase 0 (skills) → Phase 1 (prompt) → Phase 2 (follow-ups) → Phase 3 (done) → Pod exits
```

| Phase | Duration | What happens |
|---|---|---|
| **Phase 0** | ~0.5s | Symlink skills into session directory |
| **Phase 1** | 3–60s | Read prompt from `request.json`, build context, run claude, write streaming response |
| **Phase 2** | 0–120s | Poll per-group queue for follow-ups, resume claude for each |
| **Phase 3** | ~0.1s | Write final status, exit |

---

## Crate Structure

```
crates/kk-agent/
  Cargo.toml
  src/
    lib.rs          # pub mod declarations
    main.rs         # fn main() → run() sequential entrypoint
    config.rs       # AgentConfig::from_env()
    claude.rs       # spawn_claude(), spawn_claude_resume() via std::process::Command
    phases.rs       # phase_0_skills, phase_1_prompt, phase_2_followups, phase_3_done
  tests/
    e2e_agent.rs    # 19 integration tests with mock claude binary
```

### Dependencies (minimal)

```toml
[dependencies]
kk-core = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
```

No tokio, no kube, no k8s-openapi.

### kk-core reuse

| Need | kk-core provides | Module |
|---|---|---|
| Queue polling | `nq::list_pending`, `nq::read_message`, `nq::delete` | `nq` |
| All paths | `DataPaths` — `results_dir`, `request_manifest`, `result_status`, `result_response`, `skills_dir`, `session_dir_threaded`, `group_queue_dir_threaded`, `soul_md`, `group_claude_md` | `paths` |
| Follow-up type | `FollowUpMessage` | `types` |
| Request manifest | `RequestManifest`, `RequestMessage` | `types` |
| Status enum | `ResultStatus` with `as_str()` | `types` |
| Logging | `logging::init()` | `logging` |

---

## Image

```
kk-agent:latest

Base:    debian:bookworm-slim (Claude Code needs glibc)
Install:
  - kk-agent binary (Rust, compiled from crate)
  - claude-code CLI (npm install -g @anthropic-ai/claude-code)
  - node 20+ (required by claude-code)
  - git (claude may use it as a tool)
  - common CLI tools: curl, jq, ripgrep
Size:    ~400MB
```

### Why not Alpine?

Claude Code depends on native Node modules that require glibc. Alpine uses musl, which causes compatibility issues. Debian slim is the safe choice.

---

## Environment Variables

Set by the Gateway when creating the Job (see [Gateway](kk-gateway.md) — Job Spec Builder):

| Var | Example | Required | Description |
|---|---|---|---|
| `SESSION_ID` | `family-chat-1708801290` | **yes** | Unique session ID — determines results dir |
| `GROUP` | `family-chat` | **yes** | Group slug — determines session dir and follow-up queue |
| `DATA_DIR` | `/data` | no (default `/data`) | PVC mount path |
| `IDLE_TIMEOUT` | `120` | no (default `120`) | Seconds to wait for follow-ups before exiting Phase 2 |
| `MAX_TURNS` | `25` | no (default `25`) | Max agentic turns for `claude -p` |
| `THREAD_ID` | `42` | no | Thread ID within group — enables thread-aware paths |
| `CLAUDE_BIN` | `claude` | no (default `claude`) | Path to claude binary (overridable for testing) |

The initial prompt is **not** passed via env var. It is read from `request.json` (written by the Gateway before the Job starts) — see Phase 1.

From Secret (`kk-api-keys` via `envFrom`):

| Var | Description |
|---|---|
| `ANTHROPIC_API_KEY` | API key for Claude |

---

## Directory Paths

All paths are relative to `DATA_DIR` (`/data`). The Agent Job only touches these directories:

| Path | Access | Purpose |
|---|---|---|
| `/data/skills/` | **Read** | All installed skills (symlinked into session in Phase 0) |
| `/data/sessions/{GROUP}/` | **Read + Write** | Claude Code working directory (non-threaded) |
| `/data/sessions/{GROUP}/threads/{THREAD_ID}/` | **Read + Write** | Claude Code working directory (threaded) |
| `/data/memory/SOUL.md` | **Read** | Global agent personality |
| `/data/memory/{GROUP}/CLAUDE.md` | **Read** | Per-group context |
| `/data/results/{SESSION_ID}/` | **Write** | Status file, response.jsonl, agent.log |
| `/data/groups/{GROUP}/` | **Read** (delete after read) | Follow-up queue (non-threaded) |
| `/data/groups/{GROUP}/threads/{THREAD_ID}/` | **Read** (delete after read) | Follow-up queue (threaded) |

Session dir and follow-up queue are **thread-aware**: when `THREAD_ID` is set, `DataPaths::session_dir_threaded` and `DataPaths::group_queue_dir_threaded` append `/threads/{THREAD_ID}/` to the base path.

The Agent Job **never** reads or writes:
- `/data/inbound/` (Gateway's domain)
- `/data/outbox/` (Gateway + Connector's domain)
- `/data/state/` (Gateway's domain)

---

## Entrypoint (`main.rs`)

The agent runs as a sequential Rust binary. No async runtime.

```rust
fn run() -> Result<()> {
    kk_core::logging::init();

    let config = AgentConfig::from_env()?;
    let paths = DataPaths::new(&config.data_dir);

    let session_dir = paths.session_dir_threaded(&config.group, config.thread_id.as_deref());
    std::fs::create_dir_all(&session_dir)?;
    std::fs::create_dir_all(paths.results_dir(&config.session_id))?;

    phase_0_skills(&paths, &session_dir)?;
    phase_1_prompt(&config, &paths, &session_dir)?;
    phase_2_followups(&config, &paths, &session_dir)?;
    phase_3_done(&paths, &config.session_id)?;

    Ok(())
}
```

On error: log via `tracing`, print to stderr, `std::process::exit(1)`.

---

## Phase 0: Skill Injection (Detail)

### What happens

The Agent symlinks every skill directory from `/data/skills/{name}/` into the session's `.claude/skills/` and `.agents/skills/` directories. This makes skills discoverable by the agent's native skill-loading mechanism.

Implementation: `phases::phase_0_skills` reads the skills directory with `std::fs::read_dir`, validates each has `SKILL.md`, then creates symlinks via `std::os::unix::fs::symlink`. Existing symlinks are removed with `std::fs::remove_file` before re-creating (equivalent to `ln -sfn`).

### Symlink structure (after Phase 0)

```
/data/sessions/family-chat/
├── .claude/
│   └── skills/
│       ├── frontend-design → /data/skills/frontend-design/
│       ├── vercel-deploy   → /data/skills/vercel-deploy/
│       └── agent-browser   → /data/skills/agent-browser/
├── .agents/
│   └── skills/
│       ├── frontend-design → /data/sessions/family-chat/.claude/skills/frontend-design
│       ├── vercel-deploy   → /data/sessions/family-chat/.claude/skills/vercel-deploy
│       └── agent-browser   → /data/sessions/family-chat/.claude/skills/agent-browser
└── ...
```

Note: `.agents/skills/` symlinks point to the `.claude/skills/` symlinks (chained), so there is a single canonical source (`/data/skills/`). If a skill is deleted from the PVC between Job runs, the symlink becomes dangling — the agent safely ignores it.

### Why symlinks, not copies?

- **Instant**: no file I/O proportional to skill size
- **Shared state**: if a skill is updated on the PVC (rare — requires delete+recreate CR), the next Job automatically picks up the new version
- **No disk waste**: skills aren't duplicated per session

### Edge cases

| Scenario | Handling |
|---|---|
| No skills installed | Phase 0 completes immediately, log "no skills directory found" |
| Skill dir exists but missing SKILL.md | Skip with warning log |
| Symlink already exists from previous Job | Removed and re-created cleanly |
| Skill deleted from PVC after symlink created | Dangling symlink — agent ignores gracefully |
| Hundreds of skills | Symlink loop still < 1s. Not a practical concern. |

---

## Phase 1: Initial Prompt (Detail)

### Prompt Source

The prompt is read from `request.json` (written by the Gateway before the Job starts):

```rust
let manifest: RequestManifest = serde_json::from_str(&manifest_data)?;
let prompt_text = manifest.messages.last().map(|m| m.text.as_str()).unwrap_or("");
```

The `RequestManifest` (Protocol §4.4) contains the channel, group, sender, and a `messages` array. The agent uses the text of the **last message** as the initial prompt.

### Context Assembly

The full prompt sent to the LLM is built from up to three pieces, joined with `\n\n---\n\n` separators:

```
┌─────────────────────────────────────────────┐
│ SOUL.md (global personality)                │
│                                             │
│ You are Andy, a helpful AI assistant.       │
│ You are part of a family group chat.        │
│ Be friendly, concise, and helpful.          │
├─────────────────────────────────────────────┤
│ --- (separator)                             │
├─────────────────────────────────────────────┤
│ {GROUP}/CLAUDE.md (per-group context)       │
│                                             │
│ This is the family group chat.              │
│ Members: John, Sarah, kids.                 │
│ John likes weather updates.                 │
│ Sarah prefers calendar summaries.           │
├─────────────────────────────────────────────┤
│ --- (separator)                             │
├─────────────────────────────────────────────┤
│ Prompt (from request.json last message)     │
│                                             │
│ what's the weather?                         │
└─────────────────────────────────────────────┘
```

Implementation: `build_prompt()` collects non-empty parts into a `Vec<&str>` and joins with `"\n\n---\n\n"`. Empty or missing context files are silently skipped.

### Missing context files

| File | Missing? | Behavior |
|---|---|---|
| `SOUL.md` | OK | No global personality. Prompt starts with group context or raw prompt. |
| `{GROUP}/CLAUDE.md` | OK | No per-group context. Prompt starts with raw prompt. |
| Both missing | OK | LLM receives only the user's prompt text. |

### LLM Execution

`claude::spawn_claude` runs the claude CLI via `std::process::Command`:

```
claude -p <prompt> --output-format stream-json --max-turns <N> --verbose
```

| Flag | Purpose |
|---|---|
| `-p` | Non-interactive prompt mode (pipe in, get output, exit) |
| `--output-format stream-json` | Output as JSONL stream (Protocol §4.6) |
| `--max-turns N` | Limit agentic tool-use loops (default 25) |
| `--verbose` | Include tool calls and reasoning in stderr (→ agent.log) |

### Working Directory

`Command::current_dir(session_dir)` sets the cwd to the session directory. This is critical because:
- Claude Code looks for `.claude/` in the current working directory
- Skill symlinks in `.claude/skills/` are discovered relative to cwd
- Claude Code session state (`.claude/` internal files) persists across invocations for the same group

### Output Streams

| Stream | Destination | Mode | Format |
|---|---|---|---|
| stdout | `{RESULTS_DIR}/response.jsonl` | Create (Phase 1) / Append (Phase 2) | JSONL — structured response stream (Protocol §4.6) |
| stderr | `{RESULTS_DIR}/agent.log` | Create (Phase 1) / Append (Phase 2) | Plain text — tool calls, verbose reasoning, errors |

### Exit Code Handling

| Exit Code | Meaning | Action |
|---|---|---|
| 0 | Success | Continue to Phase 2 |
| 1 | Fatal error (auth failure, invalid config) | Write `status=error`, bail with `anyhow` error |
| 2+ | Non-fatal error (tool failure, rate limit) | Log warning, continue to Phase 2 |

---

## Phase 2: Follow-up Polling (Detail)

### How it works

After the initial prompt completes, the Agent enters a polling loop on the per-group queue directory. The Gateway writes follow-up messages here when it detects a running Job for the group (hot path).

```
Gateway (hot path)                    Agent Job (Phase 2)
    │                                       │
    │  write ,{ts}.{id}.nq to               │
    │  /data/groups/{group}/                 │
    │ ─────────────────────────────────────► │
    │                                       │  nq::list_pending()
    │                                       │  nq::read_message()
    │                                       │  serde_json::from_slice::<FollowUpMessage>()
    │                                       │  spawn_claude_resume("[Follow-up from {sender}]: {text}")
    │                                       │  append to response.jsonl
    │                                       │  nq::delete()
    │                                       │
    │                                       │  std::thread::sleep(2s), poll again
    │                                       │  ...
    │                                       │  idle for IDLE_TIMEOUT → exit loop
```

Queue operations use `kk_core::nq` — `list_pending()` reads `read_dir`, filters for `,*.nq` files, and sorts by filename for FIFO ordering.

### The `--resume` Flag

`claude -p "follow-up text" --resume` tells Claude Code to:
1. Load the existing session state from `.claude/` in cwd
2. Continue the conversation with the new prompt appended
3. Maintain tool state, memory, and context from Phase 1

`claude::spawn_claude_resume` opens stdout/stderr with `OpenOptions::append(true)` to append to the same response.jsonl and agent.log files.

### Follow-up Message Format

The Agent reads follow-up messages from the per-group queue (Protocol §4.3):

```jsonc
{
  "sender": "John",
  "text": "also check my calendar",
  "timestamp": 1708801300,
  "channel": "telegram-bot-1",
  "meta": { ... }
}
```

The Agent only uses `sender` and `text`. The rest is ignored (routing info for the Gateway).

The follow-up is wrapped with sender context before being sent to the LLM:

```
[Follow-up from John]: also check my calendar
```

This helps the LLM understand that a new message arrived from a specific person, especially in multi-user group chats.

### Thread-Aware Queue

When `THREAD_ID` is set, the agent polls the thread-specific queue:
- Non-threaded: `/data/groups/{GROUP}/`
- Threaded: `/data/groups/{GROUP}/threads/{THREAD_ID}/`

This is handled by `DataPaths::group_queue_dir_threaded(group, thread_id.as_deref())`.

### Context Overflow Detection

After each Claude invocation (Phase 1 and each Phase 2 follow-up), the Agent checks for context overflow by scanning both `response.jsonl` and `agent.log` for patterns:
- `context_length_exceeded`
- `context window`
- `maximum context length`
- `token limit`
- `too many tokens`

If overflow is detected, the Agent writes `status=overflow` and exits immediately. The Gateway's results loop picks this up via `process_overflow`, which sends the last response text with an overflow notice.

### Stop Sentinel

At the start of each Phase 2 poll iteration, the Agent checks for a stop sentinel file at `/data/groups/{group}/_stop` (or `/data/groups/{group}/threads/{tid}/_stop` for threaded). This file is written by the Gateway when a user sends `/stop`.

When detected:
1. Delete the sentinel file
2. Write `status=stopped`
3. Return immediately (skip remaining follow-ups)

### Idle Timeout Behavior

```
IDLE_TIMEOUT = 120 seconds (default, configurable via env)
POLL_INTERVAL = 2 seconds (hardcoded constant)

Timeline:
  T+0s    Phase 2 starts, last_activity = Instant::now()
  T+2s    Poll: no messages → sleep 2s
  T+4s    Poll: no messages → sleep 2s
  ...
  T+10s   Poll: message found! → process → last_activity = Instant::now() (RESET)
  T+12s   Poll: no messages → sleep 2s
  ...
  T+130s  last_activity.elapsed() >= 120s → exit Phase 2
```

The idle timer uses `std::time::Instant` and **resets** every time messages are processed. This means:
- If users keep sending follow-ups, the Job stays alive indefinitely (up to `JOB_ACTIVE_DEADLINE` — 300s)
- If no follow-ups arrive for `IDLE_TIMEOUT` seconds, the Job exits gracefully

### Multiple Messages in One Poll

If multiple follow-up messages are pending when the Agent polls, they are all processed in FIFO order (sorted by filename = timestamp). The idle timer resets after processing all of them.

### Follow-up Processing Errors

| Error | Handling |
|---|---|
| Empty text field | Skip message, delete file, log warning |
| JSON parse error | Skip message, delete file, log warning |
| `nq::read_message` fails | Skip message, delete file, log warning |
| `claude --resume` fails to spawn | Log error, continue polling |
| `claude --resume` exits non-zero | Log warning, continue polling (non-fatal) |

---

## Phase 3: Done (Detail)

### What happens

```
1. Write "done" to {RESULTS_DIR}/status    (Protocol §4.5)
2. Log completion
```

Implementation: `std::fs::write(status_path, ResultStatus::Done.as_str())`.

Note: The status file may already contain `stopped` or `overflow` if the Agent exited early from Phase 2 due to a stop sentinel or context overflow. In those cases, Phase 3 is not reached — the Agent returns directly.

The Gateway's results loop polls this file. On seeing `done`/`stopped`/`overflow`/`error`, it reads `response.jsonl` and `request.json` to route the response.

### What the Agent does NOT clean up

| Resource | Cleaned up by |
|---|---|
| Results directory (`/data/results/{session}/`) | Gateway (archives to `.done/`, then purges after 24h) |
| Session directory (`/data/sessions/{group}/`) | Never — persistent across Jobs for the same group |
| Skill symlinks in session dir | Overwritten by next Job's Phase 0 |
| Per-group queue directory | Persists — reused by future Jobs |
| The K8s Job itself | K8s `ttlSecondsAfterFinished` + Gateway cleanup loop |

---

## Resource Limits

Set by Gateway when creating the Job:

| Resource | Request | Limit | Rationale |
|---|---|---|---|
| CPU | 250m | 1 core | Claude CLI is mostly I/O-bound (waiting for API). Spikes during tool execution. |
| Memory | 256Mi | 1Gi | Node.js baseline ~100MB. Spikes if Claude uses large tools or reads big files. |

### Active Deadline

```yaml
activeDeadlineSeconds: 300    # 5 minutes max
```

This is a hard ceiling on total Job execution time. If the Job exceeds this, K8s kills the pod. The Gateway's cleanup loop detects this as a failed Job.

### TTL After Finished

```yaml
ttlSecondsAfterFinished: 300  # 5 minutes
```

K8s automatically deletes completed Jobs after this period. This is a backup to the Gateway's cleanup loop.

---

## Results Directory Structure

After a complete Job run, the results directory looks like:

```
/data/results/family-chat-1708801290/
├── request.json              ← Written by Gateway (before Job starts)
│                                Protocol §4.4 — routing info
│
├── status                    ← Written by Agent
│                                "running" → "done" / "error" / "stopped" / "overflow"
│                                Protocol §4.5
│
├── response.jsonl            ← Written by Agent (streaming, appended)
│                                Protocol §4.6 — JSONL stream from claude
│                                Phase 1 output + Phase 2 follow-up outputs
│
└── agent.log                 ← Written by Agent (stderr from claude)
                                 Tool calls, reasoning, errors, debug info
```

### response.jsonl Content Example

```jsonc
// Phase 1: initial prompt response
{"type":"system","message":"Claude Code session started"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"weather","input":{"city":"Helsinki"}}]}}
{"type":"tool_result","content":"22°C, sunny, humidity 45%"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The weather in Helsinki is 22°C and sunny with 45% humidity."}]}}
{"type":"result","result":"The weather in Helsinki is 22°C and sunny with 45% humidity."}

// Phase 2: follow-up response (appended)
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"calendar","input":{"date":"today"}}]}}
{"type":"tool_result","content":"2 meetings: standup at 10am, 1:1 with Sarah at 2pm"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Your calendar shows 2 meetings today:\n- Standup at 10am\n- 1:1 with Sarah at 2pm"}]}}
{"type":"result","result":"Your calendar shows 2 meetings today:\n- Standup at 10am\n- 1:1 with Sarah at 2pm"}
```

The Gateway extracts the **last** `"type":"result"` line as the final response text (Protocol §4.6). For follow-ups, the last result line contains the follow-up response, which is the most recent output the user should see.

---

## Session Persistence

The session directory `/data/sessions/{group}/` persists across Job runs for the same group. This means:

### What persists

| What | Where | Effect |
|---|---|---|
| Claude Code session state | `.claude/` internal files | `--resume` works across follow-ups within the same Job |
| Skill symlinks | `.claude/skills/`, `.agents/skills/` | Refreshed each Job (Phase 0 overwrites) |
| Files created by Claude | Any files claude creates in cwd | Available to future Jobs for the same group |

### Cross-Job Session Resume

The Agent persists the Claude session ID to a file after each invocation. On the next Job for the same group, Phase 1 reads this file and passes the session ID to `claude --resume {session-id}`, allowing Claude Code to continue the conversation from the previous Job.

| What | Where | Effect |
|---|---|---|
| Claude session ID file | `/data/sessions/{group}/.claude-session-id` | `--resume {id}` loads previous conversation across Jobs |
| In-memory state | Pod is gone | No process state carries over — only the Claude session persists |

If the session ID file is missing or empty (first Job for a group), Phase 1 starts a fresh session.

---

## Dockerfile

```dockerfile
FROM debian:bookworm-slim

# System dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    git \
    jq \
    ripgrep \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Node.js 20 LTS
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Claude Code CLI
RUN npm install -g @anthropic-ai/claude-code

# kk-agent binary
COPY kk-agent /usr/local/bin/kk-agent

WORKDIR /data
ENTRYPOINT ["kk-agent"]
```

---

## Error Handling

### Fatal Errors (Agent exits, status=error)

| Error | Detection | Result |
|---|---|---|
| `SESSION_ID` or `GROUP` missing | `AgentConfig::from_env()` returns `Err` | `std::process::exit(1)` |
| `ANTHROPIC_API_KEY` missing/invalid | `claude` exits 1 | `status=error`, `anyhow` bail |
| `claude` binary not found | `Command::status()` returns `Err` | `anyhow` propagation, `std::process::exit(1)` |
| PVC not mounted / permissions | `create_dir_all`/`write` fails | `anyhow` propagation, `std::process::exit(1)` |
| `request.json` missing or invalid | `read_to_string`/`serde_json` fails | `anyhow` propagation, `std::process::exit(1)` |
| No prompt text in request manifest | `bail!("no prompt text...")` | `std::process::exit(1)` |

### Non-Fatal Errors (Agent continues)

| Error | Detection | Result |
|---|---|---|
| `claude -p` exits with code 2+ | Exit code check | Log warning, continue to Phase 2 |
| `claude --resume` fails on a follow-up | Exit code check or spawn error | Log warning/error, continue polling |
| Follow-up message has empty text | `text.trim().is_empty()` | Skip message, delete file, log warning |
| Follow-up message is invalid JSON | `serde_json::from_slice` error | Skip message, delete file, log warning |
| SOUL.md missing | `read_to_string` returns `Err` (`.ok()`) | No global context — OK |
| GROUP/CLAUDE.md missing | `read_to_string` returns `Err` (`.ok()`) | No group context — OK |
| Skills dir empty or missing | `exists()` check / empty `read_dir` | No skills — OK |

### Timeout

If the Job hits `activeDeadlineSeconds` (300s), K8s kills the pod. In this case:
- The status file may still say "running"
- The Gateway's cleanup loop detects the K8s Job as failed
- The Gateway writes "error" to the status file
- The Gateway routes an error message to the outbox

---

## Logging

The agent uses `kk_core::logging::init()` which sets up structured JSON logging via `tracing-subscriber` with `EnvFilter` (controlled by `RUST_LOG`, defaults to `info`).

All tracing output goes to **stderr**. stdout is reserved for `claude`'s JSONL output (redirected to response.jsonl).

Example log output:

```jsonc
// Phase 0
{"level":"INFO","message":"phase 0: injecting skills","target":"kk_agent::phases"}
{"level":"INFO","count":3,"message":"injected skills into session","target":"kk_agent::phases"}

// Phase 1
{"level":"INFO","message":"phase 1: processing initial prompt","target":"kk_agent::phases"}
{"level":"INFO","message":"loaded SOUL.md","target":"kk_agent::phases"}
{"level":"INFO","group":"family-chat","message":"loaded group CLAUDE.md","target":"kk_agent::phases"}
{"level":"INFO","max_turns":25,"message":"running claude -p","target":"kk_agent::phases"}
{"level":"INFO","message":"phase 1 complete","target":"kk_agent::phases"}

// Phase 2
{"level":"INFO","idle_timeout_secs":120,"message":"phase 2: polling for follow-ups","target":"kk_agent::phases"}
{"level":"INFO","sender":"John","text_preview":"also check my calendar","message":"processing follow-up","target":"kk_agent::phases"}
{"level":"INFO","count":1,"message":"follow-up processed","target":"kk_agent::phases"}
{"level":"INFO","followup_count":1,"message":"phase 2 complete: idle timeout reached","target":"kk_agent::phases"}

// Phase 3
{"level":"INFO","message":"phase 3: writing final status","target":"kk_agent::phases"}
{"level":"INFO","message":"agent job complete","target":"kk_agent::phases"}

// Errors
{"level":"ERROR","exit_code":1,"message":"claude exited with fatal error","target":"kk_agent::phases"}
{"level":"WARN","message":"follow-up has empty text, skipping","target":"kk_agent::phases"}
{"level":"WARN","skill":"broken-skill","message":"skipping skill: SKILL.md not found","target":"kk_agent::phases"}
```

---

## Tests

The agent has 33 tests (15 unit + 18 integration), all passing.

Tests use a **mock claude binary** — a bash script that writes predictable JSONL to stdout and exits with a configurable code. The `CLAUDE_BIN` config field points to the mock.

### Unit tests (`phases.rs`)

| Test | Verifies |
|---|---|
| `build_prompt_both_contexts` | SOUL.md + CLAUDE.md + prompt joined with `---` separators |
| `build_prompt_soul_only` | SOUL.md + prompt, no group context |
| `build_prompt_group_only` | CLAUDE.md + prompt, no soul |
| `build_prompt_no_context` | Prompt only, no separators |
| `build_prompt_empty_contexts_ignored` | Empty strings treated as missing |
| `truncate_short` / `truncate_exact` / `truncate_long` | UTF-8-safe string truncation |
| `extract_session_id_from_result_line` | Extracts Claude session ID from response.jsonl result line |
| `extract_session_id_missing` | No session_id field → None |
| `extract_session_id_empty_file` | Empty file → None |
| `extract_session_id_no_file` | Non-existent file → None |
| `detect_overflow_in_response` | `context_length_exceeded` in response.jsonl → true |
| `detect_overflow_in_log` | `maximum context length` in agent.log → true |
| `no_overflow_normal_response` | Normal response without overflow patterns → false |

### Integration tests (`e2e_agent.rs`)

**Phase 0 — Skills:**

| Test | Verifies |
|---|---|
| `phase0_skills_injected` | Valid skill → symlinked into `.claude/skills/` and `.agents/skills/` (chained) |
| `phase0_missing_skill_md_skipped` | Skill without SKILL.md → not symlinked |
| `phase0_no_skills_dir` | Missing skills dir → succeeds with no skills |
| `phase0_existing_symlink_overwritten` | Pre-existing symlink → removed and re-created |

**Phase 1 — Initial Prompt:**

| Test | Verifies |
|---|---|
| `phase1_success` | Happy path: status=running, response.jsonl has content |
| `phase1_fatal_error` | Exit code 1 → status=error, function returns Err |
| `phase1_nonfatal_error` | Exit code 2 → status stays running, function returns Ok |
| `phase1_context_both` | Prompt contains SOUL.md + CLAUDE.md + user message + separators |
| `phase1_context_soul_only` | Prompt contains SOUL.md + user message |
| `phase1_context_none` | Prompt is just the user message, no separators |
| `phase1_reads_prompt_from_request_json` | Prompt text comes from request.json, not env var |

**Phase 2 — Follow-ups:**

| Test | Verifies |
|---|---|
| `phase2_followup_processed` | Follow-up read, claude resumed, nq file deleted, response appended |
| `phase2_empty_text_skipped` | Whitespace-only text → skipped, file deleted, claude NOT called |
| `phase2_invalid_json_skipped` | Invalid JSON → skipped, file deleted, claude NOT called |
| `phase2_idle_timeout` | No messages → exits after idle timeout |
| `phase2_threaded_queue` | With thread_id=42 → polls thread-specific queue, non-threaded queue unaffected |

**E2E — Full Lifecycle:**

| Test | Verifies |
|---|---|
| `e2e_full_lifecycle` | All 4 phases, skill injected, status transitions running→done |
| `e2e_lifecycle_with_followup` | All 4 phases with follow-up, response has lines from both initial + resume |
