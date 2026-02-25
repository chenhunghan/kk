# kk: Controller

> **Cross-references:**
> [Communication Protocol](kk-protocol.md) ·
> [Connector](kk-connector.md) ·
> [Gateway](kubeclaw-plan-gateway.md) ·
> [Agent Job](kubeclaw-plan-agent-job.md) ·
> [Skill](kubeclaw-plan-skill.md)

---

## Summary

The Controller is a single-replica Deployment that watches two CRD types and reconciles desired state:

| CRD | What Controller does | Child resource |
|---|---|---|
| `Channel` | Creates/updates/deletes Connector Deployments | `Deployment` (via K8s API) |
| `Skill` | Clones repo, extracts skill dir to PVC, cleans up on delete | Files on PVC (no K8s child) |

The Controller has **no knowledge** of the Gateway, Agent Jobs, messages, or queues. It only manages Deployments and PVC files.

---

## Implementation

- **Crate:** `crates/kk-controller`
- **Language:** Rust
- **Key dependencies:** `kube` (kube-rs) v0.98, `k8s-openapi` v0.24, `tokio`, `axum`, `serde_yaml`

### Source layout

| File | Purpose |
|---|---|
| `src/lib.rs` | Exports `config`, `crd`, `reconcilers` modules |
| `src/main.rs` | Loads config, ensures PVC dirs, starts health server + both reconcilers |
| `src/config.rs` | `ControllerConfig::from_env()` |
| `src/crd.rs` | `Channel` and `Skill` CRD definitions (`kk.io/v1alpha1`) |
| `src/reconcilers/channel.rs` | Channel reconciler — builds Connector Deployments via server-side apply |
| `src/reconcilers/skill.rs` | Skill reconciler — git clone, validate, copy to PVC, finalizer cleanup |

---

## Image

```
kk-controller:latest

Base:   debian-slim (or alpine:3.19)
Install: git, ca-certificates
Runtime: Rust binary
Size:    ~15MB (static Rust binary + git)
```

`git` is required in the image for Skill cloning. The Controller shells out to `git clone` rather than using a git library — simpler, proven, and avoids additional dependencies.

---

## Environment Variables

| Var | Default | Description |
|---|---|---|
| `IMAGE_CONNECTOR` | `kk-connector:latest` | Image used for Connector Deployments |
| `DATA_DIR` | `/data` | PVC mount path |
| `NAMESPACE` | Auto-detected (in-cluster) | Namespace to watch. Falls back to reading `/var/run/secrets/.../namespace`, then `default` |
| `SKILL_CLONE_TIMEOUT` | `60` | Seconds before git clone is killed |
| `PVC_CLAIM_NAME` | `kk-data` | Name of the PersistentVolumeClaim to mount in Connector Deployments |

---

## Startup Sequence

```
1. Load ControllerConfig from env vars (with defaults)
2. Ensure PVC directory structure exists:
   DataPaths::ensure_dirs() → creates inbound/, outbox/, groups/, results/, skills/, sessions/, memory/, state/
3. Build K8s client (in-cluster config via ServiceAccount token)
4. Start health server on :8081 (/healthz, /readyz)
5. Start Channel controller (kube-rs Controller with .owns(Deployments))
6. Start Skill controller (kube-rs Controller with finalizer)
7. Both controllers run concurrently via tokio::select!
```

---

## Channel Reconciliation

### Trigger

kube-rs `Controller::new(channels).owns(deployments)` — reconciles on Channel CR changes and child Deployment status changes.

### Logic

```
reconcile(channel):

  # 1. Validate secretRef exists
  secret = GET Secret channel.spec.secretRef in channel.namespace
  IF secret == 404:
    UPDATE channel.status:
      phase: "Error"
      message: "Secret '{secretRef}' not found"
    RETURN (requeue after 30s — secret might be created later)

  # 2. Build desired Connector Deployment
  desired = build_connector_deployment(channel, config)
  #   → name: "connector-{channel.name}"
  #   → labels: app=kk-connector, channel={name}, channel-type={type}
  #   → ownerReferences: Channel CR (controller=true, blockOwnerDeletion=true)
  #   → container env: CHANNEL_TYPE, CHANNEL_NAME, INBOUND_DIR, OUTBOX_DIR, GROUPS_D_FILE
  #   → envFrom: secretRef from channel.spec.secretRef
  #   → channel.spec.config keys merged as CONFIG_{UPPER_SNAKE(key)} env vars
  #   → PVC volume mount at $DATA_DIR (claimName from PVC_CLAIM_NAME)

  # 3. Server-side apply (no manual diff needed)
  applied = PATCH Deployment with Patch::Apply, field manager "kk-controller", force=true

  # 4. Ensure outbox directory exists
  mkdir -p $DATA_DIR/outbox/{channel.name}

  # 5. Update status based on Deployment readiness
  IF applied.status.readyReplicas >= 1:
    phase: "Connected", message: "Connector running"
  ELSE IF Deployment condition Available=False with CrashLoopBackOff/Error:
    phase: "Error", message: condition message
  ELSE:
    phase: "Pending", message: "Waiting for connector to become ready"

  Requeue after 300s
```

### Deletion

ownerReferences on the Connector Deployment cause K8s garbage collector to delete it automatically when the Channel CR is deleted. No explicit delete logic needed.

### Helper: `to_upper_snake`

Converts camelCase/kebab-case config keys to UPPER_SNAKE_CASE for env vars:
- `botToken` → `BOT_TOKEN`
- `allowedChatIds` → `ALLOWED_CHAT_IDS`
- `some-key` → `SOME_KEY`

---

## Skill Reconciliation

### Trigger

kube-rs `Controller::new(skills)` — reconciles on Skill CR changes. Uses `kube::runtime::finalizer` with finalizer name `skills.kk.io/cleanup` for cleanup on delete.

### Logic

```
Finalizer::Apply(skill):

  # 1. Parse source
  source = skill.spec.source  # e.g. "acme/skills-repo/tools/weather"
  Split into owner/repo/path (min 3 segments)
  cloneURL = "https://github.com/{owner}/{repo}.git"

  # 2. Update status → Fetching
  phase: "Fetching", message: "Cloning repository"

  # 3. Git clone (shallow, treeless) to temp dir
  git clone --depth 1 --single-branch --filter=blob:none {cloneURL} {tmpDir}
  Timeout: SKILL_CLONE_TIMEOUT seconds (via tokio::time::timeout)

  # 4. Validate skill path exists in cloned repo
  IF tmpDir/{path} is not a directory → Error

  # 5. Validate SKILL.md exists and parse frontmatter
  Read tmpDir/{path}/SKILL.md
  Parse YAML between --- delimiters → requires name + description fields

  # 6. Copy skill dir to PVC
  rm -rf $DATA_DIR/skills/{skill.name}
  cp -r tmpDir/{path} → $DATA_DIR/skills/{skill.name}

  # 7. Set permissions
  Directories: 755, regular files: 644, scripts (.sh, .py, "run"): 755

  # 8. Update status → Ready
  phase: "Ready", skillName, skillDescription, lastFetched (ISO 8601)

  # 9. Cleanup temp dir

Finalizer::Cleanup(skill):
  rm -rf $DATA_DIR/skills/{skill.name}
```

### YAML Frontmatter Parsing

SKILL.md files use YAML frontmatter (delimited by `---`):

```markdown
---
name: frontend-design
description: React and Next.js performance optimization guidelines from Vercel Engineering
---

# Frontend Design

Instructions for the agent...
```

Required fields: `name` (non-empty), `description` (non-empty). Parsed via `serde_yaml`.

---

## Error Handling

### Channel Errors

| Error | Detection | Status Update | Recovery |
|---|---|---|---|
| Secret not found | GET Secret returns 404 | phase=Error, message="Secret not found" | Requeue after 30s (secret may be created later) |
| Deployment apply fails | K8s API error | Propagated via error_policy | Requeue after 30s |
| Connector crashlooping | Deployment condition check | phase=Error, message=condition message | Requeue after 300s (auto-heals if transient) |
| Invalid channel type | Deployment created but connector exits | phase=Error via Deployment status | User must fix CR spec |

### Skill Errors

| Error | Detection | Status Update | Recovery |
|---|---|---|---|
| Invalid source format | String parsing (< 3 segments) | phase=Error, message="expected owner/repo/path" | Requeue after 300s |
| Git clone fails (404) | git exit code + stderr | phase=Error, message="git clone failed: ..." | Error propagated, requeue after 60s |
| Git clone timeout | tokio::time::timeout | phase=Error, message="timed out after Ns" | Error propagated, requeue after 60s |
| Path not found in repo | Directory check | phase=Error | Error propagated, requeue after 60s |
| SKILL.md missing | File check | phase=Error | Error propagated, requeue after 60s |
| Frontmatter invalid | serde_yaml parse | phase=Error, message="invalid frontmatter YAML" | Error propagated, requeue after 60s |
| PVC write fails | std::io::Error | phase=Error | Error propagated, requeue after 60s |

### Requeue Strategy

| CRD | On success | On error |
|---|---|---|
| Channel | Requeue after 300s (periodic re-check) | Requeue after 30s (via error_policy) |
| Skill | `await_change()` (no periodic requeue) | Requeue after 60s (via error_policy) |

---

## RBAC

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: kk-controller
  namespace: kk
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: kk-controller
  namespace: kk
rules:
# Watch and update Channel CRs
- apiGroups: ["kk.io"]
  resources: ["channels", "channels/status"]
  verbs: ["get", "list", "watch", "update", "patch"]

# Watch and update Skill CRs (including finalizer management)
- apiGroups: ["kk.io"]
  resources: ["skills", "skills/status"]
  verbs: ["get", "list", "watch", "update", "patch"]

# Manage Connector Deployments (child of Channel CRs)
- apiGroups: ["apps"]
  resources: ["deployments"]
  verbs: ["create", "get", "list", "watch", "update", "patch", "delete"]

# Validate that Secrets exist (read-only)
- apiGroups: [""]
  resources: ["secrets"]
  verbs: ["get"]

# Events (for recording reconciliation events on CRs)
- apiGroups: [""]
  resources: ["events"]
  verbs: ["create", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: kk-controller
  namespace: kk
subjects:
- kind: ServiceAccount
  name: kk-controller
roleRef:
  kind: Role
  name: kk-controller
  apiGroup: rbac.authorization.k8s.io
```

---

## Deployment Manifest

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: kk-controller
  namespace: kk
  labels:
    app: kk-controller
spec:
  replicas: 1                           # single replica — leader election not needed
  selector:
    matchLabels:
      app: kk-controller
  template:
    metadata:
      labels:
        app: kk-controller
    spec:
      serviceAccountName: kk-controller
      containers:
      - name: controller
        image: kk-controller:latest
        env:
        - name: IMAGE_CONNECTOR
          value: kk-connector:latest
        - name: DATA_DIR
          value: /data
        - name: SKILL_CLONE_TIMEOUT
          value: "60"
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
            path: /healthz
            port: 8081
          initialDelaySeconds: 15
          periodSeconds: 20
        readinessProbe:
          httpGet:
            path: /readyz
            port: 8081
          initialDelaySeconds: 5
          periodSeconds: 10
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: kk-data
```

---

## Health Endpoints

The Controller exposes two HTTP endpoints on port 8081 (internal only, no Service needed):

| Endpoint | Meaning | Returns 200 when |
|---|---|---|
| `GET /healthz` | Liveness | Process is running |
| `GET /readyz` | Readiness | Process is running |

---

## Logging

Structured JSON logs via `tracing` + `tracing-subscriber` (JSON format, level controlled by `RUST_LOG`):

```jsonc
// Startup
{"level":"info","message":"starting kk-controller"}
{"level":"info","message":"loaded config","namespace":"kk","data_dir":"/data"}
{"level":"info","message":"health server listening on :8081"}
{"level":"info","message":"starting channel controller"}
{"level":"info","message":"starting skill controller"}

// Channel reconciliation
{"level":"info","message":"reconciling channel","channel":"telegram-bot-1","namespace":"kk"}
{"level":"info","message":"channel reconciled","channel":"telegram-bot-1","phase":"Connected"}

// Skill reconciliation
{"level":"info","message":"applying skill","skill":"frontend-design","source":"acme/skills-repo/tools/weather"}
{"level":"info","message":"skill installed successfully","skill":"frontend-design"}

// Errors
{"level":"warn","message":"secret not found","channel":"slack-eng","secret":"slack-eng-creds"}
{"level":"error","message":"channel reconciliation error","channel":"slack-eng","error":"..."}
{"level":"error","message":"skill reconciliation error","skill":"bad-skill","error":"git clone failed: ..."}
```

---

## Unit Tests

13 unit tests in `crates/kk-controller` covering all pure functions:

### Config (`src/config.rs`)
- `test_config_from_env` — defaults, env overrides, invalid timeout fallback

### Channel (`src/reconcilers/channel.rs`)
- `test_to_upper_snake` — camelCase, kebab-case, dot-case conversions
- `test_build_connector_deployment_basic` — metadata, ownerRef, labels, env vars, secretRef, PVC mount
- `test_build_connector_deployment_with_config` — spec.config merged as CONFIG_* env vars

### Skill (`src/reconcilers/skill.rs`)
- `test_parse_source_valid` — full path with nested dirs
- `test_parse_source_minimal` — 3-segment minimum
- `test_parse_source_too_few_parts` — rejects 1 or 2 segments
- `test_parse_source_empty_segment` — rejects empty owner/repo/path
- `test_parse_frontmatter_valid` — extracts name + description
- `test_parse_frontmatter_missing_name` — rejects missing required field
- `test_parse_frontmatter_no_delimiters` — rejects non-frontmatter content
- `test_parse_frontmatter_missing_closing` — rejects unclosed frontmatter
- `test_parse_frontmatter_empty_name` — rejects empty name string

### Integration Tests (require live K8s cluster)

| Test | What it verifies |
|---|---|
| Create Channel CR → Connector Deployment created with correct env, labels, ownerRef | Channel reconciler |
| Delete Channel CR → Connector Deployment garbage collected | ownerReferences |
| Update Channel CR secretRef → Connector Deployment updated | Server-side apply |
| Channel CR with non-existent Secret → status=Error | Secret validation |
| Connector pod crashlooping → Channel status=Error with reason | Status detection |
| Connector pod becomes ready → Channel status=Connected | Status detection |
| Create Skill CR with valid source → files at `/data/skills/{name}/` | Skill reconciler |
| Delete Skill CR → `/data/skills/{name}/` removed | Finalizer cleanup |
| Invalid source format → status=Error | Source parsing |
| Source points to non-existent repo → status=Error | Git clone error |
| Git clone timeout → status=Error | Timeout handling |
| SKILL.md missing from path → status=Error | File validation |
| SKILL.md missing frontmatter name → status=Error | Frontmatter parsing |
| Scripts in skill dir → executable permissions set | Permission handling |
