# kk Lean Mode

Lean mode runs the full kk stack on a single machine without Kubernetes.
A single `kk` binary orchestrates everything using the local filesystem
and child processes instead of K8s Jobs, Deployments, and PVCs.

## Quick Start

```bash
# Build
cargo build -p kk-cli -p kk-connector -p kk-agent

# Run with defaults — opens an interactive terminal session
ANTHROPIC_API_KEY=sk-... kk

# You'll see a prompt:
# > hello, what can you do?
# (agent responds via stdout, streaming in real-time)

# Or with external channels too
cat > kk.yaml <<'EOF'
data_dir: ./data
channels:
  - name: my-telegram
    type: telegram
    env:
      TELEGRAM_BOT_TOKEN: "bot123:ABC..."
EOF
kk
# Terminal is always available alongside external channels
```

## How It Works

### Process Tree

```
kk (crates/kk-cli)
├── terminal connector (in-process, interactive stdin/stdout)
├── gateway loops (in-process)
│   ├── inbound   — polls ./data/inbound/, routes messages
│   ├── results   — polls ./data/results/*/status, writes outbox
│   ├── cleanup   — detects crashed agents, purges archives
│   └── state_reload — reloads groups config from disk
├── connector child processes (one per channel in kk.yaml)
│   ├── kk-connector [CHANNEL_TYPE=telegram, CHANNEL_NAME=my-telegram]
│   └── kk-connector [CHANNEL_TYPE=slack, CHANNEL_NAME=my-slack]
└── agent child processes (spawned on demand)
    └── kk-agent [SESSION_ID=..., GROUP=..., DATA_DIR=./data]
```

The `kk` binary runs the gateway routing logic in-process and spawns
connectors and agents as child processes. A watchdog loop monitors
connector processes and restarts them if they crash.

### Terminal Channel

When `kk` starts, it automatically registers a **terminal** channel
(`crates/kk-cli/src/terminal.rs`) that provides an interactive CLI
experience — just like chatting with Claude Code, but going through the
full queue infrastructure:

```
stdin (user types)
     ↓
terminal connector → ./data/inbound/*.nq      (InboundMessage, channel="terminal")
     ↓
gateway inbound loop → spawns kk-agent
     ↓
kk-agent runs Claude → ./data/results/{session}/response.jsonl
     ↓
gateway results loop → ./data/outbox/terminal/*.nq
     ↓
terminal connector (outbound poller) → stdout
```

The terminal channel auto-registers itself in `groups.d/terminal.json`
with `trigger_mode: always`. It supports streaming: partial responses
appear in real-time via `./data/stream/terminal/`, with the final
response delivered through the outbox.

No configuration needed — the terminal channel is always active when
running `kk`.

### Data Flow (External Channels)

The data flow for external channels (Telegram, Slack, etc.) is identical
to K8s mode. All components communicate through files on the shared
`data_dir` directory:

```
Platform API
     ↓
kk-connector → ./data/inbound/*.nq
     ↓
gateway (inbound loop) → spawns kk-agent child process
     ↓
kk-agent → ./data/results/{session}/response.jsonl
     ↓
gateway (results loop) → ./data/outbox/{channel}/*.nq
     ↓
kk-connector → Platform API
```

### Launcher Abstraction

The gateway uses a `Launcher` trait (`crates/kk-gateway/src/launcher/mod.rs`)
to abstract agent lifecycle management:

```rust
trait Launcher: Send + Sync {
    async fn launch(...)    -> Result<LaunchedAgent>;
    async fn list_active_handles() -> Result<Vec<String>>;
    async fn kill(handle)   -> Result<()>;
    async fn cleanup_finished(ttl) -> Result<()>;  // default: no-op
    async fn recover()      -> Result<Vec<LaunchedAgent>>; // default: empty
}
```

Two implementations:

| | K8sLauncher | LocalLauncher |
|---|---|---|
| Launch | `Api<Job>::create()` | `tokio::process::Command::spawn()` |
| List active | `Api<Job>::list()` + filter running | `child.try_wait()` on tracked PIDs |
| Kill | `Api<Job>::delete()` | `child.kill()` |
| Cleanup finished | Delete expired K8s Jobs | No-op (processes self-exit) |
| Recover on restart | Scan K8s Job labels | No-op (file-based recovery) |

The gateway binary (`kk-gateway`) selects the launcher via `KK_MODE` env var:
- `KK_MODE=k8s` (default) — uses `K8sLauncher`
- `KK_MODE=local` — uses `LocalLauncher`

The `kk` CLI binary always uses `LocalLauncher` directly.

### Recovery

On startup, active job state is recovered in two phases:

1. **File-based** (`rebuild_active_jobs_from_files`) — scans
   `./data/results/*/status` for entries with value `"running"`,
   reads the corresponding `request.json` to recover group and
   thread_id. This works identically in K8s and local mode.

2. **Launcher-specific** (`rebuild_active_jobs_from_launcher`) —
   in K8s mode, scans Job labels for additional recovery. In local
   mode this is a no-op since child processes are lost on restart
   and phase 1 already handles the file state.

If an agent process crashes (detected by the cleanup loop via
`list_active_handles()`), its status file is set to `"error"` and the
results loop delivers an error message back to the user.

## Configuration

### kk.yaml

Optional. If not present, all defaults apply. Place in the working
directory where `kk` is launched.

```yaml
# Root directory for all queue files (default: ./data)
data_dir: ./data

# Path to the kk-agent binary (default: kk-agent, found via PATH)
agent_bin: kk-agent

# Agent idle timeout in seconds (default: 120)
idle_timeout: 120

# Max LLM turns per agent session (default: 25)
max_turns: 25

# Channels to connect
channels:
  - name: my-telegram
    type: telegram
    env:
      TELEGRAM_BOT_TOKEN: "bot123:ABC..."

  - name: my-slack
    type: slack
    env:
      SLACK_BOT_TOKEN: "xoxb-..."
      SLACK_APP_TOKEN: "xapp-..."
```

All fields are optional. Secrets are passed via the `env` map on each
channel entry, or set as environment variables before running `kk`.
Agent processes inherit the parent environment, so API keys like
`ANTHROPIC_API_KEY` can be set in the shell.

### Directory Structure

Same as K8s mode, rooted at `data_dir` instead of the PVC mount:

```
./data/
├── inbound/              connector → gateway
├── outbox/{channel}/     gateway → connector
├── stream/{channel}/     gateway → connector (streaming)
├── groups/{group}/       gateway → agent (follow-ups)
├── results/{session}/    agent ↔ gateway
│   ├── request.json
│   ├── response.jsonl
│   └── status
├── results/.done/        archived results
├── skills/{skill}/       skill files (manually placed)
├── sessions/{group}/     persistent claude session state
├── memory/               SOUL.md, {group}/CLAUDE.md
└── state/
    ├── groups.json       admin overrides
    └── groups.d/         auto-registered by connectors
```

## What's Not Needed in Lean Mode

| K8s component | Lean mode equivalent |
|---|---|
| kk-controller | Not needed. No CRDs to reconcile. |
| Channel CRDs | `channels:` section in kk.yaml |
| Skill CRDs | Place skill dirs in `./data/skills/` manually |
| K8s Secrets | `env:` in kk.yaml or shell environment variables |
| PVC (RWX) | Local directory (`data_dir`) |
| K8s Jobs | Child processes spawned by `LocalLauncher` |
| K8s Deployments | Child processes spawned by `kk` |

## Build Without K8s Dependencies

The `kk` binary depends on `kk-gateway` with `default-features = false`,
which excludes the `kube` and `k8s-openapi` crates entirely:

```toml
# crates/kk-cli/Cargo.toml
[dependencies]
kk-gateway = { path = "../kk-gateway", default-features = false }
```

The gateway crate uses a feature flag:

```toml
# crates/kk-gateway/Cargo.toml
[features]
default = ["k8s"]
k8s = ["dep:kube", "dep:k8s-openapi"]
```

The `K8sLauncher` module is conditionally compiled:

```rust
#[cfg(feature = "k8s")]
pub mod k8s;
pub mod local;  // always available
```
