# KubeClaw: Communication Protocol

> **Cross-references:**
> [Plan: Controller](plan-controller.md) ·
> [Plan: Connector](plan-connector.md) ·
> [Plan: Gateway](plan-gateway.md) ·
> [Plan: Agent Job](plan-agent-job.md) ·
> [Plan: Skill](plan-skill.md)

This document is the single source of truth for how all KubeClaw components communicate. Every file path, message format, polling interval, and nq convention is defined here. Component plans reference this document — they do not redefine protocol details.

---

## 1. Communication Medium

All inter-component communication happens via **files on a single shared RWX PVC** (`kubeclaw-data`), mounted at `/data` in every pod.

There are **no network calls** between components (no HTTP, no gRPC, no TCP sockets). The only network calls are:
- Connectors ↔ external providers (Telegram API, WhatsApp WebSocket, etc.)
- Controller → GitHub (git clone for Skills)
- Controller/Gateway → K8s API (managing CRs, Deployments, Jobs)
- Agent → LLM API (Anthropic, Google, etc.)

---

## 1.1 Key Concepts

**Group** — A group represents a single conversation context, mapped 1:1 to a provider chat (e.g. a Telegram group chat or DM). It is the unit of isolation in kk: each group gets its own agent job, its own follow-up queue (`/data/groups/{group}/`), and its own persistent session state (`/data/sessions/{group}/`). Messages from different groups never mix. A group is identified by its **slug** — a lowercase, hyphen-separated string matching `[a-z0-9-]+` (e.g. `family-chat`, `work-team`). The slug is mapped from the provider's numeric chat ID via `groups.json`. Messages from unmapped chat IDs are dropped with a warning. Throughout code and docs, the field `group` always holds this slug, never a display name or provider ID.

**Thread** — A thread represents a sub-conversation within a group (e.g. a Telegram Forum Topic, Slack thread, or Discord thread). The routing key is `(group, thread_id)` — each pair gets its own agent job, follow-up queue, and session state. `thread_id` is `Option<String>` (`None` for non-threaded chats, string for threaded — Telegram uses integers-as-strings like `"42"`, Slack uses timestamps like `"1708801285.000050"`). Trigger config is per-group; all threads in a group share the same trigger pattern.

**Channel** — A channel represents a single bot connection to a messaging provider (e.g. one Telegram bot). Each channel has its own Connector Deployment and its own outbox queue (`/data/outbox/{channel}/`). A group can span multiple channels (e.g. the same conversation bridged across Telegram and Slack), though typically it's one channel per group.

---

## 2. nq Queue Convention

KubeClaw uses the [nq](https://github.com/leahneukirchen/nq) file-based job queue pattern. Each queue is a directory. Each message is a file in that directory.

### File Naming

```
,{timestamp_seconds}.{unique_id}.nq
```

| Part | Format | Example |
|---|---|---|
| Prefix | `,` (comma) | Marks file as pending in nq convention |
| Timestamp | Unix epoch seconds, decimal | `1708801290.12345` |
| Unique ID | Random or UUID fragment | `a1b2c3` |
| Extension | `.nq` | Identifies as queue message |

Full example: `,1708801290.12345.a1b2c3.nq`

### Atomic Write Pattern

All queue writes MUST be atomic to prevent readers from seeing partial files:

```
1. Write to temp file:   {queue_dir}/.tmp-{uuid}
2. Atomic rename:        rename(.tmp-{uuid} → ,{ts}.{id}.nq)
```

Readers only see files matching the `,*.nq` glob — the `.tmp-*` prefix ensures in-progress writes are invisible.

### Processing Pattern

```
1. List files matching  ,*.nq  in queue dir
2. Sort by filename (oldest timestamp first) → FIFO ordering
3. For each file:
   a. Read contents (JSON)
   b. Process
   c. Delete file (unlink) on success
   d. On failure: leave file in place (will be retried next poll)
```

### Done Marker

No separate done marker. Successful processing = file deleted. If a file persists, it hasn't been processed yet.

---

## 3. Queue Directories & Ownership

```
/data/
├── inbound/                    WRITE: Connectors    READ: Gateway
├── groups/{group}/        WRITE: Gateway       READ: Agent Jobs
│   └── threads/{thread-id}/    WRITE: Gateway       READ: Agent Jobs (threaded)
├── outbox/{channel-name}/      WRITE: Gateway       READ: Connectors
├── results/{session-id}/       WRITE: Agent Jobs    READ: Gateway
├── skills/{skill-name}/        WRITE: Controller    READ: Agent Jobs
├── sessions/{group}/      R+W:   Agent Jobs    (claude state)
│   └── threads/{thread-id}/    R+W:   Agent Jobs    (threaded sessions)
├── memory/                     R+W:   Human/Admin   READ: Agent Jobs
└── state/                      R+W:   Gateway
```

### Ownership Rules

| Directory | Writer(s) | Reader(s) | Cleanup by |
|---|---|---|---|
| `/data/inbound/` | Connectors | Gateway | Gateway (deletes after processing) |
| `/data/groups/{group}/` | Gateway | Agent Job for that group | Agent Job (deletes after processing) |
| `/data/outbox/{channel}/` | Gateway | Connector for that channel | Connector (deletes after sending) |
| `/data/results/{session}/` | Agent Job | Gateway | Gateway (moves to `.done/` after reading) |
| `/data/skills/{skill}/` | Controller | Agent Jobs (via symlink) | Controller (on Skill CR delete) |
| `/data/sessions/{group}/` | Agent Jobs | Agent Jobs | Never auto-cleaned (persistent state) |
| `/data/memory/` | Human (kubectl/editor) | Agent Jobs | Never auto-cleaned |
| `/data/state/` | Gateway | Gateway | Gateway (overwrites in place) |

---

## 4. Message Formats

### 4.1 Inbound Message

**Written by:** Connector → `/data/inbound/`
**Read by:** Gateway

```jsonc
{
  // Identity
  "channel": "telegram-bot-1",          // string — Channel CR metadata.name
  "channel_type": "telegram",           // string — Channel CR spec.type
  "group": "family-chat",               // string — group slug (see §1.1, §6)
  "thread_id": "42",                    // string? — thread/topic ID (omitted if not threaded)
  "sender": "John",                     // string — human-readable sender name

  // Content
  "text": "@Andy what's the weather?",  // string — message body (plain text)

  // Timing
  "timestamp": 1708801290,              // int — Unix epoch seconds (from provider)

  // Routing (opaque to Gateway — passed through to outbox for response)
  "meta": {
    // Telegram
    "chat_id": "-1001234567890",        // string — always stringified
    "message_id": 42,                   // int — for reply threading

    // WhatsApp (alternative)
    // "jid": "1234567890@g.us",
    // "message_id": "ABCDEF123",

    // Slack (alternative)
    // "channel_id": "C0123456789",
    // "ts": "1708801290.000100",
    // "thread_ts": "1708801285.000050"
  }
}
```

**Validation rules (Gateway enforces on read):**
- `channel` — required, non-empty string
- `channel_type` — required, one of: `telegram`, `whatsapp`, `slack`, `discord`, `signal`
- `group` — required, non-empty string, group slug (`[a-z0-9-]+`, see §1.1)
- `text` — required, non-empty string
- `timestamp` — required, positive integer
- `meta` — required, object (contents are opaque)

### 4.2 Outbound Message

**Written by:** Gateway → `/data/outbox/{channel-name}/`
**Read by:** Connector for that channel

```jsonc
{
  "channel": "telegram-bot-1",          // string — which connector picks this up
  "group": "family-chat",               // string — for logging/debugging
  "thread_id": "42",                    // string? — thread/topic ID (omitted if not threaded)
  "text": "The weather is 22°C.",       // string — response text to send
  "meta": {                    // object — passed through from inbound
    "chat_id": "-1001234567890",
    "reply_to_message_id": 42           // optional — for reply threading
  }
}
```

**Validation rules (Connector enforces on read):**
- `channel` — must match own `CHANNEL_NAME` env var
- `text` — required, non-empty string
- `meta` — required, must contain provider-specific routing fields

### 4.3 Group Follow-up Message

**Written by:** Gateway → `/data/groups/{group}/`
**Read by:** Running Agent Job for that group

```jsonc
{
  "sender": "John",                     // string — who sent the follow-up
  "text": "also check my calendar",     // string — follow-up message body
  "timestamp": 1708801300,              // int — Unix epoch seconds
  "channel": "telegram-bot-1",          // string — for context
  "thread_id": "42",                    // string? — thread/topic ID (omitted if not threaded)
  "meta": {                    // object — for response routing
    "chat_id": "-1001234567890",
    "message_id": 43
  }
}
```

Same format as inbound but used for hot-path delivery to an already-running Job.

### 4.4 Request Manifest

**Written by:** Gateway → `/data/results/{session-id}/request.json`
**Read by:** Gateway (results loop, for response routing)

```jsonc
{
  "channel": "telegram-bot-1",          // string — where to send response
  "group": "family-chat",               // string — which group
  "thread_id": "42",                    // string? — thread/topic ID (omitted if not threaded)
  "sender": "John",                     // string — who triggered the Job
  "meta": {                    // object — response routing info
    "chat_id": "-1001234567890",
    "message_id": 42
  },
  "messages": [                         // array — all messages that triggered this Job
    {
      "sender": "John",
      "text": "what's the weather?",
      "ts": 1708801290
    }
  ]
}
```

### 4.5 Result Status File

**Written by:** Agent Job → `/data/results/{session-id}/status`
**Read by:** Gateway (results loop)

Plain text file, single line, one of:

```
running
done
error
```

No JSON. Just the string. Gateway reads with `cat` / `readFile` and trims whitespace.

### 4.6 Result Response File

**Written by:** Agent Job → `/data/results/{session-id}/response.jsonl`
**Read by:** Gateway (results loop, after status = "done")

JSONL format (one JSON object per line). This is the raw output from `claude --output-format stream-json`. The Gateway extracts the final assistant text from this stream.

```jsonc
// Each line is a complete JSON object (stream-json format from Claude Code)
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The weather is"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"The weather is 22°C and sunny!"}]}}
{"type":"result","result":"The weather is 22°C and sunny!\nYour calendar shows 2 meetings today."}
```

**Gateway extracts the final response:**
1. Read all lines
2. Find the last line with `"type":"result"`
3. Extract `result` field as the response text

---

## 5. File Path Conventions

### 5.1 Session ID Format

Non-threaded:
```
{group}-{unix-timestamp}
```
Examples: `family-chat-1708801290`, `work-team-1708801305`

Threaded (when `thread_id` is present):
```
{group}-t{thread-id}-{unix-timestamp}
```
Examples: `dev-team-t42-1708801290`, `eng-t1708801285.000050-1708801290`

Used in:
- Job name: `agent-{session-id}` → `agent-family-chat-1708801290`
- Results dir: `/data/results/{session-id}/`
- Env var `SESSION_ID` passed to Agent Job

### 5.2 Skill Directory Name

Uses the Skill CR `metadata.name` — NOT the `name` from SKILL.md frontmatter.

```
/data/skills/{skill-cr-metadata-name}/
```

Example: CR `metadata.name: frontend-design` → `/data/skills/frontend-design/`

This avoids collisions if two CRs reference skills with the same frontmatter name from different repos.

### 5.3 Session Directory Structure

```
/data/sessions/{group}/
├── .claude/                    ← Claude Code working directory
│   ├── skills/                 ← symlinks to /data/skills/* (created by Agent Phase 0)
│   │   ├── frontend-design → /data/skills/frontend-design
│   │   ├── vercel-deploy → /data/skills/vercel-deploy
│   │   └── agent-browser → /data/skills/agent-browser
│   └── ...                     ← other Claude Code state files
├── .agents/                    ← generic agent skills directory
│   └── skills/                 ← same symlinks
│       ├── frontend-design → /data/skills/frontend-design
│       └── ...
└── ...                         ← any other files claude creates during sessions
```

### 5.4 Memory Directory Structure

```
/data/memory/
├── SOUL.md                     ← global personality (read by every Agent Job)
├── {group}/
│   └── CLAUDE.md               ← per-group context
└── ...
```

### 5.5 Gateway State Files

```
/data/state/
├── groups.json                 ← group registry, admin-managed (see §6)
├── groups.d/                   ← auto-registered groups (see §6)
│   ├── telegram-bot-1.json     ← per-channel auto-registrations
│   └── ...
└── cursors.json                ← processing cursors (see §7)
```

---

## 6. Group Registry

**File:** `/data/state/groups.json`
**Owner:** Gateway (read + write)
**Also read by:** Connectors (for chat_id → group slug mapping, read-only)

```jsonc
{
  "groups": {
    "family-chat": {
      "trigger_pattern": "@Andy",       // regex or simple string match
      "trigger_mode": "mention",        // "mention" | "always" | "prefix"
      "channels": {                     // which channels map to this group
        "telegram-bot-1": {
          "chat_id": "-1001234567890"   // provider-specific group ID
        }
      }
    },
    "work-team": {
      "trigger_pattern": "@Andy",
      "trigger_mode": "mention",
      "channels": {
        "slack-eng": {
          "chat_id": "C0123456789"
        }
      }
    },
    "main": {
      "trigger_pattern": null,          // null = always trigger
      "trigger_mode": "always",
      "channels": {
        "whatsapp-main": {
          "chat_id": "1234567890@s.whatsapp.net"
        }
      }
    }
  }
}
```

### Auto-Registration via groups.d/

Connectors auto-register unknown chats by writing to `/data/state/groups.d/{channel-name}.json`. Each Connector instance writes only its own file, avoiding concurrent writer conflicts.

**Auto-generated slug format:** `tg-{abs(chat_id)}` — strips leading `-` from Telegram supergroup IDs. E.g. `-1001234567890` → `tg-1001234567890`. Default trigger mode: `mention`.

**How Connectors use it:** On startup, the Connector loads its `groups.d/{channel}.json` to build a reverse lookup map: `chat_id → group slug`. When a message arrives from an unmapped chat_id, the Connector auto-registers it (writes to `groups.d/`) and processes the message. The config is reloaded every 30s.

**How Gateway uses it:** The Gateway merges all `groups.d/*.json` files with its own `groups.json` to discover both auto-registered and admin-configured groups. See the Gateway plan for merge details.

**How Gateway uses it:** When processing an inbound message, the Gateway reads the `trigger_pattern` and `trigger_mode` for the message's `group` to decide whether to act on it.

### Trigger Modes

| Mode | Behavior | Example |
|---|---|---|
| `always` | Every message triggers an Agent Job | Main/private chats |
| `mention` | Only messages matching `trigger_pattern` as a substring | `@Andy` in group chats |
| `prefix` | Only messages starting with `trigger_pattern` | `/ask` commands |

---

## 7. Polling Intervals & Timing

| Component | What it polls | Directory / Resource | Interval | Notes |
|---|---|---|---|---|
| **Gateway** — inbound loop | Inbound queue | `/data/inbound/` | **2 seconds** | Main routing loop |
| **Gateway** — results loop | Result status files | `/data/results/*/status` | **2 seconds** | Checks for completed Jobs |
| **Gateway** — cleanup loop | Completed K8s Jobs | K8s API (batch/v1 Jobs) | **60 seconds** | Deletes Jobs older than 5min |
| **Connector** — outbound loop | Outbound queue | `/data/outbox/{channel}/` | **1 second** | Faster for responsiveness |
| **Agent Job** — follow-up loop | Per-group queue | `/data/groups/{group}/` | **2 seconds** | Phase 2 hot-path polling |
| **Agent Job** — idle timeout | No new messages | Per-group queue | **120 seconds** (default) | Configurable via `IDLE_TIMEOUT` env |
| **Controller** | K8s informer | Channel + Skill CRDs | **event-driven** | Not polling — uses K8s watch |

### Timing Diagram: Cold Path (worst case)

```
T+0.0s   User sends message
T+0.0s   Connector receives from provider (WebSocket/long-poll)
T+0.1s   Connector writes to /data/inbound/
T+2.0s   Gateway polls inbound (worst case: just missed the 2s window)
T+2.1s   Gateway creates K8s Job
T+4.0s   Job pod scheduled + image pulled (cached: ~2s, cold: ~5-10s)
T+4.5s   Agent Phase 0: skills injected (~0.5s)
T+5.0s   Agent Phase 1: claude -p starts
T+8.0s   Claude responds (~3s for simple queries, longer for tool use)
T+8.1s   Agent writes response.jsonl + status=done
T+10.0s  Gateway polls results (worst case: just missed 2s window)
T+10.1s  Gateway writes to /data/outbox/{channel}/
T+11.0s  Connector polls outbox (worst case: just missed 1s window)
T+11.1s  Connector sends via provider API
T+11.2s  User sees response
```

**Typical end-to-end latency: 5-15 seconds** (dominated by pod scheduling + LLM response time, not by polling).

### Timing Diagram: Hot Path (follow-up to running Job)

```
T+0.0s   User sends follow-up message
T+0.0s   Connector receives, writes to /data/inbound/
T+2.0s   Gateway polls inbound (worst case)
T+2.1s   Gateway detects running Job for group → enqueues to /data/groups/{group}/
T+4.0s   Agent polls per-group queue (worst case: just missed 2s window)
T+4.1s   Agent runs claude -p --resume
T+7.0s   Claude responds
T+7.1s   Agent appends to response.jsonl
         (Agent does NOT write status=done — stays alive for more follow-ups)
         ...eventually idle timeout → status=done → normal results flow
```

**Hot path saves ~4-5 seconds** by skipping pod scheduling.

---

## 8. Component Communication Matrix

```
                  Controller    Connector    Gateway    Agent Job
Controller           —            creates      —          —
                                  Deployments
                                  (K8s API)

Connector            —              —          →          —
                                            /data/inbound/
                                            (writes inbound msgs)

Gateway              —           ←              —         creates Jobs
                              /data/outbox/              (K8s API)
                              (writes outbound)          →
                                                      /data/groups/
                                                      (writes follow-ups)

Agent Job            —              —          →          —
                                            /data/results/
                                            (writes status + response)
```

Arrows show data flow direction:
- **Connector → Gateway:** via `/data/inbound/` (nq files)
- **Gateway → Connector:** via `/data/outbox/{channel}/` (nq files)
- **Gateway → Agent Job:** via `/data/groups/{group}/` (nq files, hot path only)
- **Gateway → Agent Job:** via K8s Job creation (cold path, passes env vars)
- **Agent Job → Gateway:** via `/data/results/{session}/` (status + response.jsonl)
- **Controller → Connector:** via K8s Deployment creation/deletion
- **Controller → PVC:** writes `/data/skills/` (read by Agent Jobs)

### What does NOT communicate directly

| A | B | Why not |
|---|---|---|
| Controller | Gateway | No need. Gateway doesn't know about CRDs. |
| Controller | Agent Job | Skills are on PVC. No direct interaction. |
| Connector | Agent Job | All routing goes through Gateway. |
| Agent Job | Connector | Agent writes results → Gateway reads → Gateway writes outbox → Connector reads. |

---

## 9. File Locking & Concurrency

**No file locking is used.** The design avoids concurrent writes to the same file:

| Guarantee | How |
|---|---|
| No two writers to same queue file | Each writer creates a unique filename (timestamp + uuid) |
| No partial reads | Atomic rename pattern (write to `.tmp-*`, rename to `,*.nq`) |
| One reader per queue | Each queue has exactly one reader (Gateway for inbound, Connector for its outbox, Agent for its group queue) |
| No concurrent results writes | Each session-id is unique, only one Job writes to its results dir |
| groups.json consistency | Only Gateway writes; single-replica Deployment, single-threaded |

The only potential race is if Gateway creates a Job at the exact moment a previous Job for the same group is finishing. This is handled by the Gateway checking Job status (via K8s API) before creating a new one.

---

## 10. Error Handling at the Protocol Level

### Malformed Messages

| Who detects | What | Action |
|---|---|---|
| Gateway | Inbound message fails validation (§4.1) | Log warning, delete file, skip. Do not create Job. |
| Connector | Outbound message missing meta fields | Log error, delete file, skip. Message is lost. |
| Agent | Follow-up message malformed | Log warning, skip, continue polling. |

### Stuck Files

| Queue | Stuck = | Recovery |
|---|---|---|
| `/data/inbound/` | File older than 5 minutes | Gateway cleanup loop deletes stale files and logs warning |
| `/data/outbox/{channel}/` | File older than 5 minutes | Connector logs warning but does NOT delete (may indicate connector bug) |
| `/data/groups/{group}/` | File still present after Job exits | Gateway cleanup detects no running Job + pending files → creates new Job |
| `/data/results/{session}/status` = "running" | Job pod is gone (crashed) | Gateway detects Job completed/failed via K8s API → treats as error, routes error message to outbox |

### Results Dir Lifecycle

```
1. Gateway creates:          /data/results/{session-id}/request.json
2. Agent writes:             /data/results/{session-id}/status  → "running"
3. Agent writes:             /data/results/{session-id}/response.jsonl  (streaming)
4. Agent writes:             /data/results/{session-id}/status  → "done" (or "error")
5. Gateway reads:            status, response.jsonl, request.json
6. Gateway moves:            /data/results/{session-id}/ → /data/results/.done/{session-id}/
7. Gateway cleanup (daily):  rm -rf /data/results/.done/* older than 24h
```
