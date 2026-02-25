# AGENTS.md

Concise orientation for the `kk` monorepo.

## What This Repo Is

`kk` is a Rust monorepo for bridging chat platforms to an LLM agent runtime using file-based queues on a shared data directory.

Core message path:

1. Connector receives inbound provider events.
2. Connector writes `InboundMessage` files to `/data/inbound/`.
3. Gateway routes each message:
   - cold path: launch agent
   - hot path: enqueue follow-up
4. Agent writes status + streaming/final output to `/data/results/{session}/`.
5. Gateway writes outbound/stream files.
6. Connector sends or edits messages on the provider.

## Workspace Crates

- `crates/kk-core`: shared types, paths, queue ops, logging, health utilities.
- `crates/kk-controller`: reconciles `Channel` and `Skill` CRDs; manages connector Deployments and skill files on data volume.
- `crates/kk-gateway`: routing brain; inbound/results/cleanup/state-reload loops; launches agents (K8s or local launcher).
- `crates/kk-connector`: provider adapters + inbound/outbound workers.
- `crates/kk-agent`: sequential job runner around Claude CLI (`phase_0..phase_3` lifecycle).
- `crates/kk-cli`: lean/local mode entrypoint (`kk`) for non-K8s runs.

## Runtime Modes

- K8s mode: controller/gateway/connectors as Deployments, agent as ephemeral Jobs.
- Lean mode: `kk` binary runs gateway + terminal channel in-process and launches connectors/agents as child processes.

See: [lean.md](./lean.md)

## Data Directory Contracts

- `/data/inbound/`: connector -> gateway
- `/data/groups/{group}/...`: gateway -> agent follow-ups (+ `_stop`)
- `/data/results/{session}/`: agent -> gateway status + response stream
- `/data/outbox/{channel}/`: gateway -> connector final outbound
- `/data/stream/{channel}/`: gateway -> connector streaming updates
- `/data/state/groups.d/`: connector auto-registration output
- `/data/skills/`: controller-managed installed skills, consumed by agent
- `/data/sessions/`: persisted LLM session state

Protocol source of truth: [kk-protocol.md](./kk-protocol.md)

## Current Snapshot (Doc-Based)

From [kk-record.md](./kk-record.md) (Session 14, 2026-02-25):

- Main architecture and streaming path are implemented.
- Session resume, stop sentinel, overflow handling are implemented.
- K8s integration tests were added for controller.
- Remaining items called out: CI/CD workflows, Docker image/manifests in repo form, and some optional/deferred features.

Treat `kk-record.md` as the running status log and verify against code for final truth.

## Documentation Index

- [kk-record.md](./kk-record.md): chronological build record + status snapshot.
- [kk-protocol.md](./kk-protocol.md): inter-component protocol, file paths, message formats.
- [kk-controller.md](./kk-controller.md): controller behavior, reconciler logic, RBAC/testing notes.
- [kk-gateway.md](./kk-gateway.md): gateway loops, routing, job lifecycle, result handling.
- [kk-connector.md](./kk-connector.md): connector architecture, provider abstraction, inbound/outbound flow.
- [kk-agent.md](./kk-agent.md): agent lifecycle phases, context assembly, follow-up polling.
- [kubeclaw-plan-skill.md](./kubeclaw-plan-skill.md): skill model, CRD lifecycle, install/update/remove behavior.
- [kk-connector-stream-plan.md](./kk-connector-stream-plan.md): streaming architecture plan and implemented phases.
- [spawn.md](./spawn.md): comparative study of agent spawning models and lessons for kk.
- [chat-streaming.md](./chat-streaming.md): analysis of chat streaming patterns (Vercel SDK reference).
- [lean.md](./lean.md): local/lean runtime mode, config, process model.

## Quick Commands

```bash
# full test suite
cargo test --workspace

# targeted suites
cargo test -p kk-gateway
cargo test -p kk-connector
cargo test -p kk-agent
cargo test -p kk-controller

# lean mode run
ANTHROPIC_API_KEY=... cargo run -p kk-cli
```
