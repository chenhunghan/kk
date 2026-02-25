# kk-gateway

> **Crate:** `crates/kk-gateway` — Rust, async (tokio)
> **Cross-references:** [Protocol](kk-protocol.md) · [Controller](kk-controller.md) · [Connector](kk-connector.md) · [Agent Job](kk-agent.md) · [Skill](kubeclaw-plan-skill.md)

---

## Summary

The Gateway is a single-replica Deployment and the **routing brain** of kk. It reads messages from the inbound queue, decides whether to act, creates Agent Jobs (cold path) or forwards to running Jobs (hot path), reads completed results, writes streaming updates to the stream directory, and writes final responses to the outbox.

All inter-component communication is via files on a shared RWX PVC — no network calls between components.

The Gateway has **no knowledge** of Skills, provider SDKs, or LLM APIs. It only knows about queues, groups, triggers, and K8s Jobs.

---

## Crate Structure

```
crates/kk-gateway/
  Cargo.toml
  src/
    lib.rs          # pub mod config, loops, state
    main.rs         # binary entrypoint (uses kk_gateway::*)
    config.rs       # GatewayConfig (env vars)
    state.rs        # SharedState, ActiveJob, routing_key(), load_groups_config()
    loops/
      mod.rs        # pub mod cleanup, inbound, results, state_reload
      inbound.rs    # Inbound loop: poll_once → route_message → cold_path / hot_path
      results.rs    # Results loop: poll_once → process_done / process_error
      cleanup.rs    # Cleanup loop: cleanup_once → 4 sub-tasks
      state_reload.rs  # State reload loop: reloads groups config from disk
  tests/
    common/mod.rs   # GatewayTestHarness
    e2e_gateway.rs  # 23 e2e tests
```

**Dependencies:** `kk-core` (workspace), `kube` 0.98, `k8s-openapi` 0.24 (v1_32), `tokio`, `serde`/`serde_json`, `tracing`, `anyhow`, `thiserror`, `chrono`
**Dev-dependencies:** `tempfile` 3, `filetime` 0.2

---

## Concurrent Loops

| Loop | Module | Polls | Default Interval | Purpose |
|---|---|---|---|---|
| **Inbound** | `loops::inbound` | `/data/inbound/` | 2s | Route messages: create Jobs (cold) or enqueue follow-ups (hot) |
| **Results** | `loops::results` | `/data/results/*/status` | 2s | Read completed results, stream partials, write final to outbox, archive |
| **Cleanup** | `loops::cleanup` | K8s Jobs API + filesystem | 60s | Delete finished Jobs, detect crashed Jobs, clean orphaned queues, purge archives |
| **State reload** | `loops::state_reload` | `/data/state/groups*.json` | 30s | Reload groups config from disk |

Each loop exposes `pub async fn run(state: SharedState)` (infinite loop) and `pub async fn poll_once(state: &SharedState)` (single iteration, used by e2e tests).

---

## Configuration (`config.rs`)

`GatewayConfig` is loaded from environment variables via `from_env()`.

| Env Var | Field | Default | Description |
|---|---|---|---|
| `DATA_DIR` | `data_dir` | `/data` | PVC mount path |
| `NAMESPACE` | `namespace` | `default` | K8s namespace for Jobs |
| `IMAGE_AGENT` | `image_agent` | `kk-agent:latest` | Agent Job container image |
| `API_KEYS_SECRET` | `api_keys_secret` | `kk-api-keys` | Secret with LLM API keys |
| `JOB_ACTIVE_DEADLINE` | `job_active_deadline` | `300` | Max seconds an Agent Job can run |
| `JOB_TTL_AFTER_FINISHED` | `job_ttl_after_finished` | `300` | Seconds before K8s auto-deletes completed Job |
| `JOB_IDLE_TIMEOUT` | `job_idle_timeout` | `120` | Passed to Agent as `IDLE_TIMEOUT` |
| `JOB_MAX_TURNS` | `job_max_turns` | `25` | Passed to Agent as `MAX_TURNS` |
| `JOB_CPU_REQUEST` | `job_cpu_request` | `250m` | Agent Job CPU request |
| `JOB_CPU_LIMIT` | `job_cpu_limit` | `1` | Agent Job CPU limit |
| `JOB_MEMORY_REQUEST` | `job_memory_request` | `256Mi` | Agent Job memory request |
| `JOB_MEMORY_LIMIT` | `job_memory_limit` | `1Gi` | Agent Job memory limit |
| `INBOUND_POLL_INTERVAL` | `inbound_poll_interval_ms` | `2000` | ms between inbound polls |
| `RESULTS_POLL_INTERVAL` | `results_poll_interval_ms` | `2000` | ms between results scans |
| `CLEANUP_INTERVAL` | `cleanup_interval_ms` | `60000` | ms between cleanup sweeps |
| `STALE_MESSAGE_TIMEOUT` | `stale_message_timeout` | `300` | Seconds before inbound message is stale |
| `RESULTS_ARCHIVE_TTL` | `results_archive_ttl` | `86400` | Seconds before archived results are purged |
| `PVC_CLAIM_NAME` | `pvc_claim_name` | `kk-data` | PVC claim name for Agent Job volumes |
| `STATE_RELOAD_INTERVAL` | `state_reload_interval_ms` | `30000` | ms between groups config reload |

---

## Shared State (`state.rs`)

### `SharedState` (Clone via Arc)

```rust
pub struct SharedState {
    pub config: GatewayConfig,
    pub client: kube::Client,
    pub paths: DataPaths,
    pub active_jobs: Arc<RwLock<HashMap<String, ActiveJob>>>,
    pub groups_config: Arc<RwLock<GroupsConfig>>,
}
```

### `ActiveJob`

```rust
pub struct ActiveJob {
    pub job_name: String,        // "agent-family-chat-1708801290"
    pub session_id: String,      // "family-chat-1708801290" or "family-chat-t42-1708801290"
    pub group: String,
    pub thread_id: Option<String>,
    pub created_at: u64,         // Unix timestamp
}
```

### Routing Key

`routing_key(group, thread_id)` builds the `active_jobs` lookup key:
- Unthreaded: `"family-chat"`
- Threaded: `"family-chat|42"`

### Groups Config Loading

`load_groups_config(paths)` merges two layers:
1. **`/data/state/groups.d/*.json`** — auto-registered by Connectors (baseline)
2. **`/data/state/groups.json`** — admin overrides (takes precedence, wins on key conflict)

### Methods

- `SharedState::new(config, client, paths)` — loads groups config from disk
- `rebuild_active_jobs()` — lists K8s Jobs labeled `app=kk-agent`, populates `active_jobs` for running Jobs (handles gateway restarts). Reads `kk.io/group`, `kk.io/session-id`, `kk.io/thread-id` labels.
- `reload_groups_config()` — re-reads groups config from disk
- `status_json()` — returns `{"active_jobs": N, "group_count": N}` for health endpoint

---

## Startup Sequence (`main.rs`)

1. `kk_core::logging::init()` — structured JSON logging via `tracing`
2. `GatewayConfig::from_env()` — load config
3. `Client::try_default()` — K8s client (in-cluster ServiceAccount)
4. `DataPaths::new(&config.data_dir).ensure_dirs()` — create all PVC directories
5. `SharedState::new(config, client, &paths)` — load groups config
6. `state.rebuild_active_jobs()` — recover active Jobs from K8s (warn on failure, start fresh)
7. Start health server on `:8082` with `/status` endpoint
8. Spawn 4 concurrent loops: inbound, results, cleanup, state_reload
9. `tokio::select!` — exit if any loop fails

---

## Inbound Loop (`loops/inbound.rs`)

### `poll_once`

1. `nq::list_pending(inbound_dir)` — list `*.nq` files sorted FIFO
2. For each file:
   - Read and parse as `InboundMessage`
   - On parse error: delete file, continue (invalid JSON discarded)
   - On read error: skip (leave for retry)
   - Check staleness: `now - msg.timestamp > stale_message_timeout` → discard
   - `route_message(state, &msg)`
   - Delete processed file

### `route_message`

1. Look up `msg.group` in `groups_config` → skip if unregistered
2. **Stop command check**: if `is_stop_command(msg.text)` → `handle_stop` (write stop sentinel, remove from active_jobs)
3. Check trigger:
   - `Always` → always trigger
   - `Mention` → `msg.text.contains(trigger_pattern)`
   - `Prefix` → `msg.text.starts_with(trigger_pattern)`
4. Build routing key: `routing_key(group, thread_id)`
5. If key in `active_jobs` → **hot path**, else → **cold path**

### Stop Command

`is_stop_command(text)` matches `/stop` or `stop` (case-insensitive, trimmed). When a stop command is detected:

1. Look up active Job by routing key — if none, ignore
2. Write stop sentinel file to `/data/groups/{group}/_stop` (or `/data/groups/{group}/threads/{tid}/_stop` for threaded)
3. Remove from `active_jobs`

The Agent detects this sentinel file in Phase 2 and exits early with `status=stopped`.

### Cold Path (`cold_path`)

Creates a new Agent Job for a routing key with no active Job.

1. Generate `session_id` via `session_id_threaded(group, thread_id, timestamp)`
   - Unthreaded: `"eng-1708801290"`
   - Threaded: `"eng-t42-1708801290"`
2. Generate `job_name` via `job_name(&session_id)` → `"agent-eng-1708801290"` (sanitized to K8s name rules)
3. Create `/data/results/{session_id}/` directory
4. Write `request.json` (`RequestManifest` with channel, group, thread_id, sender, meta, messages)
5. Write `status` file: `"running"`
6. Ensure group queue dir exists (thread-aware: `/data/groups/{group}/` or `/data/groups/{group}/threads/{tid}/`)
7. Strip trigger pattern from prompt text
8. Build K8s Job spec via `build_agent_job` and create via K8s API
9. Insert into `active_jobs`

### Hot Path (`hot_path`)

Forwards a message to a running Agent Job as a follow-up.

1. Look up `ActiveJob` by routing key
2. Build `FollowUpMessage` (sender, text, timestamp, channel, thread_id, meta)
3. Atomic `nq::enqueue` to thread-aware group queue dir
4. Append to `request.json` messages array (bookkeeping)

### `strip_trigger`

- `Prefix` + pattern → `text.strip_prefix(pattern).trim()`
- `Mention` + pattern → `text.replace(pattern, "").trim()`
- `Always` / no pattern → `text` unchanged

### `build_agent_job`

Produces a `batch/v1 Job` with:

**Labels:** `app=kk-agent`, `kk.io/group`, `kk.io/session-id`, `kk.io/thread-id` (if threaded)

**Env vars:** `SESSION_ID`, `GROUP`, `DATA_DIR`, `IDLE_TIMEOUT`, `MAX_TURNS`, `THREAD_ID` (if threaded)

**envFrom:** Secret `api_keys_secret` (optional)

**Spec:** `backoffLimit: 0`, `restartPolicy: Never`, `activeDeadlineSeconds`, `ttlSecondsAfterFinished`, PVC volume mount at `/data`, configurable CPU/memory requests and limits.

---

## Results Loop (`loops/results.rs`)

### `poll_once`

1. Scan `/data/results/` directory entries
2. Skip `.done/` and non-directories
3. Read `status` file for each session dir
4. Dispatch on status:
   - `running` → `try_stream_partial` (tail response.jsonl for intermediate streaming updates)
   - `done` → `process_done`
   - `error` → `process_error`
   - `stopped` → `process_stopped`
   - `overflow` → `process_overflow`
   - unknown → warn and skip

### `process_done`

1. `read_manifest` — parse `request.json` for routing info
2. `extract_response_text` — parse `response.jsonl`
3. `write_final_or_outbox` — if a stream file exists, overwrite it with `meta.final: true` (Connector does final edit + cleanup); otherwise fall back to `write_outbound` to enqueue to `/data/outbox/{channel}/`
4. `archive_results` — move session dir to `/data/results/.done/`
5. `remove_from_active` — remove routing key from `active_jobs`

### `process_error`

1. `read_manifest`
2. Build error text: `"[kk] Agent job failed for session {session_id}. Check logs for details."`
3. `write_final_or_outbox`
4. `archive_results`
5. `remove_from_active`

### `process_stopped`

1. `read_manifest`
2. Build stopped text: `"[kk] Session stopped by user."`
3. `write_final_or_outbox`
4. `archive_results`
5. `remove_from_active`

### `process_overflow`

1. `read_manifest`
2. `extract_response_text` — get last response before overflow
3. Build overflow text with the last response + overflow notice
4. `write_final_or_outbox`
5. `archive_results`
6. `remove_from_active`

### `try_stream_partial`

Called for sessions with `status=running`. Tails `response.jsonl` incrementally, extracts the latest assistant text, and writes it to the stream file at `/data/stream/{channel}/{session-id}` (atomic write via tmp+rename) with `meta.final: false`. The Connector polls this file and sends/edits the platform message.

### `write_final_or_outbox`

Shared helper used by `process_done`, `process_error`, `process_stopped`, and `process_overflow`:
- If a stream file exists for this session → overwrite it with the final text and `meta.final: true` (Connector does the final edit and cleans up)
- If no stream file exists → fall back to `write_outbound` (enqueue to outbox as a regular message)

### `extract_response_text`

Parses `response.jsonl` (JSONL lines from Claude CLI output):

1. If file doesn't exist → `"(no response file)"`
2. Scan all lines, collecting:
   - Last `{"type":"result","result":"..."}` line → `last_result`
   - Last `{"type":"assistant","message":{"content":[{"type":"text","text":"..."}]}}` → `last_assistant_text`
3. Return `last_result` if present, else `last_assistant_text`, else `"(no response)"`

### `write_outbound`

Builds `OutboundMessage` (channel, group, thread_id, text, meta) and enqueues to `/data/outbox/{channel}/` via `nq::enqueue`.

---

## Cleanup Loop (`loops/cleanup.rs`)

### `cleanup_once`

Runs 4 sub-tasks sequentially:

1. **`cleanup_finished_jobs`** (K8s) — List Jobs with `app=kk-agent`, delete finished Jobs (succeeded or failed) past `job_ttl_after_finished`
2. **`detect_crashed_jobs`** (K8s) — Compare `active_jobs` against actual K8s Jobs. For Jobs in `active_jobs` but not in K8s: remove from `active_jobs`, write `"error"` to status if still `"running"`
3. **`cleanup_orphaned_queues`** (file-based, `pub`) — Scan `/data/groups/*/` and `/data/groups/*/threads/*/` for nq files older than `stale_message_timeout`, delete them
4. **`purge_old_archives`** (file-based, `pub`) — Scan `/data/results/.done/` for directories with mtime older than `results_archive_ttl`, `remove_dir_all`

Sub-tasks 1 and 2 require K8s API access. Sub-tasks 3 and 4 are purely file-based and are exposed as `pub fn` for e2e testing.

---

## State Reload Loop (`loops/state_reload.rs`)

Simple loop: `sleep(interval)` then `state.reload_groups_config()`. Errors are logged but don't crash.

---

## Health Endpoint

Port 8082 (via `kk_core::health::HealthServer`):

| Endpoint | Response |
|---|---|
| `GET /status` | `{"active_jobs": N, "group_count": N}` |

---

## Testing

### Unit Tests (13)

In `#[cfg(test)]` modules within source files:

| Module | Tests | What they cover |
|---|---|---|
| `state` | 3 | `routing_key`, `load_groups_config` (empty, merge with admin override) |
| `loops::inbound` | 6 | `strip_trigger` (prefix, mention, always), `build_agent_job` (threaded, unthreaded), `is_stop_command` |
| `loops::results` | 4 | `extract_response_text` (result line, assistant fallback, no file, empty file) |

### E2E Tests (23)

In `tests/e2e_gateway.rs`, using `GatewayTestHarness` from `tests/common/mod.rs`.

**Strategy:** Cold path creates K8s Jobs (untestable without a cluster). Hot path and results processing are purely file-based. Tests simulate cold_path file artifacts, then exercise hot path and results via `poll_once`.

#### GatewayTestHarness

- Creates `tempfile::TempDir` with full PVC directory structure via `DataPaths::ensure_dirs()`
- Constructs `SharedState` directly with dummy `kube::Client` pointing to `https://127.0.0.1:1` (never called)
- Helper methods: `new()`, `with_groups()`, `set_group()`, `enqueue_inbound()`, `simulate_cold_path()`, `simulate_agent_done()`, `simulate_agent_error()`, `add_active_job()`, `read_outbound()`, `read_follow_ups()`, `read_manifest()`, `active_job_count()`, `list_archived()`

#### Test Scenarios

**Full Lifecycle (Results Loop) — 8 tests:**

| Test | Description |
|---|---|
| `full_lifecycle` | simulate cold_path → agent done → results poll → outbound message + archived + active_jobs cleared |
| `full_lifecycle_threaded` | Same with thread_id="42", verify outbound has thread_id, routing key removed |
| `error_result` | simulate cold_path → agent error → results poll → outbound contains "[kk] Agent job failed" |
| `no_response_file` | status=done but no response.jsonl → outbound "(no response file)" |
| `empty_response_file` | status=done + empty response.jsonl → outbound "(no response)" |
| `assistant_fallback` | response.jsonl has only assistant lines (no "result") → outbound uses last assistant text |
| `running_status_skipped` | status=running → no outbound, no archive, active_job still present |
| `multiple_sessions_one_poll` | 2 done sessions → 2 outbound messages, both archived |

**Inbound Routing (Hot Path) — 11 tests:**

| Test | Description |
|---|---|
| `hot_path_follow_up` | Active job exists → inbound poll → follow-up in group queue + manifest updated |
| `hot_path_threaded` | Active job for eng\|42 → follow-up in `/groups/eng/threads/42/` |
| `trigger_always` | trigger_mode=Always + active job → message routed |
| `trigger_mention_match` | trigger_mode=Mention, pattern="@bot", text contains "@bot" → routed |
| `trigger_mention_no_match` | pattern="@bot", text without "@bot" → skipped |
| `trigger_prefix_match` | trigger_mode=Prefix, pattern="/ask", text="/ask foo" → routed |
| `trigger_prefix_no_match` | text doesn't start with "/ask" → skipped |
| `unregistered_group_skipped` | Empty groups config → message consumed, no routing |
| `stale_message_discarded` | Timestamp 600s old (stale_timeout=300) → consumed, no routing |
| `invalid_json_discarded` | Malformed nq file → consumed, no crash |
| `multiple_messages_one_poll` | 3 messages for same group with active job → 3 follow-ups |

**Groups Config — 1 test:**

| Test | Description |
|---|---|
| `groups_d_merge` | Two groups.d files + groups.json overlay → all groups present, admin overrides win |

**Cleanup (File-Based) — 3 tests:**

| Test | Description |
|---|---|
| `orphaned_queue_cleanup` | Old nq file in group queue → deleted by cleanup (uses filetime to backdate) |
| `orphaned_thread_queue_cleanup` | Old nq file in threads/ subdir → deleted |
| `archive_purge` | Old .done/ dir → removed by purge (uses filetime to backdate) |

### Running Tests

```bash
cargo test -p kk-gateway                       # all 36 tests (13 unit + 23 e2e)
cargo test -p kk-gateway --test e2e_gateway     # e2e only
cargo clippy -p kk-gateway                      # lint
cargo test --workspace                           # no regressions
```

---

## RBAC

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: kk-gateway
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: kk-gateway
rules:
- apiGroups: ["batch"]
  resources: ["jobs"]
  verbs: ["create", "get", "list", "watch", "delete"]
- apiGroups: [""]
  resources: ["pods"]
  verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: kk-gateway
subjects:
- kind: ServiceAccount
  name: kk-gateway
roleRef:
  kind: Role
  name: kk-gateway
  apiGroup: rbac.authorization.k8s.io
```

---

## Deployment Manifest

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: kk-gateway
  labels:
    app: kk-gateway
spec:
  replicas: 1
  selector:
    matchLabels:
      app: kk-gateway
  template:
    metadata:
      labels:
        app: kk-gateway
    spec:
      serviceAccountName: kk-gateway
      containers:
      - name: gateway
        image: kk-gateway:latest
        env:
        - name: DATA_DIR
          value: /data
        - name: IMAGE_AGENT
          value: kk-agent:latest
        - name: API_KEYS_SECRET
          value: kk-api-keys
        - name: PVC_CLAIM_NAME
          value: kk-data
        volumeMounts:
        - name: data
          mountPath: /data
        resources:
          requests:
            cpu: "100m"
            memory: "128Mi"
          limits:
            cpu: "500m"
            memory: "256Mi"
        livenessProbe:
          httpGet:
            path: /status
            port: 8082
          initialDelaySeconds: 10
          periodSeconds: 20
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: kk-data
```
