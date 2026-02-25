# kk — Development Record

A chronological record of everything built in the kk project, from first commit to current state.

---

## What is kk?

kk is a K8s-native system that bridges messaging platforms (Telegram, Slack, Discord, GitHub, WhatsApp, Teams, Google Chat, Linear; Signal planned) to LLM agents via file-based queues on a shared RWX PVC. All inter-component communication is through files on disk — no network calls between components.

**Language:** Rust monorepo
**CRD API group:** `kk.io` (v1alpha1)

---

## Session 1 — Scaffold the Monorepo

**Date:** 2026-02-24
**Commit:** `29053be` — *chore: scaffold the kk monorepo*

Started from the initial commit (`3441a99`) with the project plan documents (originally named `kubeclaw-protocol.md`, `kubeclaw-plan-*.md`).

Built the Rust workspace foundation:
- Created `Cargo.toml` workspace with 4 member crates
- `crates/kk-core` — shared types, nq queue ops, file paths, logging, health server
- `crates/kk-controller` — K8s controller (stub)
- `crates/kk-gateway` — routing brain (stub)
- `crates/kk-connector` — platform bridge (stub)
- Set up workspace-level dependencies: `kube` 0.98, `k8s-openapi` 0.24, `tokio`, `serde`, `tracing`, etc.

**kk-core** was implemented with:
- `types.rs` — `InboundMessage`, `OutboundMessage`, `FollowUpMessage`, `RequestManifest`, `ResultStatus`, `GroupsConfig`, `GroupEntry`, `ChannelType`, `TriggerMode`, `ResultLine`
- `nq.rs` — file-based queue: `list_pending()`, `enqueue()`, `read_message()`, `delete()`, `file_age_secs()` using `,{timestamp}.{unique_id}.nq` naming convention with atomic `.tmp` → `.nq` rename
- `paths.rs` — `DataPaths` struct with all PVC directory helpers (inbound, outbox, groups, results, skills, sessions, memory, state), thread-aware variants, `ensure_dirs()`, `session_id_threaded()`, `job_name()`, `sanitize_k8s_name()`
- `logging.rs` — structured JSON logging via `tracing-subscriber`
- `health.rs` — minimal HTTP health server for K8s probes

---

## Session 2 — Telegram Connector

**Date:** 2026-02-24
**Commit:** `ef60bf7` — *chore: abstract connecter*

Implemented the full kk-connector crate with an abstract provider pattern and Telegram as the first provider.

**Architecture:**
- Single binary, provider selected by `CHANNEL_TYPE` env var
- 3 concurrent tasks: provider dispatcher, inbound processor, outbound poller

**Provider trait abstraction:**
- `ProviderEvent` enum: `Message { chat_id, thread_id, sender, text, meta }`, `NewChat { chat_id, title }`
- Each provider returns a stream of `ProviderEvent` and implements `send_message()`

**Telegram provider** (`provider/telegram.rs`):
- Uses `teloxide` 0.13 for Bot API long-polling
- Dispatches `Message`, `EditedMessage`, `MyChatMember` (for group join detection)
- Sends replies via `send_message` with Telegram-specific meta (chat_id, message_id, reply markup)

**Inbound flow:**
- Provider event → resolve group via `GroupMap` → build `InboundMessage` → `nq::enqueue` to `/data/inbound/`

**Outbound flow:**
- Poll `/data/outbox/{channel}/` → read `OutboundMessage` → split text to platform limit (4096 chars for Telegram) → `provider.send_message()` → delete nq file

**Text splitting** (`kk-core/text.rs`):
- `split_text(text, max_len)` — splits at newlines first, then spaces, then hard-cuts, respecting per-platform limits

Also renamed plan doc from `kubeclaw-plan-connector.md` to `kk-connector.md` and updated content to reflect the Rust implementation.

---

## Session 3 — Naming Update & Documentation

**Date:** 2026-02-24, early afternoon
**Commit:** `474cea6` — *chore: update documentation for kk components and naming conventions*

Renamed all plan documents from `kubeclaw-*` to `kk-*` naming:
- `kubeclaw-protocol.md` → `kk-protocol.md`
- `kubeclaw-plan-controller.md` → `kk-controller.md`
- Updated all references inside documents from "kubeclaw" to "kk"

Established naming conventions:
- Services: `kk-controller`, `kk-gateway`, `kk-connector`, `kk-agent`
- CRDs: `channels.kk.io`, `skills.kk.io`
- Health ports: controller=8081, gateway=8082, connector=8083

---

## Session 4 — kk-controller Implementation

**Date:** 2026-02-24
**Commit:** `e094819` — *chore: init kk-controller*

Implemented the full kk-controller crate using kube-rs controller runtime.

**Channel reconciler** (`reconcilers/channel.rs`):
- Watches `Channel` CRs → creates/updates Connector `Deployment` for each
- Builds Deployment with: env vars from CR spec (BOT_TOKEN, CHANNEL_TYPE, etc.), PVC volume mount, owner references for garbage collection
- Handles create, update, and delete lifecycle

**Skill reconciler** (`reconcilers/skill.rs`):
- Watches `Skill` CRs → git clones source repo → validates `SKILL.md` frontmatter → copies to `/data/skills/{name}/`
- Parses YAML frontmatter from SKILL.md for metadata (name, description, version)
- Finalizer-based cleanup: removes skill directory from PVC on CR deletion

**CRD definitions** (`crd.rs`):
- `Channel` CRD: channel_type, bot_token, additional env vars
- `Skill` CRD: source (git URL), version

**Config** (`config.rs`): data_dir, namespace, connector image

13 unit tests covering config parsing, Deployment building, skill source parsing, and SKILL.md frontmatter validation.

---

## Session 5 — Thread Support (Telegram Topics, Slack Threads)

**Date:** 2026-02-24

Added `thread_id` as a first-class `Option<String>` field across the entire system for multi-platform thread support (Telegram Topics, Slack threads, Discord threads).

**Changes across all crates:**
- `InboundMessage`, `OutboundMessage`, `FollowUpMessage`, `RequestManifest` — added `thread_id: Option<String>` with `#[serde(skip_serializing_if = "Option::is_none")]`
- `DataPaths` — added `group_queue_dir_threaded()`, `session_dir_threaded()` for thread-aware paths (`/data/groups/{group}/threads/{tid}/`)
- `session_id_threaded()` — generates `{group}-t{tid}-{timestamp}` format
- `routing_key()` — `"group"` or `"group|thread_id"` for active_jobs map
- Backward compatible: `thread_id = None` falls back to unthreaded behavior

---

## Session 6 — Auto-Registration & Slack Connector

**Date:** 2026-02-24
**Commit:** `1e3bf99` — *chore: add slack connecter*
**Commit:** `adcdcf6` — *chore: add .DS_Store to .gitignore*

### Auto-Registration System

Eliminated the need for manual `groups.json` configuration. Connectors now auto-register unknown chats.

**GroupMap** (`connector/groups.rs`):
- Maintains `chat_id → group_slug` mapping
- On unknown chat_id: auto-generates slug (`tg-{abs(chat_id)}` for Telegram, `slack-{channel_id}` for Slack)
- Persists to `/data/state/groups.d/{channel-name}.json`
- Gateway merges `groups.d/*.json` (auto-registered baseline) + `groups.json` (admin overrides, wins on conflict)

### Slack Connector

**Slack provider** (`provider/slack.rs`):
- Socket Mode WebSocket connection for receiving events (no public URL needed)
- Web API (`chat.postMessage`) for sending
- Handles: `message` events, `member_joined_channel` for auto-discovery
- Thread support via Slack's `thread_ts` field
- App-level token for Socket Mode + Bot token for Web API

### Connector E2E Tests

Built a comprehensive test harness (`tests/common/mod.rs`):
- `TestHarness` — creates temp dir with PVC structure, group config, mock provider
- `provider_e2e_tests!` macro — generates parameterized test suite for each provider
- 6 tests per provider: `inbound_lifecycle`, `inbound_threaded`, `auto_register`, `new_chat_event`, `auto_register_reuses_slug`, `outbound_validation`
- Tests for both Telegram and Slack providers

---

## Session 7 — Remove axum, Simplify Health Server

**Date:** 2026-02-24
**Commit:** `c620cdb` — *chore: remove axum*
**Commit:** `e90b18c` — *chore: remove axum dependency from Cargo.toml*

Removed the `axum` HTTP framework dependency. The health server in `kk-core` was simplified to not require axum, using a lighter-weight approach for K8s health probes.

---

## Session 8 — kk-gateway Implementation & E2E Tests

**Date:** 2026-02-24
**(Current session — changes not yet committed)**

### Gateway Implementation

Implemented the full routing engine with 4 concurrent loops:

**Inbound loop** (`loops/inbound.rs`):
- Polls `/data/inbound/` every 2s
- For each nq file: parse `InboundMessage`, check staleness, check trigger match, route
- **Cold path**: No active Job → create results dir, write `request.json`, write `status=running`, build K8s Job spec, create via K8s API, add to `active_jobs`
- **Hot path**: Active Job exists → build `FollowUpMessage`, enqueue to group queue, append to `request.json` messages
- Trigger modes: `Always` (always fire), `Mention` (text contains pattern), `Prefix` (text starts with pattern)
- `strip_trigger()` removes pattern from prompt text before sending to agent

**Results loop** (`loops/results.rs`):
- Polls `/data/results/*/status` every 2s
- `status=done` → extract response text from `response.jsonl`, write `OutboundMessage` to outbox, archive to `.done/`, remove from `active_jobs`
- `status=error` → write error message `"[kk] Agent job failed..."` to outbox, archive, remove from `active_jobs`
- `extract_response_text()` — parses JSONL: last `"type":"result"` line wins, fallback to last assistant text block, fallback to `"(no response)"`
- `status=running` → skip

**Cleanup loop** (`loops/cleanup.rs`):
- Runs every 60s with 4 sub-tasks:
  1. `cleanup_finished_jobs` — delete K8s Jobs past TTL
  2. `detect_crashed_jobs` — compare `active_jobs` vs K8s, mark missing as error
  3. `cleanup_orphaned_queues` — delete stale nq files in group queues
  4. `purge_old_archives` — remove old `.done/` dirs past `results_archive_ttl`

**State reload loop** (`loops/state_reload.rs`):
- Reloads groups config from disk every 30s

**Shared state** (`state.rs`):
- `SharedState` with `Arc<RwLock<HashMap<String, ActiveJob>>>` and `Arc<RwLock<GroupsConfig>>`
- `load_groups_config()` merges `groups.d/*.json` + `groups.json` (admin wins)
- `rebuild_active_jobs()` recovers from K8s Jobs on startup
- Labels on Jobs: `app=kk-agent`, `kk.io/group`, `kk.io/session-id`, `kk.io/thread-id`

**Job builder** (`build_agent_job` in `inbound.rs`):
- K8s `batch/v1 Job` with: `backoffLimit: 0`, `restartPolicy: Never`, PVC mount, env vars (`SESSION_ID`, `GROUP`, `DATA_DIR`, `IDLE_TIMEOUT`, `MAX_TURNS`, `THREAD_ID`), `envFrom` Secret for API keys, configurable CPU/memory

### Bug Fix

Found and fixed a bug in results loop and cleanup loop: `results_dir("")` returns `{root}/results` and the code called `.parent()` which went to `{root}` instead of scanning `{root}/results`. Same pattern in `cleanup_orphaned_queues` with `group_queue_dir("")`. Fix: removed the `.parent()` calls, used the directory directly.

### Production Code Changes for Testability

- Added `src/lib.rs` re-exporting `config`, `loops`, `state` modules
- Added `[lib]` section to `Cargo.toml`
- Changed `main.rs` from `mod` declarations to `use kk_gateway::*` imports
- Made `poll_once` pub in inbound and results loops
- Made `cleanup_once`, `cleanup_orphaned_queues`, `purge_old_archives` pub in cleanup loop

### E2E Test Suite (23 tests)

Built `GatewayTestHarness` (`tests/common/mod.rs`):
- Creates temp dir with full PVC structure
- Dummy `kube::Client` pointing to `127.0.0.1:1` (never called)
- Helpers: `simulate_cold_path()`, `simulate_agent_done()`, `simulate_agent_error()`, `enqueue_inbound()`, `read_outbound()`, `read_follow_ups()`, etc.

**Test coverage:**
- **Results loop (8 tests):** full lifecycle, threaded lifecycle, error result, no response file, empty response, assistant fallback, running skip, multiple sessions
- **Inbound routing (11 tests):** hot path follow-up, threaded hot path, trigger always/mention/prefix (match and no-match), unregistered group, stale message, invalid JSON, multiple messages
- **Groups config (1 test):** groups.d merge with admin overlay
- **Cleanup (3 tests):** orphaned queue files, thread queue files, archive purge (using `filetime` to backdate mtimes)

### Documentation Update

Renamed `kubeclaw-plan-gateway.md` → `kk-gateway.md` and rewrote entirely to reflect 100% of current implementation.

---

## Session 9 — kk-gateway Commit & kk-agent Implementation

**Date:** 2026-02-24
**Commit:** `74f9578` — *chore: init kk-gateway*
**Commit:** `1bc19b0` — *chore: init kk-agent*
**Commit:** `da31d49` — *chore: add record*

### kk-gateway Committed

The gateway implementation from Session 8 was committed with all 4 loops, shared state, Job builder, and the full E2E test suite (23 tests).

### kk-agent Implementation

Built the full agent crate — a Rust binary that runs inside ephemeral K8s Jobs, wrapping the `claude` CLI.

**4-phase lifecycle** (`phases.rs`):
- **Phase 0 — Skill Injection**: Lists `/data/skills/`, validates each has `SKILL.md`, symlinks into `.claude/skills/` and `.agents/skills/` for Claude CLI discovery
- **Phase 1 — Initial Prompt**: Reads `request.json`, loads `SOUL.md` + `GROUP/CLAUDE.md` context, checks for persisted session ID, spawns `claude -p --output-format stream-json --max-turns N` (with `--resume {id}` if resuming), captures stdout to `response.jsonl` and stderr to `agent.log`
- **Phase 2 — Follow-up Polling**: Polls group queue every 2s for `FollowUpMessage` nq files, wraps as `"[Follow-up from {sender}]: {text}"`, spawns `claude -p --resume` for each, checks for `_stop` sentinel and context overflow between iterations
- **Phase 3 — Done**: Writes final status (`done`, `error`, `stopped`, or `overflow`) to status file

**Cross-job session resume**: Extracts `session_id` from `response.jsonl` and persists to `/data/sessions/{group}/.claude-session-id` so the next Job can `--resume` the conversation.

**Context overflow detection**: Scans `response.jsonl` and `agent.log` for 5 overflow patterns (context window, token limit, etc.), writes `status=overflow` to trigger fresh session.

**Stop sentinel**: Checks for `_stop` file before each Phase 2 iteration, writes `status=stopped` and exits cleanly.

**Testing**: 15 unit tests (prompt building, truncation, overflow detection, session ID extraction) + 18 E2E tests (skill injection, Phase 1 success/error, context assembly, follow-ups, idle timeout, threading, full lifecycle) using a mock `claude` bash script.

### Protocol Documentation Update

**Commit:** `099b586` — *docs: add Agent, Job, Cold/Hot Path to protocol key concepts*
**Commit:** `7d43763` — *docs: update kk-agent doc*
**Commit:** `8b03781` — *docs: add agent spawning comparative study*

Added `spawn.md` — comparative analysis of 5 messaging-to-LLM systems. Key patterns adopted: cross-job session resume, incremental JSONL streaming, stop sentinel, overflow detection.

---

## Session 10 — ChatProvider Trait & Edit-in-Place Streaming

**Date:** 2026-02-24
**Commit:** `ae0a6dc` — *feat(connector): add ChatProvider trait and edit-in-place streaming*

Refactored the connector's outbound abstraction from a `ProviderSender` enum to an async `ChatProvider` trait.

**ChatProvider trait** (`provider/mod.rs`):
- `send(&self, msg) -> Result<()>` — send a new message
- `send_returning_id(&self, msg) -> Result<String>` — send and return platform message ID
- `edit(&self, msg, platform_msg_id) -> Result<()>` — edit an existing message
- `supports_native_stream()` — returns `false` by default, overridden by Slack

**Edit-in-place streaming** (`outbound.rs`):
- First streaming message: `send_returning_id()` → save platform msg ID to `.stream-{sid}` state file
- Subsequent updates: `edit()` with saved msg ID
- Final message: `edit()` + cleanup state file
- Fallback for platforms without native streaming APIs

Implemented `TelegramOutbound` and `SlackOutbound` as `ChatProvider` impls.

---

## Session 11 — Session Resume, Stop, Overflow, Agent Hardening

**Date:** 2026-02-24
**Commit:** `ca9aefe` — *feat: add session resume, streaming, stop, and overflow handling*

Enhanced the agent and gateway with production resilience features:

**Gateway additions** (`loops/results.rs`):
- 5-status dispatch: `Running` → `try_stream_partial()`, `Done`, `Error`, `Stopped`, `Overflow`
- `try_stream_partial()` — tails `response.jsonl` for in-progress streaming
- `write_final_or_outbox()` — writes final to stream file if streaming was active, otherwise falls back to outbox
- `process_stopped()`, `process_overflow()` handlers with appropriate outbox messages

**Gateway additions** (`loops/inbound.rs`):
- `is_stop_command()` — detects `/stop` and `stop` messages
- `handle_stop()` — writes `_stop` sentinel file to group results dir

---

## Session 12 — Separate Stream Path

**Date:** 2026-02-25
**Commit:** `b6b7044` — *feat: separate stream path for streaming messages*
**Commit:** `1e2f844` — *docs: update connector stream plan with separate stream path*
**Commit:** `866227b` — *docs: add Vercel Chat SDK analysis and connector streaming plan*

Moved streaming from the outbox to a dedicated path, keeping the outbox clean for final/non-streaming messages only.

**New stream path**: `/data/stream/{channel}/{session-id}` — Gateway overwrites a single JSON file per session. Connector polls it, sends/edits the platform message, and deletes on `final: true`.

**Gateway changes**:
- `try_stream_partial()` writes to stream file (atomic tmp → rename) with `meta.final: false`
- `process_done()` writes final to stream file with `meta.final: true`, or falls back to outbox if no streaming happened

**Connector changes**:
- `poll_stream()` — new function polling `/data/stream/{channel}/`
- Dispatch logic: no state file → `send_returning_id()` + create state; state file exists → `edit()`; `final: true` → edit + cleanup both files
- `poll_outbound()` simplified — streaming logic removed, pure send loop

Added `chat-streaming.md` — analysis of Vercel's `@chat-adapter/slack` SDK validating the two-tier streaming design.

---

## Session 13 — Slack Native Streaming

**Date:** 2026-02-25
**Commit:** `2b06c77` — *feat(connector): add Slack native streaming via chat.startStream API*
**Commit:** `966b963` — *docs: update connector stream plan with Slack native streaming*

Implemented Slack's Oct 2025 native streaming APIs alongside the fallback edit-in-place pattern.

**ChatProvider trait additions**:
- `supports_native_stream()` — Slack returns `true`
- `stream_start(msg, text) -> Result<String>` — calls `chat.startStream`, returns msg ID
- `stream_append(msg_id, text) -> Result<()>` — calls `chat.appendStream`
- `stream_stop(msg_id, text) -> Result<()>` — calls `chat.stopStream` with final content

**Connector dispatch** (`poll_stream()`):
- Native path (Slack): `stream_start()` → `stream_append()` → `stream_stop()`
- Fallback path (Telegram, others): `send_returning_id()` → `edit()` → `edit()` + cleanup
- Both paths share the same `.stream-{sid}` state file pattern

**Streaming tests**: 5 native streaming tests (start, append, stop, final without prior, full lifecycle) + 6 fallback streaming tests.

---

## Session 14 — Documentation Sync & K8s Integration Tests

**Date:** 2026-02-25
**Commit:** `0db63bb` — *docs: sync all plan docs with implemented code*
**Commit:** `a662cc7` — *test(controller): add K8s integration tests with mock tower service*

### Documentation Audit

Systematically compared all 6 plan docs against actual code using parallel sub-agents, fixing every discrepancy:

- **kk-connector.md**: Added `team_id` to Slack metadata, `async-trait` to deps
- **kk-gateway.md**: Added 5-status dispatch, stop command, `try_stream_partial`, `write_final_or_outbox`, updated test count
- **kk-agent.md**: Added cross-job session resume, overflow detection, stop sentinel, updated test count (27→33)
- **kk-controller.md**: Fixed cross-references, skill parse error requeue clarification
- **kubeclaw-plan-skill.md**: Renamed all `kubeclaw` references to `kk`

### K8s Integration Tests

Added 11 integration tests for the controller using a mock `kube::Client` backed by `tower::service_fn` — no cluster required, sub-second execution.

**Mock infrastructure** (`tests/mock_k8s.rs`):
- `mock_client(handler)` — builds `kube::Client` from a closure, records all HTTP requests
- Response helpers: `secret_response`, `deployment_response`, `deployment_crashloop_response`, `channel_response`, `skill_response`, `not_found_response`
- CR builders: `channel_cr`, `skill_cr`

**Channel reconciler tests** (`tests/k8s_channel.rs`, 7 tests):
- `channel_deployment_ready_sets_connected` — Secret OK + ready deployment → Connected
- `channel_missing_secret_sets_error` — Missing secret → Error, 30s requeue
- `channel_deployment_pending_sets_pending` — Not ready → Pending
- `channel_deployment_crashloop_sets_error` — CrashLoopBackOff → Error
- `channel_config_merged_as_env_vars` — spec.config → CONFIG_* env vars
- `channel_deployment_has_owner_ref` — Correct ownerReference on Deployment
- `channel_creates_outbox_dir` — Outbox directory created on disk

**Skill reconciler tests** (`tests/k8s_skill.rs`, 4 tests):
- `skill_without_finalizer_adds_it` — Finalizer PATCH, returns await_change
- `skill_invalid_source_sets_error_status` — Bad source → Error, 300s requeue
- `skill_git_clone_failure_sets_error` — Failed clone → error propagated
- `skill_cleanup_on_delete` — Deletion → skill dir removed, finalizer removed

---

## Current State Summary

### Commits (25 total)

| # | Hash | Message |
|---|------|---------|
| 1 | `3441a99` | Initial commit |
| 2 | `29053be` | chore: scaffold the kk monorepo |
| 3 | `ef60bf7` | chore: abstract connecter |
| 4 | `474cea6` | chore: update documentation for kk components and naming conventions |
| 5 | `e094819` | chore: init kk-controller |
| 6 | `1e3bf99` | chore: add slack connecter |
| 7 | `adcdcf6` | chore: add .DS_Store to .gitignore |
| 8 | `c620cdb` | chore: remove axum |
| 9 | `e90b18c` | chore: remove axum dependency from Cargo.toml |
| 10 | `74f9578` | chore: init kk-gateway |
| 11 | `da31d49` | chore: add record |
| 12 | `1bc19b0` | chore: init kk-agent |
| 13 | `099b586` | docs: add Agent, Job, Cold/Hot Path to protocol key concepts |
| 14 | `7d43763` | docs: update kk-agent doc |
| 15 | `8b03781` | docs: add agent spawning comparative study |
| 16 | `ae0a6dc` | feat(connector): add ChatProvider trait and edit-in-place streaming |
| 17 | `ca9aefe` | feat: add session resume, streaming, stop, and overflow handling |
| 18 | `a446650` | docs: update spawn.md with implemented improvements |
| 19 | `866227b` | docs: add Vercel Chat SDK analysis and connector streaming plan |
| 20 | `b6b7044` | feat: separate stream path for streaming messages |
| 21 | `1e2f844` | docs: update connector stream plan with separate stream path |
| 22 | `2b06c77` | feat(connector): add Slack native streaming via chat.startStream API |
| 23 | `966b963` | docs: update connector stream plan with Slack native streaming |
| 24 | `0db63bb` | docs: sync all plan docs with implemented code |
| 25 | `a662cc7` | test(controller): add K8s integration tests with mock tower service |

### Crate Status

| Crate | Status | Unit Tests | Integration Tests | E2E Tests |
|-------|--------|------------|-------------------|-----------|
| kk-core | Complete | 23 | — | — |
| kk-controller | Complete | 13 | 11 | — |
| kk-connector | Complete | 24 | — | 12 (6 Telegram + 6 Slack) |
| kk-gateway | Complete | 13 | — | 23 |
| kk-agent | Complete | 15 | — | 19 |
| **Total** | | **88** | **11** | **54** |

**Grand total: 153 tests.**

### Plan Documents

| Document | Status |
|----------|--------|
| `kk-protocol.md` | Up to date |
| `kk-controller.md` | Up to date |
| `kk-connector.md` | Up to date |
| `kk-gateway.md` | Up to date |
| `kk-agent.md` | Up to date |
| `kubeclaw-plan-skill.md` | Up to date (renamed references to kk) |
| `kk-connector-stream-plan.md` | Up to date (all 3 phases complete) |
| `spawn.md` | Reference doc (comparative study) |
| `chat-streaming.md` | Reference doc (Vercel SDK analysis) |

### What Remains

- **CI/CD** — no GitHub Actions workflow yet
- **Docker images** — no Dockerfiles
- **K8s manifests** — documented in plan docs but not in repo as actual YAML
- **Signal connector** — `ChannelType::Signal` exists, but no `provider/signal.rs` implementation yet
- **Non-Slack/Telegram provider E2E coverage** — Discord/GitHub/WhatsApp/Teams/Gchat/Linear need dedicated E2E suites
- **Private repo skills** — optional `secretRef` field (deferred)
- **Skill branch/tag pinning** — optional `ref` field (deferred)
