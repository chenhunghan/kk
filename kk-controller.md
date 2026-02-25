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

## Image

```
kk-controller:latest

Base:   alpine:3.19 (or debian-slim)
Install: git, ca-certificates
Runtime: Go binary (preferred) or Node.js
Size:    ~50MB
```

`git` is required in the image for Skill cloning. The Controller shells out to `git clone` rather than using a git library — simpler, proven, and avoids CGO dependencies.

---

## Environment Variables

| Var | Default | Description |
|---|---|---|
| `IMAGE_CONNECTOR` | `kk-connector:latest` | Image used for Connector Deployments |
| `DATA_DIR` | `/data` | PVC mount path |
| `NAMESPACE` | Auto-detected (in-cluster) | Namespace to watch. Falls back to reading `/var/run/secrets/.../namespace` |
| `RECONCILE_WORKERS` | `2` | Number of concurrent reconcile workers per CRD type |
| `SKILL_CLONE_TIMEOUT` | `60` | Seconds before git clone is killed |

---

## Startup Sequence

```
1. Read env vars, validate required ones
2. Build K8s client (in-cluster config via ServiceAccount token)
3. Verify CRDs exist:
   GET /apis/kk.io/v1alpha1 → must list "channels" and "skills"
   If missing → log FATAL "CRDs not installed. Apply CRDs before starting controller." → exit 1
4. Ensure PVC directories exist:
   mkdir -p $DATA_DIR/skills
5. Start shared informer factory with two informers:
   a. Informer: channels.kk.io/v1alpha1
      → onAdd/onUpdate/onDelete → enqueue to channelWorkQueue
   b. Informer: skills.kk.io/v1alpha1
      → onAdd/onUpdate/onDelete → enqueue to skillWorkQueue
6. Wait for informer caches to sync (with 30s timeout)
7. Start worker goroutines:
   - RECONCILE_WORKERS goroutines reading from channelWorkQueue
   - RECONCILE_WORKERS goroutines reading from skillWorkQueue
8. Block on SIGTERM/SIGINT → graceful shutdown:
   - Stop informers
   - Drain work queues
   - Exit 0
```

---

## Channel Reconciliation

### Trigger

K8s informer fires on Channel CR create/update/delete → key (`namespace/name`) enqueued to channelWorkQueue.

### Logic

```
channelReconcile(key):

  channel = GET Channel CR by key from API server
  
  ┌──────────────────────────────────────────────────────┐
  │ CASE 1: Channel CR was deleted (channel == nil)      │
  │──────────────────────────────────────────────────────│
  │ Nothing to do.                                       │
  │ ownerReferences on the Connector Deployment cause    │
  │ K8s garbage collector to delete it automatically.    │
  │                                                      │
  │ Optional: clean up /data/outbox/{channel-name}/      │
  │ (stale outbox dir). Not critical — Connector is gone │
  │ so nobody reads it.                                  │
  └──────────────────────────────────────────────────────┘

  ┌──────────────────────────────────────────────────────┐
  │ CASE 2: Channel CR exists (create or update)         │
  └──────────────────────────────────────────────────────┘

  # 1. Validate secretRef exists
  secret = GET Secret channel.spec.secretRef in channel.namespace
  IF secret == nil:
    UPDATE channel.status:
      phase: "Error"
      message: "Secret '{secretRef}' not found"
    RETURN (requeue after 30s — secret might be created later)

  # 2. Build desired Connector Deployment
  desired = {
    metadata:
      name:       "connector-" + channel.metadata.name
      namespace:  channel.metadata.namespace
      labels:
        app:           "kk-connector"
        channel:       channel.metadata.name
        channel-type:  channel.spec.type
      ownerReferences:
        - apiVersion:          "kk.io/v1alpha1"
          kind:                "Channel"
          name:                channel.metadata.name
          uid:                 channel.metadata.uid
          controller:          true
          blockOwnerDeletion:  true

    spec:
      replicas: 1
      selector:
        matchLabels:
          app:      "kk-connector"
          channel:  channel.metadata.name
      template:
        metadata:
          labels:
            app:           "kk-connector"
            channel:       channel.metadata.name
            channel-type:  channel.spec.type
        spec:
          containers:
          - name: connector
            image: $IMAGE_CONNECTOR
            env:
            - name:  CHANNEL_TYPE
              value: channel.spec.type
            - name:  CHANNEL_NAME
              value: channel.metadata.name
            - name:  INBOUND_DIR
              value: $DATA_DIR + "/inbound"
            - name:  OUTBOX_DIR
              value: $DATA_DIR + "/outbox/" + channel.metadata.name
            envFrom:
            - secretRef:
                name: channel.spec.secretRef
            volumeMounts:
            - name:      data
              mountPath: /data
          volumes:
          - name: data
            persistentVolumeClaim:
              claimName: kk-data
  }

  # Merge channel.spec.config into connector env if present
  IF channel.spec.config != nil:
    for key, value in channel.spec.config:
      add env var: CONFIG_{UPPER_SNAKE(key)} = JSON.stringify(value)
      # e.g. allowedChatIds → CONFIG_ALLOWED_CHAT_IDS = '["-1001234567890"]'

  # 3. Apply Deployment
  existing = GET Deployment "connector-" + channel.metadata.name
  IF existing == nil:
    CREATE Deployment desired
    log.Info("Created connector deployment", "channel", channel.metadata.name)
  ELSE:
    IF specChanged(existing, desired):
      UPDATE Deployment with desired spec
      log.Info("Updated connector deployment", "channel", channel.metadata.name)

  # 4. Ensure outbox directory exists
  mkdir -p $DATA_DIR/outbox/channel.metadata.name

  # 5. Update status based on Deployment readiness
  deployment = GET Deployment "connector-" + channel.metadata.name
  pods = LIST Pods with label channel=channel.metadata.name

  IF deployment.status.readyReplicas >= 1:
    UPDATE channel.status:
      phase: "Connected"
      connectorPod: pods[0].metadata.name
      message: ""

  ELSE IF pods exist AND pods[0].status has waiting reason "CrashLoopBackOff":
    UPDATE channel.status:
      phase: "Error"
      message: "Connector crashlooping: " + pods[0].status.containerStatuses[0].state.waiting.message

  ELSE:
    UPDATE channel.status:
      phase: "Pending"
      message: "Waiting for connector pod to be ready"
```

### Channel Spec Change Detection

The Controller must detect meaningful changes to avoid unnecessary Deployment updates:

```
specChanged(existing, desired):
  Compare:
    - container image
    - container env vars (CHANNEL_TYPE, CHANNEL_NAME, config vars)
    - secretRef name
    - labels
  Ignore:
    - status fields
    - resourceVersion
    - annotations not set by controller
```

---

## Skill Reconciliation

### Trigger

K8s informer fires on Skill CR create/update/delete → key (`namespace/name`) enqueued to skillWorkQueue.

### Logic

```
skillReconcile(key):

  skill = GET Skill CR by key from API server

  ┌──────────────────────────────────────────────────────┐
  │ CASE 1: Skill CR was deleted (skill == nil)          │
  └──────────────────────────────────────────────────────┘

  skillDir = $DATA_DIR + "/skills/" + extractNameFromKey(key)
  IF exists(skillDir):
    rm -rf skillDir
    log.Info("Removed skill from PVC", "skill", extractNameFromKey(key))
  RETURN

  ┌──────────────────────────────────────────────────────┐
  │ CASE 2: Skill CR exists (create or update)           │
  └──────────────────────────────────────────────────────┘

  source = skill.spec.source
  # Example: "vercel-labs/agent-skills/skills/frontend-design"

  # 1. Parse source
  parts = source.split("/")
  IF len(parts) < 3:
    UPDATE skill.status:
      phase: "Error"
      message: "Invalid source format. Expected: owner/repo/path/to/skill. Got: " + source
    RETURN

  owner    = parts[0]                    # "vercel-labs"
  repo     = parts[1]                    # "agent-skills"
  skillPath = join("/", parts[2:])       # "skills/frontend-design"
  cloneURL = "https://github.com/" + owner + "/" + repo + ".git"

  # 2. Update status → Fetching
  UPDATE skill.status:
    phase: "Fetching"
    message: "Cloning " + owner + "/" + repo

  # 3. Clone to temp dir
  tmpDir = mktemp("-d", "/tmp/skill-" + skill.metadata.name + "-XXXXXX")

  result = exec(
    "git", "clone",
    "--depth", "1",
    "--single-branch",
    "--filter=blob:none",      # treeless clone — faster for large repos
    cloneURL,
    tmpDir,
    timeout: SKILL_CLONE_TIMEOUT seconds
  )

  IF result.exitCode != 0:
    UPDATE skill.status:
      phase: "Error"
      message: "git clone failed: " + truncate(result.stderr, 200)
    rm -rf tmpDir
    RETURN

  # 4. Validate skill dir exists in cloned repo
  skillSrcDir = tmpDir + "/" + skillPath
  IF not isDirectory(skillSrcDir):
    UPDATE skill.status:
      phase: "Error"
      message: "Path '" + skillPath + "' not found in repo " + owner + "/" + repo
    rm -rf tmpDir
    RETURN

  # 5. Validate SKILL.md exists
  skillMdPath = skillSrcDir + "/SKILL.md"
  IF not exists(skillMdPath):
    UPDATE skill.status:
      phase: "Error"
      message: "SKILL.md not found at " + skillPath + "/SKILL.md"
    rm -rf tmpDir
    RETURN

  # 6. Parse SKILL.md frontmatter
  content = readFile(skillMdPath)
  frontmatter = parseYamlFrontmatter(content)
  #   Expects:
  #   ---
  #   name: frontend-design
  #   description: React and Next.js performance optimization guidelines
  #   ---

  IF not frontmatter.name:
    UPDATE skill.status:
      phase: "Error"
      message: "SKILL.md missing required 'name' field in YAML frontmatter"
    rm -rf tmpDir
    RETURN

  IF not frontmatter.description:
    UPDATE skill.status:
      phase: "Error"
      message: "SKILL.md missing required 'description' field in YAML frontmatter"
    rm -rf tmpDir
    RETURN

  # 7. Copy to PVC (atomic-ish: remove old, copy new)
  destDir = $DATA_DIR + "/skills/" + skill.metadata.name
  rm -rf destDir
  cp -r skillSrcDir destDir

  # 8. Set permissions
  chmod -R a+rX destDir                  # readable by all (Agent Jobs run as different user)
  IF isDirectory(destDir + "/scripts"):
    find destDir + "/scripts" -type f -exec chmod a+x {} \;   # scripts must be executable

  # 9. Update status → Ready
  UPDATE skill.status:
    phase:            "Ready"
    skillName:        frontmatter.name
    skillDescription: truncate(frontmatter.description, 256)
    lastFetched:      now().toISO8601()
    message:          ""

  # 10. Cleanup temp dir
  rm -rf tmpDir
  log.Info("Skill installed",
    "skill", skill.metadata.name,
    "name", frontmatter.name,
    "source", source)
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

Parsing logic:

```
parseYamlFrontmatter(content):
  IF not content.startsWith("---"):
    RETURN {}
  
  endIndex = content.indexOf("---", 3)   # find closing ---
  IF endIndex == -1:
    RETURN {}
  
  yamlStr = content.substring(3, endIndex).trim()
  RETURN yaml.parse(yamlStr)             # returns { name, description, ... }
```

---

## Error Handling

### Channel Errors

| Error | Detection | Status Update | Recovery |
|---|---|---|---|
| Secret not found | GET Secret returns 404 | phase=Error, message="Secret not found" | Requeue after 30s (secret may be created later) |
| Deployment create fails | K8s API error | phase=Error, message=API error | Requeue with exponential backoff |
| Connector crashlooping | Pod status check | phase=Error, message=crash reason | Requeue after 60s (auto-heals if transient) |
| Invalid channel type | Deployment created but connector exits | phase=Error via pod status | User must fix CR spec |

### Skill Errors

| Error | Detection | Status Update | Recovery |
|---|---|---|---|
| Invalid source format | String parsing | phase=Error, message="Invalid source format..." | User must fix CR spec |
| Git clone fails (404) | git exit code + stderr | phase=Error, message="git clone failed: ..." | User deletes + recreates CR |
| Git clone fails (network) | git exit code + stderr | phase=Error, message=stderr | User deletes + recreates CR |
| Git clone timeout | Process killed after SKILL_CLONE_TIMEOUT | phase=Error, message="Clone timed out" | User deletes + recreates CR |
| Path not found in repo | Directory check | phase=Error, message="Path not found..." | User must fix source path |
| SKILL.md missing | File check | phase=Error, message="SKILL.md not found..." | User must fix source path |
| Frontmatter invalid | YAML parse | phase=Error, message="Missing name/description" | Upstream skill must be fixed |
| PVC write fails | cp/chmod error | phase=Error, message=OS error | Check PVC capacity/permissions |

### Requeue Strategy

| CRD | On success | On transient error | On permanent error |
|---|---|---|---|
| Channel | No requeue (watches for changes) | Requeue after 30s with exponential backoff (max 5m) | No requeue — user must fix CR |
| Skill | No requeue | No requeue — user deletes + recreates | No requeue — user must fix CR |

Skills do NOT requeue on failure because the recovery path is always delete + recreate. This avoids the Controller repeatedly cloning a repo that doesn't exist.

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

# Watch and update Skill CRs
- apiGroups: ["kk.io"]
  resources: ["skills", "skills/status"]
  verbs: ["get", "list", "watch", "update", "patch"]

# Manage Connector Deployments (child of Channel CRs)
- apiGroups: ["apps"]
  resources: ["deployments"]
  verbs: ["create", "get", "list", "watch", "update", "delete"]

# Read Pods (for Connector readiness checks)
- apiGroups: [""]
  resources: ["pods"]
  verbs: ["get", "list"]

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
        - name: RECONCILE_WORKERS
          value: "2"
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
| `GET /healthz` | Liveness | Process is running, not deadlocked |
| `GET /readyz` | Readiness | Informer caches are synced, work queues are processing |

---

## Logging

Structured JSON logs (for easy parsing by log aggregators):

```jsonc
// Startup
{"level":"info","msg":"Starting controller","version":"v0.1.0","namespace":"kk"}
{"level":"info","msg":"CRDs verified","channels":true,"skills":true}
{"level":"info","msg":"Informer caches synced","duration_ms":1200}

// Channel reconciliation
{"level":"info","msg":"Reconciling channel","channel":"telegram-bot-1","event":"create"}
{"level":"info","msg":"Created connector deployment","channel":"telegram-bot-1","deployment":"connector-telegram-bot-1"}
{"level":"info","msg":"Channel connected","channel":"telegram-bot-1","pod":"connector-telegram-bot-1-abc12"}

// Skill reconciliation
{"level":"info","msg":"Reconciling skill","skill":"frontend-design","source":"vercel-labs/agent-skills/skills/frontend-design"}
{"level":"info","msg":"Cloning repo","skill":"frontend-design","repo":"vercel-labs/agent-skills"}
{"level":"info","msg":"Skill installed","skill":"frontend-design","name":"frontend-design","description":"React and Next.js..."}

// Errors
{"level":"error","msg":"git clone failed","skill":"bad-skill","error":"repository not found","stderr":"..."}
{"level":"error","msg":"Secret not found","channel":"slack-eng","secretRef":"slack-eng-creds"}
```

---

## Testing Checklist

### Channel Tests
- [ ] Create Channel CR → Connector Deployment created with correct env, labels, ownerRef
- [ ] Delete Channel CR → Connector Deployment garbage collected
- [ ] Update Channel CR secretRef → Connector Deployment updated
- [ ] Channel CR with non-existent Secret → status=Error
- [ ] Connector pod crashlooping → Channel status=Error with reason
- [ ] Connector pod becomes ready → Channel status=Connected

### Skill Tests
- [ ] Create Skill CR with valid source → files appear at `/data/skills/{name}/`
- [ ] SKILL.md frontmatter parsed → status shows skillName + skillDescription
- [ ] Delete Skill CR → `/data/skills/{name}/` removed
- [ ] Invalid source format (< 3 segments) → status=Error
- [ ] Source points to non-existent repo → status=Error with git stderr
- [ ] Source points to non-existent path in valid repo → status=Error
- [ ] SKILL.md missing from path → status=Error
- [ ] SKILL.md missing frontmatter name → status=Error
- [ ] SKILL.md missing frontmatter description → status=Error
- [ ] Scripts in skill dir → executable permissions set
- [ ] Two Skill CRs from same repo → both installed independently
- [ ] Git clone timeout → status=Error with timeout message
