# KubeClaw: Plan — Agent Job

> **Cross-references:**
> [Communication Protocol](protocol.md) — message formats §4.3, §4.5, §4.6; file paths §5; polling intervals §7
> [Plan: Controller](plan-controller.md) ·
> [Plan: Connector](plan-connector.md) ·
> [Plan: Gateway](plan-gateway.md) — creates Agent Jobs (cold path), writes follow-ups (hot path)
> [Plan: Skill](plan-skill.md) — skills live on PVC, symlinked in Phase 0

---

## Summary

The Agent Job is an ephemeral K8s Job (one per invocation). It is created by the Gateway when a message triggers the cold path. The Job runs a single pod that executes an LLM CLI (`claude`, `gemini`, etc.), reads per-group context and skills, polls for follow-up messages, and writes results.

The Agent Job has **no knowledge** of the Gateway, Connectors, Channels, or the K8s API. It only interacts with the shared PVC via well-defined file paths.

### Lifecycle

```
Gateway creates Job → Pod scheduled → Phase 0 (skills) → Phase 1 (prompt) → Phase 2 (follow-ups) → Phase 3 (cleanup) → Pod exits
```

| Phase | Duration | What happens |
|---|---|---|
| **Phase 0** | ~0.5s | Symlink skills into session directory |
| **Phase 1** | 3–60s | Build context, run LLM with initial prompt, write streaming response |
| **Phase 2** | 0–120s | Poll per-group queue for follow-ups, resume LLM for each |
| **Phase 3** | ~0.1s | Write final status, exit |

---

## Image

```
kubeclaw-agent:latest

Base:    debian:bookworm-slim (Claude Code needs glibc)
Install:
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

Set by the Gateway when creating the Job (see [Gateway Plan](plan-gateway.md) — Job Spec Builder):

| Var | Example | Description |
|---|---|---|
| `PROMPT` | `what's the weather?` | Initial prompt text (trigger pattern already stripped) |
| `GROUP` | `family-chat` | Group name — determines session dir and follow-up queue |
| `SESSION_ID` | `family-chat-1708801290` | Unique session ID — determines results dir |
| `IDLE_TIMEOUT` | `120` | Seconds to wait for follow-ups before exiting Phase 2 |
| `MAX_TURNS` | `25` | Max agentic turns for `claude -p` |
| `DATA_DIR` | `/data` | PVC mount path |

From Secret (`kubeclaw-api-keys` via `envFrom`):

| Var | Description |
|---|---|
| `ANTHROPIC_API_KEY` | API key for Claude |
| `GEMINI_API_KEY` | API key for Gemini (optional) |
| `OPENAI_API_KEY` | API key for OpenAI (optional) |

---

## Directory Paths

All paths are relative to `DATA_DIR` (`/data`). The Agent Job only touches these directories:

| Path | Access | Purpose |
|---|---|---|
| `/data/skills/` | **Read** | All installed skills (symlinked into session in Phase 0) |
| `/data/sessions/{GROUP}/` | **Read + Write** | Claude Code working directory, session state, skill symlinks |
| `/data/memory/SOUL.md` | **Read** | Global agent personality |
| `/data/memory/{GROUP}/CLAUDE.md` | **Read** | Per-group context |
| `/data/results/{SESSION_ID}/` | **Write** | Status file, response.jsonl, agent.log |
| `/data/groups/{GROUP}/` | **Read** (delete after read) | Per-group follow-up queue (hot path messages from Gateway) |

The Agent Job **never** reads or writes:
- `/data/inbound/` (Gateway's domain)
- `/data/outbox/` (Gateway + Connector's domain)
- `/data/state/` (Gateway's domain)

---

## Entrypoint Script (Full)

```bash
#!/bin/bash
set -euo pipefail

# ══════════════════════════════════════════════
# Environment
# ══════════════════════════════════════════════
GROUP="${GROUP:?GROUP env var required}"
SESSION_ID="${SESSION_ID:?SESSION_ID env var required}"
PROMPT="${PROMPT:?PROMPT env var required}"
DATA_DIR="${DATA_DIR:-/data}"
IDLE_TIMEOUT="${IDLE_TIMEOUT:-120}"
MAX_TURNS="${MAX_TURNS:-25}"

SKILLS_DIR="$DATA_DIR/skills"
SESSION_DIR="$DATA_DIR/sessions/$GROUP"
RESULTS_DIR="$DATA_DIR/results/$SESSION_ID"
GROUP_QUEUE="$DATA_DIR/groups/$GROUP"
MEMORY_DIR="$DATA_DIR/memory"

# Logging helper
log() {
  echo "{\"level\":\"info\",\"msg\":\"$1\",\"group\":\"$GROUP\",\"session\":\"$SESSION_ID\",\"ts\":$(date +%s)}" >&2
}

log_error() {
  echo "{\"level\":\"error\",\"msg\":\"$1\",\"group\":\"$GROUP\",\"session\":\"$SESSION_ID\",\"ts\":$(date +%s)}" >&2
}

# Ensure directories exist
mkdir -p "$SESSION_DIR"
mkdir -p "$RESULTS_DIR"
mkdir -p "$GROUP_QUEUE"

# ══════════════════════════════════════════════
# PHASE 0: Inject Skills
# ══════════════════════════════════════════════
log "Phase 0: Injecting skills"

SKILL_COUNT=0

if [ -d "$SKILLS_DIR" ] && [ "$(ls -A "$SKILLS_DIR" 2>/dev/null)" ]; then
  # Create skill directories for agent discovery
  mkdir -p "$SESSION_DIR/.claude/skills"
  mkdir -p "$SESSION_DIR/.agents/skills"

  for skill_dir in "$SKILLS_DIR"/*/; do
    [ -d "$skill_dir" ] || continue
    skill_name=$(basename "$skill_dir")

    # Validate SKILL.md exists (skip broken skills)
    if [ ! -f "$skill_dir/SKILL.md" ]; then
      log_error "Skipping skill '$skill_name': SKILL.md not found"
      continue
    fi

    # Symlink into .claude/skills/ (Claude Code native discovery)
    ln -sfn "$skill_dir" "$SESSION_DIR/.claude/skills/$skill_name"

    # Symlink into .agents/skills/ (generic agent discovery)
    ln -sfn "$SESSION_DIR/.claude/skills/$skill_name" "$SESSION_DIR/.agents/skills/$skill_name"

    SKILL_COUNT=$((SKILL_COUNT + 1))
  done

  log "Injected $SKILL_COUNT skills into session"
else
  log "No skills found in $SKILLS_DIR"
fi

# ══════════════════════════════════════════════
# PHASE 1: Process Initial Prompt
# ══════════════════════════════════════════════
log "Phase 1: Processing initial prompt"

# Mark as running (Protocol §4.5)
echo "running" > "$RESULTS_DIR/status"

# Build context from memory files
CONTEXT=""

# Global personality
if [ -f "$MEMORY_DIR/SOUL.md" ]; then
  SOUL_CONTENT=$(cat "$MEMORY_DIR/SOUL.md")
  CONTEXT="$SOUL_CONTENT"
  log "Loaded SOUL.md ($(wc -c < "$MEMORY_DIR/SOUL.md") bytes)"
fi

# Per-group context
if [ -f "$MEMORY_DIR/$GROUP/CLAUDE.md" ]; then
  GROUP_CONTEXT=$(cat "$MEMORY_DIR/$GROUP/CLAUDE.md")
  if [ -n "$CONTEXT" ]; then
    CONTEXT="$CONTEXT

---

$GROUP_CONTEXT"
  else
    CONTEXT="$GROUP_CONTEXT"
  fi
  log "Loaded $GROUP/CLAUDE.md ($(wc -c < "$MEMORY_DIR/$GROUP/CLAUDE.md") bytes)"
fi

# Build the full prompt
if [ -n "$CONTEXT" ]; then
  FULL_PROMPT="$CONTEXT

---

$PROMPT"
else
  FULL_PROMPT="$PROMPT"
fi

# Change to session directory (Claude Code uses cwd for .claude/ discovery)
cd "$SESSION_DIR"

# Execute LLM
# Redirect structured output to response.jsonl, stderr to agent.log
log "Running claude -p (max-turns=$MAX_TURNS)"

claude -p "$FULL_PROMPT" \
  --output-format stream-json \
  --max-turns "$MAX_TURNS" \
  --verbose \
  > "$RESULTS_DIR/response.jsonl" \
  2> "$RESULTS_DIR/agent.log" \
  || {
    EXIT_CODE=$?
    log_error "claude exited with code $EXIT_CODE in Phase 1"
    # Don't exit yet — still try Phase 2 follow-ups if the error was non-fatal
    # But if exit code indicates a fatal error (e.g., auth failure), bail
    if [ $EXIT_CODE -eq 1 ]; then
      echo "error" > "$RESULTS_DIR/status"
      exit 1
    fi
  }

log "Phase 1 complete"

# ══════════════════════════════════════════════
# PHASE 2: Poll for Follow-ups
# ══════════════════════════════════════════════
log "Phase 2: Polling for follow-ups (idle_timeout=${IDLE_TIMEOUT}s)"

IDLE_ELAPSED=0
POLL_INTERVAL=2  # seconds (Protocol §7)
FOLLOWUP_COUNT=0

while [ "$IDLE_ELAPSED" -lt "$IDLE_TIMEOUT" ]; do

  # List pending nq files in per-group queue
  # Sort by name (= timestamp order = FIFO)
  PENDING_FILES=$(find "$GROUP_QUEUE" -maxdepth 1 -name ',*.nq' -type f 2>/dev/null | sort)

  if [ -n "$PENDING_FILES" ]; then
    # Process all pending follow-ups
    echo "$PENDING_FILES" | while IFS= read -r nq_file; do
      [ -f "$nq_file" ] || continue

      # Read follow-up message (Protocol §4.3)
      FOLLOWUP_JSON=$(cat "$nq_file")

      # Extract text field from JSON
      FOLLOWUP_TEXT=$(echo "$FOLLOWUP_JSON" | jq -r '.text // empty')
      FOLLOWUP_SENDER=$(echo "$FOLLOWUP_JSON" | jq -r '.sender // "someone"')

      if [ -z "$FOLLOWUP_TEXT" ]; then
        log_error "Follow-up message has empty text, skipping"
        rm -f "$nq_file"
        continue
      fi

      # Build follow-up prompt with sender context
      FOLLOWUP_PROMPT="[Follow-up from $FOLLOWUP_SENDER]: $FOLLOWUP_TEXT"

      log "Processing follow-up from $FOLLOWUP_SENDER: $(echo "$FOLLOWUP_TEXT" | head -c 80)..."

      # Resume claude session with follow-up
      claude -p "$FOLLOWUP_PROMPT" \
        --resume \
        --output-format stream-json \
        --max-turns "$MAX_TURNS" \
        --verbose \
        >> "$RESULTS_DIR/response.jsonl" \
        2>> "$RESULTS_DIR/agent.log" \
        || {
          EXIT_CODE=$?
          log_error "claude --resume exited with code $EXIT_CODE"
          # Non-fatal: continue polling for more follow-ups
        }

      # Delete processed file
      rm -f "$nq_file"

      FOLLOWUP_COUNT=$((FOLLOWUP_COUNT + 1))
      log "Follow-up processed (#$FOLLOWUP_COUNT)"
    done

    # Reset idle timer — just got new messages
    IDLE_ELAPSED=0
  else
    # No messages — increment idle timer
    sleep "$POLL_INTERVAL"
    IDLE_ELAPSED=$((IDLE_ELAPSED + POLL_INTERVAL))
  fi

done

log "Phase 2 complete: idle timeout reached after ${IDLE_ELAPSED}s, processed $FOLLOWUP_COUNT follow-ups"

# ══════════════════════════════════════════════
# PHASE 3: Cleanup
# ══════════════════════════════════════════════
log "Phase 3: Cleanup"

# Write final status (Protocol §4.5)
echo "done" > "$RESULTS_DIR/status"

log "Agent Job complete"
exit 0
```

---

## Phase 0: Skill Injection (Detail)

### What happens

The Agent symlinks every skill directory from `/data/skills/{name}/` into the session's `.claude/skills/` and `.agents/skills/` directories. This makes skills discoverable by the agent's native skill-loading mechanism.

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
| No skills installed | Phase 0 completes immediately, log "No skills found" |
| Skill dir exists but missing SKILL.md | Skip with warning log (broken skill) |
| Symlink already exists from previous Job | `ln -sfn` overwrites — safe |
| Skill deleted from PVC after symlink created | Dangling symlink — agent ignores gracefully |
| Hundreds of skills | Symlink loop still < 1s. Not a practical concern. |

---

## Phase 1: Initial Prompt (Detail)

### Context Assembly

The full prompt sent to the LLM is built from three pieces:

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
│ {PROMPT} (user's message, trigger stripped) │
│                                             │
│ what's the weather?                         │
└─────────────────────────────────────────────┘
```

### Missing context files

| File | Missing? | Behavior |
|---|---|---|
| `SOUL.md` | OK | No global personality. Prompt starts with group context or raw prompt. |
| `{GROUP}/CLAUDE.md` | OK | No per-group context. Prompt starts with raw prompt. |
| Both missing | OK | LLM receives only the user's prompt text. |

### LLM Execution

```
claude -p "$FULL_PROMPT" \
  --output-format stream-json \
  --max-turns 25 \
  --verbose
```

| Flag | Purpose |
|---|---|
| `-p` | Non-interactive prompt mode (pipe in, get output, exit) |
| `--output-format stream-json` | Output as JSONL stream (Protocol §4.6) |
| `--max-turns 25` | Limit agentic tool-use loops |
| `--verbose` | Include tool calls and reasoning in stderr (→ agent.log) |

### Working Directory

The entrypoint `cd`s to `$SESSION_DIR` before running `claude`. This is critical because:
- Claude Code looks for `.claude/` in the current working directory
- Skill symlinks in `.claude/skills/` are discovered relative to cwd
- Claude Code session state (`.claude/` internal files) persists across invocations for the same group

### Output Streams

| Stream | Destination | Format |
|---|---|---|
| stdout | `$RESULTS_DIR/response.jsonl` | JSONL — structured response stream (Protocol §4.6) |
| stderr | `$RESULTS_DIR/agent.log` | Plain text — tool calls, verbose reasoning, errors |

### Exit Code Handling

| Exit Code | Meaning | Action |
|---|---|---|
| 0 | Success | Continue to Phase 2 |
| 1 | Fatal error (auth failure, invalid config) | Write status=error, exit 1 |
| 2+ | Non-fatal error (tool failure, rate limit) | Log warning, continue to Phase 2 |

---

## Phase 2: Follow-up Polling (Detail)

### How it works

After the initial prompt completes, the Agent enters a polling loop on the per-group queue directory. The Gateway writes follow-up messages here when it detects a running Job for the group (hot path).

```
Gateway (hot path)                    Agent Job (Phase 2)
    │                                       │
    │  write ,{ts}.{id}.nq to               │
    │  /data/groups/family-chat/             │
    │ ─────────────────────────────────────► │
    │                                       │  find + read nq file
    │                                       │  parse JSON (Protocol §4.3)
    │                                       │  extract .text field
    │                                       │  run: claude -p "{text}" --resume
    │                                       │  append to response.jsonl
    │                                       │  delete nq file
    │                                       │
    │                                       │  sleep 2s, poll again
    │                                       │  ...
    │                                       │  idle for IDLE_TIMEOUT → exit loop
```

### The `--resume` Flag

`claude -p "follow-up text" --resume` tells Claude Code to:
1. Load the existing session state from `.claude/` in cwd
2. Continue the conversation with the new prompt appended
3. Maintain tool state, memory, and context from Phase 1

This is why `cd "$SESSION_DIR"` is critical — the session state file must be in the working directory.

### Follow-up Message Format

The Agent reads follow-up messages from the per-group queue (Protocol §4.3):

```jsonc
{
  "sender": "John",
  "text": "also check my calendar",
  "timestamp": 1708801300,
  "channel": "telegram-bot-1",
  "platform_meta": { ... }
}
```

The Agent only uses `sender` and `text`. The rest is ignored (routing info for the Gateway).

The follow-up is wrapped with sender context before being sent to the LLM:

```
[Follow-up from John]: also check my calendar
```

This helps the LLM understand that a new message arrived from a specific person, especially in multi-user group chats.

### Idle Timeout Behavior

```
IDLE_TIMEOUT = 120 seconds (default, configurable via env)
POLL_INTERVAL = 2 seconds

Timeline:
  T+0s    Phase 2 starts
  T+2s    Poll: no messages → idle_elapsed = 2
  T+4s    Poll: no messages → idle_elapsed = 4
  ...
  T+10s   Poll: message found! → process → idle_elapsed = 0 (RESET)
  T+12s   Poll: no messages → idle_elapsed = 2
  ...
  T+130s  idle_elapsed = 120 → exit Phase 2
```

The idle timer **resets to 0** every time a message is found. This means:
- If users keep sending follow-ups, the Job stays alive indefinitely (up to `JOB_ACTIVE_DEADLINE` — 300s)
- If no follow-ups arrive for 120s, the Job exits gracefully

### Multiple Messages in One Poll

If multiple follow-up messages are pending when the Agent polls, they are all processed in FIFO order (sorted by filename = timestamp). The idle timer resets after processing all of them.

### Follow-up Processing Errors

| Error | Handling |
|---|---|
| Empty text field | Skip message, delete file, log warning |
| JSON parse error | Skip message, delete file, log error |
| `claude --resume` fails | Log error, continue polling (non-fatal) |
| `claude --resume` exits with code 1 | Log error, continue polling (session may be recoverable) |

---

## Phase 3: Cleanup (Detail)

### What happens

```
1. Write "done" to $RESULTS_DIR/status    (Protocol §4.5)
2. Log completion
3. Exit 0
```

### Status File

The status file is a plain text file with a single word (Protocol §4.5):

```bash
echo "done" > "$RESULTS_DIR/status"
```

The Gateway's results loop polls this file. On seeing "done", it reads `response.jsonl` and `request.json` to route the response.

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
│                                "running" → "done" (or "error")
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

### What does NOT persist across Jobs

| What | Why |
|---|---|
| Claude conversation history | Each Job starts a new `claude -p` session. `--resume` only works within the same Job's Phase 2. |
| In-memory state | Pod is gone. No process state carries over. |

If cross-Job conversation memory is needed in the future, it would go in `/data/memory/{group}/CLAUDE.md` (human-curated) or a future conversation log mechanism.

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

# Entrypoint
COPY entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh

WORKDIR /data
ENTRYPOINT ["/app/entrypoint.sh"]
```

---

## Error Handling

### Fatal Errors (Agent exits, status=error)

| Error | Detection | Result |
|---|---|---|
| `ANTHROPIC_API_KEY` missing/invalid | `claude` exits 1 immediately | status=error, Gateway routes error message |
| `claude` binary not found | bash `command not found` | status=error (set -e catches it) |
| PVC not mounted / permissions | mkdir/write fails | status=error (set -e catches it) |

### Non-Fatal Errors (Agent continues)

| Error | Detection | Result |
|---|---|---|
| `claude -p` exits non-zero (rate limit, tool error) | Exit code check | Log warning, continue to Phase 2 |
| `claude --resume` fails on a follow-up | Exit code check | Log warning, continue polling |
| Follow-up message has empty text | jq check | Skip message, delete file |
| Follow-up message is invalid JSON | jq parse error | Skip message, delete file |
| SOUL.md missing | File check | No global context — OK |
| GROUP/CLAUDE.md missing | File check | No group context — OK |
| Skills dir empty | Directory check | No skills — OK |

### Timeout

If the Job hits `activeDeadlineSeconds` (300s), K8s kills the pod. In this case:
- The status file may still say "running"
- The Gateway's cleanup loop detects the K8s Job as failed
- The Gateway writes "error" to the status file
- The Gateway routes an error message to the outbox

---

## Logging

All logging goes to **stderr** (structured JSON). stdout is reserved for `claude`'s JSONL output.

```jsonc
// Phase 0
{"level":"info","msg":"Phase 0: Injecting skills","group":"family-chat","session":"family-chat-1708801290","ts":1708801292}
{"level":"info","msg":"Injected 3 skills into session","group":"family-chat","session":"family-chat-1708801290","ts":1708801292}

// Phase 1
{"level":"info","msg":"Phase 1: Processing initial prompt","group":"family-chat","session":"family-chat-1708801290","ts":1708801293}
{"level":"info","msg":"Loaded SOUL.md (2048 bytes)","group":"family-chat","session":"family-chat-1708801290","ts":1708801293}
{"level":"info","msg":"Loaded family-chat/CLAUDE.md (512 bytes)","group":"family-chat","session":"family-chat-1708801290","ts":1708801293}
{"level":"info","msg":"Running claude -p (max-turns=25)","group":"family-chat","session":"family-chat-1708801290","ts":1708801293}
{"level":"info","msg":"Phase 1 complete","group":"family-chat","session":"family-chat-1708801290","ts":1708801298}

// Phase 2
{"level":"info","msg":"Phase 2: Polling for follow-ups (idle_timeout=120s)","group":"family-chat","session":"family-chat-1708801290","ts":1708801298}
{"level":"info","msg":"Processing follow-up from John: also check my calendar...","group":"family-chat","session":"family-chat-1708801290","ts":1708801310}
{"level":"info","msg":"Follow-up processed (#1)","group":"family-chat","session":"family-chat-1708801290","ts":1708801315}
{"level":"info","msg":"Phase 2 complete: idle timeout reached after 120s, processed 1 follow-ups","group":"family-chat","session":"family-chat-1708801290","ts":1708801435}

// Phase 3
{"level":"info","msg":"Phase 3: Cleanup","group":"family-chat","session":"family-chat-1708801290","ts":1708801435}
{"level":"info","msg":"Agent Job complete","group":"family-chat","session":"family-chat-1708801290","ts":1708801435}

// Errors
{"level":"error","msg":"claude exited with code 1 in Phase 1","group":"family-chat","session":"family-chat-1708801290","ts":1708801295}
{"level":"error","msg":"Follow-up message has empty text, skipping","group":"family-chat","session":"family-chat-1708801290","ts":1708801312}
{"level":"error","msg":"Skipping skill 'broken-skill': SKILL.md not found","group":"family-chat","session":"family-chat-1708801290","ts":1708801292}
```

---

## Testing Checklist

### Phase 0 — Skills
- [ ] Skills in /data/skills/ → symlinked into .claude/skills/ and .agents/skills/
- [ ] No skills → Phase 0 completes with "No skills found" log
- [ ] Skill dir missing SKILL.md → skipped with warning
- [ ] Existing symlinks from previous Job → overwritten cleanly
- [ ] Claude Code discovers skills (verify with `claude --list-skills` or similar)

### Phase 1 — Initial Prompt
- [ ] SOUL.md + CLAUDE.md assembled into prompt with separators
- [ ] SOUL.md missing → prompt has no global context (OK)
- [ ] CLAUDE.md missing → prompt has no group context (OK)
- [ ] Both missing → prompt is raw user text only
- [ ] status=running written before claude starts
- [ ] response.jsonl contains valid JSONL with "result" type line
- [ ] agent.log captures tool calls and reasoning
- [ ] claude exit code 0 → continues to Phase 2
- [ ] claude exit code 1 → status=error, Job exits
- [ ] cwd is session dir (skills discoverable)

### Phase 2 — Follow-ups
- [ ] Follow-up nq file → read, parsed, text extracted
- [ ] claude --resume invoked with follow-up text
- [ ] Response appended (>>) to same response.jsonl
- [ ] nq file deleted after processing
- [ ] Idle timer resets when message found
- [ ] Idle timeout reached → exits Phase 2
- [ ] Multiple messages in one poll → all processed FIFO
- [ ] Empty text field → skipped
- [ ] Invalid JSON → skipped
- [ ] claude --resume fails → logged, polling continues
- [ ] Sender name included in follow-up prompt

### Phase 3 — Cleanup
- [ ] status=done written
- [ ] Exit code 0
- [ ] Session dir preserved (not deleted)
- [ ] Results dir preserved (Gateway cleans up later)

### Integration
- [ ] Gateway creates Job → Pod starts within 5s (cached image)
- [ ] Gateway hot-path message → Agent picks up within 2s
- [ ] Gateway reads response.jsonl after status=done → routes to outbox
- [ ] activeDeadlineSeconds exceeded → K8s kills pod → Gateway detects failure
