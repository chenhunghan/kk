# KubeClaw: Plan — Gateway

> **Cross-references:**
> [Communication Protocol](protocol.md) — message formats §4.1–§4.6; nq conventions §2; polling intervals §7; group registry §6
> [Plan: Controller](plan-controller.md) ·
> [Plan: Connector](plan-connector.md) — writes inbound, reads outbox
> [Plan: Agent Job](plan-agent-job.md) — created by Gateway, writes results
> [Plan: Skill](plan-skill.md)

---

## Summary

The Gateway is a single-replica Deployment. It is the **routing brain** of KubeClaw. It reads messages from the inbound queue, decides whether to act, creates Agent Jobs (cold path) or forwards to running Jobs (hot path), reads completed results, and writes responses to the outbox.

The Gateway runs three concurrent loops:

| Loop | Polls | Interval | Purpose |
|---|---|---|---|
| **Inbound loop** | `/data/inbound/` | 2s | Route messages → create Jobs or enqueue follow-ups |
| **Results loop** | `/data/results/*/status` | 2s | Read completed results → write to outbox |
| **Cleanup loop** | K8s Jobs API | 60s | Delete finished Jobs, handle stuck results |

The Gateway has **no knowledge** of Skills, platform SDKs, or LLM APIs. It only knows about queues, groups, triggers, and K8s Jobs.

---

## Image

```
kubeclaw-gateway:latest

Base:    node:20-alpine (or Go binary)
Runtime: K8s client library + file I/O
Size:    ~80MB
```

---

## Environment Variables

| Var | Default | Description |
|---|---|---|
| `DATA_DIR` | `/data` | PVC mount path |
| `NAMESPACE` | Auto-detected (in-cluster) | Namespace for creating Jobs |
| `IMAGE_AGENT` | `kubeclaw-agent:latest` | Image used for Agent Job pods |
| `API_KEYS_SECRET` | `kubeclaw-api-keys` | Secret name with LLM API keys |
| `JOB_ACTIVE_DEADLINE` | `300` | Max seconds an Agent Job can run |
| `JOB_TTL_AFTER_FINISHED` | `300` | Seconds before K8s auto-deletes completed Job |
| `JOB_IDLE_TIMEOUT` | `120` | Passed to Agent Job as IDLE_TIMEOUT |
| `JOB_MAX_TURNS` | `25` | Passed to Agent Job as MAX_TURNS |
| `JOB_CPU_REQUEST` | `250m` | Agent Job CPU request |
| `JOB_CPU_LIMIT` | `1` | Agent Job CPU limit |
| `JOB_MEMORY_REQUEST` | `256Mi` | Agent Job memory request |
| `JOB_MEMORY_LIMIT` | `1Gi` | Agent Job memory limit |
| `INBOUND_POLL_INTERVAL` | `2000` | ms between inbound queue polls |
| `RESULTS_POLL_INTERVAL` | `2000` | ms between results dir scans |
| `CLEANUP_INTERVAL` | `60000` | ms between Job cleanup sweeps |
| `STALE_MESSAGE_TIMEOUT` | `300` | seconds before inbound message considered stale |
| `RESULTS_ARCHIVE_TTL` | `86400` | seconds before archived results are deleted |

---

## Startup Sequence

```
1. Read env vars, validate required ones
2. Build K8s client (in-cluster config via ServiceAccount)
3. Ensure directories exist:
   mkdir -p $DATA_DIR/inbound
   mkdir -p $DATA_DIR/outbox
   mkdir -p $DATA_DIR/groups
   mkdir -p $DATA_DIR/results
   mkdir -p $DATA_DIR/results/.done
   mkdir -p $DATA_DIR/state
4. Load state files:
   a. groupsConfig = loadJsonFile($DATA_DIR/state/groups.json) || { groups: {} }
   b. cursors = loadJsonFile($DATA_DIR/state/cursors.json) || {}
5. Build in-memory indexes:
   a. activeJobs = {}    # group → { jobName, sessionId, createdAt }
   b. Sync with K8s: LIST Jobs with label app=kubeclaw-agent
      → populate activeJobs for any currently running Jobs
6. Start concurrent loops:
   a. inboundLoop()
   b. resultsLoop()
   c. cleanupLoop()
   d. stateReloadLoop()   # reload groups.json every 30s
7. Start health server on port 8082
8. Register SIGTERM handler:
   - Stop all loops
   - Save cursors to disk
   - Exit 0
```

---

## In-Memory State

The Gateway maintains in-memory state (not persisted beyond what's on disk):

```
activeJobs: Map<groupName, {
  jobName:    string,     // "agent-family-chat-1708801290"
  sessionId:  string,     // "family-chat-1708801290"
  createdAt:  number,     // Unix timestamp
}>

groupsConfig: {           // loaded from /data/state/groups.json (Protocol §6)
  groups: {
    [groupName]: {
      trigger_pattern: string | null,
      trigger_mode:    "mention" | "always" | "prefix",
      channels: { ... }
    }
  }
}
```

`activeJobs` is rebuilt on startup by listing K8s Jobs and is kept in sync by the inbound loop (on Job creation) and the results/cleanup loops (on Job completion/deletion).

---

## Inbound Loop

### Overview

```
/data/inbound/,{ts}.{id}.nq
  │
  │  Gateway polls every 2s (Protocol §7)
  ▼
Gateway: inboundLoop()
  │
  ├── Trigger match? NO → skip (delete file)
  │
  ├── Job running for group? YES → HOT PATH
  │   └── Write to /data/groups/{group}/,{ts}.{id}.nq
  │
  └── Job running for group? NO → COLD PATH
      ├── Write /data/results/{session-id}/request.json
      └── Create K8s Job: agent-{session-id}
```

### Detailed Logic

```
inboundLoop():
  every INBOUND_POLL_INTERVAL ms:

    # 1. List pending inbound messages
    files = listNqFiles(DATA_DIR + "/inbound")     # Protocol §2: ,*.nq glob, sorted
    IF files.length === 0: CONTINUE

    for file in files:
      filePath = DATA_DIR + "/inbound/" + file

      try:
        # 2. Read and parse
        content = readFileSync(filePath, "utf8")
        msg = JSON.parse(content)

        # 3. Validate (Protocol §4.1 validation rules)
        IF not isValidInboundMessage(msg):
          log.warn("Invalid inbound message, discarding", { file, reason: validationError })
          unlinkSync(filePath)
          CONTINUE

        # 4. Check stale
        IF (now() - msg.timestamp) > STALE_MESSAGE_TIMEOUT:
          log.warn("Stale inbound message, discarding", { file, age: now() - msg.timestamp })
          unlinkSync(filePath)
          CONTINUE

        # 5. Route the message
        routeMessage(msg)

        # 6. Delete processed file
        unlinkSync(filePath)

      catch error:
        log.error("Failed to process inbound message", { file, error: error.message })
        # Check if file is corrupt JSON — if so, discard
        IF error instanceof SyntaxError:
          unlinkSync(filePath)
        # Otherwise leave for retry on next poll
```

### Message Routing

```
routeMessage(msg):

  group = msg.group
  groupConfig = groupsConfig.groups[group]

  # ──────────────────────────────────────────
  # STEP 1: Trigger check
  # ──────────────────────────────────────────

  IF groupConfig == null:
    # Unknown group — not registered in groups.json
    # Option: auto-register with trigger_mode="mention", or skip
    log.info("Message from unregistered group, skipping", { group })
    RETURN

  shouldTrigger = checkTrigger(msg.text, groupConfig)
  IF not shouldTrigger:
    log.debug("Message did not match trigger, skipping", {
      group,
      trigger_mode: groupConfig.trigger_mode,
      trigger_pattern: groupConfig.trigger_pattern
    })
    RETURN

  # ──────────────────────────────────────────
  # STEP 2: Hot path vs cold path
  # ──────────────────────────────────────────

  IF group in activeJobs:
    # HOT PATH — Job already running for this group
    hotPath(msg, group)
  ELSE:
    # COLD PATH — create new Job
    coldPath(msg, group)
```

### Trigger Checking

```
checkTrigger(text, groupConfig):
  switch groupConfig.trigger_mode:

    case "always":
      RETURN true

    case "mention":
      # Check if trigger_pattern appears anywhere in the text
      pattern = groupConfig.trigger_pattern      # e.g. "@Andy"
      RETURN text.includes(pattern)
      # Case-insensitive variant:
      # RETURN text.toLowerCase().includes(pattern.toLowerCase())

    case "prefix":
      # Check if text starts with trigger_pattern
      pattern = groupConfig.trigger_pattern      # e.g. "/ask"
      RETURN text.startsWith(pattern)

    default:
      log.warn("Unknown trigger_mode", { mode: groupConfig.trigger_mode })
      RETURN false
```

### Cold Path: Create New Job

```
coldPath(msg, group):
  timestamp = msg.timestamp
  sessionId = group + "-" + timestamp                # Protocol §5.1
  jobName   = "agent-" + sessionId

  # Sanitize jobName for K8s (max 63 chars, lowercase, alphanumeric + hyphens)
  jobName = sanitizeK8sName(jobName)

  log.info("COLD PATH: creating Agent Job", { group, sessionId, jobName })

  # ──────────────────────────────────────────
  # 1. Create results directory and request.json
  # ──────────────────────────────────────────
  resultsDir = DATA_DIR + "/results/" + sessionId
  mkdirSync(resultsDir, { recursive: true })

  requestManifest = {                                # Protocol §4.4
    channel:       msg.channel,
    group:         msg.group,
    sender:        msg.sender,
    platform_meta: msg.platform_meta,
    messages: [
      {
        sender: msg.sender,
        text:   msg.text,
        ts:     msg.timestamp
      }
    ]
  }
  writeFileSync(resultsDir + "/request.json", JSON.stringify(requestManifest, null, 2))

  # ──────────────────────────────────────────
  # 2. Ensure per-group queue directory exists
  # ──────────────────────────────────────────
  mkdirSync(DATA_DIR + "/groups/" + group, { recursive: true })

  # ──────────────────────────────────────────
  # 3. Strip trigger pattern from prompt text
  # ──────────────────────────────────────────
  promptText = stripTrigger(msg.text, groupsConfig.groups[group])
  # e.g. "@Andy what's the weather?" → "what's the weather?"

  # ──────────────────────────────────────────
  # 4. Create K8s Job
  # ──────────────────────────────────────────
  jobSpec = buildJobSpec(jobName, sessionId, group, promptText, msg.channel)
  await k8sClient.createNamespacedJob(NAMESPACE, jobSpec)

  # ──────────────────────────────────────────
  # 5. Update in-memory state
  # ──────────────────────────────────────────
  activeJobs[group] = {
    jobName:   jobName,
    sessionId: sessionId,
    createdAt: now()
  }

  log.info("Agent Job created", { jobName, group, sessionId })
```

### Job Spec Builder

```
buildJobSpec(jobName, sessionId, group, promptText, channel):
  RETURN {
    apiVersion: "batch/v1",
    kind: "Job",
    metadata: {
      name: jobName,
      namespace: NAMESPACE,
      labels: {
        app:     "kubeclaw-agent",
        group:   group,
        channel: channel
      }
    },
    spec: {
      ttlSecondsAfterFinished: parseInt(JOB_TTL_AFTER_FINISHED),
      activeDeadlineSeconds:   parseInt(JOB_ACTIVE_DEADLINE),
      backoffLimit: 1,
      template: {
        metadata: {
          labels: {
            app:   "kubeclaw-agent",
            group: group
          }
        },
        spec: {
          restartPolicy: "Never",
          containers: [{
            name:  "agent",
            image: IMAGE_AGENT,
            env: [
              { name: "PROMPT",       value: promptText },
              { name: "GROUP",        value: group },
              { name: "SESSION_ID",   value: sessionId },
              { name: "IDLE_TIMEOUT", value: String(JOB_IDLE_TIMEOUT) },
              { name: "MAX_TURNS",    value: String(JOB_MAX_TURNS) },
              { name: "DATA_DIR",     value: "/data" }
            ],
            envFrom: [
              { secretRef: { name: API_KEYS_SECRET } }
            ],
            volumeMounts: [
              { name: "data", mountPath: "/data" }
            ],
            resources: {
              requests: {
                cpu:    JOB_CPU_REQUEST,
                memory: JOB_MEMORY_REQUEST
              },
              limits: {
                cpu:    JOB_CPU_LIMIT,
                memory: JOB_MEMORY_LIMIT
              }
            }
          }],
          volumes: [{
            name: "data",
            persistentVolumeClaim: {
              claimName: "kubeclaw-data"
            }
          }]
        }
      }
    }
  }
```

### Hot Path: Forward to Running Job

```
hotPath(msg, group):
  jobInfo = activeJobs[group]

  log.info("HOT PATH: forwarding to running Job", {
    group,
    jobName: jobInfo.jobName,
    sessionId: jobInfo.sessionId
  })

  # ──────────────────────────────────────────
  # 1. Build follow-up message (Protocol §4.3)
  # ──────────────────────────────────────────
  followUp = {
    sender:        msg.sender,
    text:          stripTrigger(msg.text, groupsConfig.groups[group]),
    timestamp:     msg.timestamp,
    channel:       msg.channel,
    platform_meta: msg.platform_meta
  }

  # ──────────────────────────────────────────
  # 2. Atomic write to per-group queue
  # ──────────────────────────────────────────
  groupQueueDir = DATA_DIR + "/groups/" + group
  json = JSON.stringify(followUp)
  id   = randomHex(6)
  tmp  = groupQueueDir + "/.tmp-" + id
  nq   = groupQueueDir + "/," + msg.timestamp + "." + id + ".nq"

  writeFileSync(tmp, json)
  renameSync(tmp, nq)

  # ──────────────────────────────────────────
  # 3. Append to request.json messages array
  # ──────────────────────────────────────────
  requestPath = DATA_DIR + "/results/" + jobInfo.sessionId + "/request.json"
  IF exists(requestPath):
    request = JSON.parse(readFileSync(requestPath))
    request.messages.push({
      sender: msg.sender,
      text:   msg.text,
      ts:     msg.timestamp
    })
    writeFileSync(requestPath, JSON.stringify(request, null, 2))

  log.info("Follow-up enqueued to group queue", { group, file: nq })
```

### Trigger Stripping

```
stripTrigger(text, groupConfig):
  IF groupConfig == null or groupConfig.trigger_pattern == null:
    RETURN text.trim()

  switch groupConfig.trigger_mode:
    case "mention":
      # Remove the mention pattern from anywhere in text
      RETURN text.replace(groupConfig.trigger_pattern, "").trim()
      # "@Andy what's the weather?" → "what's the weather?"

    case "prefix":
      # Remove the prefix
      IF text.startsWith(groupConfig.trigger_pattern):
        RETURN text.substring(groupConfig.trigger_pattern.length).trim()
      RETURN text.trim()

    case "always":
      RETURN text.trim()
```

---

## Results Loop

### Overview

```
/data/results/{session-id}/status  → "done"
/data/results/{session-id}/response.jsonl
/data/results/{session-id}/request.json
  │
  │  Gateway polls every 2s (Protocol §7)
  ▼
Gateway: resultsLoop()
  │
  │  1. Find results dirs where status = "done" or "error"
  │  2. Extract response text from response.jsonl (Protocol §4.6)
  │  3. Read routing info from request.json (Protocol §4.4)
  │  4. Build OutboundMessage (Protocol §4.2)
  │  5. Write to /data/outbox/{channel}/
  │  6. Archive results dir to /data/results/.done/
  │  7. Remove group from activeJobs
  │
  ▼
/data/outbox/{channel}/,{ts}.{id}.nq
```

### Detailed Logic

```
resultsLoop():
  every RESULTS_POLL_INTERVAL ms:

    # 1. Scan results directories
    resultDirs = readdirSync(DATA_DIR + "/results")
      .filter(d => d !== ".done" && isDirectory(DATA_DIR + "/results/" + d))

    for dirName in resultDirs:
      resultDir  = DATA_DIR + "/results/" + dirName
      statusPath = resultDir + "/status"

      # 2. Check status file
      IF not exists(statusPath):
        CONTINUE                         # Job hasn't started writing yet

      status = readFileSync(statusPath, "utf8").trim()

      IF status === "running":
        CONTINUE                         # Job still active

      IF status === "done":
        processCompletedResult(dirName, resultDir)

      ELSE IF status === "error":
        processErrorResult(dirName, resultDir)

      ELSE:
        log.warn("Unknown result status", { dir: dirName, status })
```

### Processing Completed Results

```
processCompletedResult(dirName, resultDir):

  # 1. Read request.json for routing info
  requestPath = resultDir + "/request.json"
  IF not exists(requestPath):
    log.error("request.json missing for completed result", { dir: dirName })
    archiveResult(dirName)
    RETURN

  request = JSON.parse(readFileSync(requestPath, "utf8"))

  # 2. Extract response text from response.jsonl (Protocol §4.6)
  responsePath = resultDir + "/response.jsonl"
  IF not exists(responsePath):
    log.error("response.jsonl missing for completed result", { dir: dirName })
    archiveResult(dirName)
    RETURN

  responseText = extractResponseText(responsePath)
  IF not responseText:
    log.warn("Empty response from agent", { dir: dirName })
    responseText = "(Agent returned empty response)"

  # 3. Build outbound message (Protocol §4.2)
  outbound = {
    channel:       request.channel,
    group:         request.group,
    text:          responseText,
    platform_meta: buildReplyMeta(request.platform_meta)
  }

  # 4. Write to outbox
  outboxDir = DATA_DIR + "/outbox/" + request.channel
  mkdirSync(outboxDir, { recursive: true })

  id  = randomHex(6)
  tmp = outboxDir + "/.tmp-" + id
  nq  = outboxDir + "/," + now() + "." + id + ".nq"

  writeFileSync(tmp, JSON.stringify(outbound))
  renameSync(tmp, nq)

  log.info("Response routed to outbox", {
    group: request.group,
    channel: request.channel,
    responseLength: responseText.length
  })

  # 5. Archive and clean up
  archiveResult(dirName)
  removeFromActiveJobs(request.group)
```

### Extracting Response Text from JSONL

```
extractResponseText(jsonlPath):
  lines = readFileSync(jsonlPath, "utf8")
    .split("\n")
    .filter(l => l.trim().length > 0)

  # Strategy: look for the last "result" type line (Protocol §4.6)
  for i = lines.length - 1; i >= 0; i--:
    try:
      obj = JSON.parse(lines[i])
      IF obj.type === "result" AND obj.result:
        RETURN obj.result
    catch:
      CONTINUE

  # Fallback: look for last assistant text content
  for i = lines.length - 1; i >= 0; i--:
    try:
      obj = JSON.parse(lines[i])
      IF obj.type === "assistant" AND obj.message?.content:
        texts = obj.message.content
          .filter(c => c.type === "text")
          .map(c => c.text)
        IF texts.length > 0:
          RETURN texts.join("\n")
    catch:
      CONTINUE

  RETURN null    # no response found
```

### Building Reply Metadata

```
buildReplyMeta(originalMeta):
  # Copy platform_meta and add reply threading hints
  meta = { ...originalMeta }

  # Telegram: reply to the original message
  IF meta.message_id:
    meta.reply_to_message_id = meta.message_id

  # Slack: reply in thread
  IF meta.ts AND not meta.thread_ts:
    meta.thread_ts = meta.ts

  RETURN meta
```

### Processing Error Results

```
processErrorResult(dirName, resultDir):
  requestPath = resultDir + "/request.json"
  IF not exists(requestPath):
    log.error("request.json missing for error result", { dir: dirName })
    archiveResult(dirName)
    RETURN

  request = JSON.parse(readFileSync(requestPath, "utf8"))

  # Check if there's a partial response
  responsePath = resultDir + "/response.jsonl"
  errorText = "(Agent encountered an error and could not complete the request.)"
  IF exists(responsePath):
    partial = extractResponseText(responsePath)
    IF partial:
      errorText = partial + "\n\n⚠️ (Agent encountered an error before finishing.)"

  # Check agent.log for error details
  logPath = resultDir + "/agent.log"
  IF exists(logPath):
    agentLog = readFileSync(logPath, "utf8")
    log.error("Agent error details", { dir: dirName, log: truncate(agentLog, 500) })

  # Route error message to outbox
  outbound = {
    channel:       request.channel,
    group:         request.group,
    text:          errorText,
    platform_meta: buildReplyMeta(request.platform_meta)
  }

  outboxDir = DATA_DIR + "/outbox/" + request.channel
  mkdirSync(outboxDir, { recursive: true })

  id  = randomHex(6)
  tmp = outboxDir + "/.tmp-" + id
  nq  = outboxDir + "/," + now() + "." + id + ".nq"
  writeFileSync(tmp, JSON.stringify(outbound))
  renameSync(tmp, nq)

  log.warn("Error result routed to outbox", { group: request.group, dir: dirName })

  archiveResult(dirName)
  removeFromActiveJobs(request.group)
```

---

## Cleanup Loop

### Detailed Logic

```
cleanupLoop():
  every CLEANUP_INTERVAL ms:

    # ──────────────────────────────────────────
    # TASK 1: Clean up completed K8s Jobs
    # ──────────────────────────────────────────
    jobs = await k8sClient.listNamespacedJob(NAMESPACE, {
      labelSelector: "app=kubeclaw-agent"
    })

    for job in jobs.items:
      # Check if Job is finished (succeeded or failed)
      IF jobIsFinished(job):
        age = now() - job.status.completionTime
        IF age > JOB_TTL_AFTER_FINISHED:
          await k8sClient.deleteNamespacedJob(NAMESPACE, job.metadata.name, {
            propagationPolicy: "Background"    # also delete pod
          })
          log.info("Deleted completed Job", { job: job.metadata.name, age })

    # ──────────────────────────────────────────
    # TASK 2: Detect crashed Jobs (in activeJobs but K8s Job gone)
    # ──────────────────────────────────────────
    for group, jobInfo in activeJobs:
      try:
        job = await k8sClient.readNamespacedJob(NAMESPACE, jobInfo.jobName)
        IF jobIsFailed(job):
          log.warn("Active Job failed", { group, job: jobInfo.jobName })
          # Write error status so results loop picks it up
          statusPath = DATA_DIR + "/results/" + jobInfo.sessionId + "/status"
          IF exists(statusPath):
            currentStatus = readFileSync(statusPath, "utf8").trim()
            IF currentStatus === "running":
              writeFileSync(statusPath, "error")
          removeFromActiveJobs(group)
      catch error:
        IF error.statusCode === 404:
          # Job was deleted (manually or by TTL) — clean up
          log.warn("Active Job disappeared from K8s", { group, job: jobInfo.jobName })
          statusPath = DATA_DIR + "/results/" + jobInfo.sessionId + "/status"
          IF not exists(statusPath) OR readFileSync(statusPath, "utf8").trim() === "running":
            writeFileSync(statusPath, "error")
          removeFromActiveJobs(group)

    # ──────────────────────────────────────────
    # TASK 3: Handle orphaned group queue files
    # ──────────────────────────────────────────
    groupDirs = readdirSync(DATA_DIR + "/groups")
    for groupDir in groupDirs:
      IF groupDir NOT in activeJobs:
        pendingFiles = listNqFiles(DATA_DIR + "/groups/" + groupDir)
        IF pendingFiles.length > 0:
          log.warn("Orphaned follow-up messages found", {
            group: groupDir,
            count: pendingFiles.length
          })
          # Option A: Create a new Job to process them
          # Option B: Discard them (they're stale)
          # Current choice: discard if older than STALE_MESSAGE_TIMEOUT
          for file in pendingFiles:
            filePath = DATA_DIR + "/groups/" + groupDir + "/" + file
            IF fileAge(filePath) > STALE_MESSAGE_TIMEOUT:
              unlinkSync(filePath)
              log.info("Discarded stale follow-up", { group: groupDir, file })

    # ──────────────────────────────────────────
    # TASK 4: Archive cleanup
    # ──────────────────────────────────────────
    doneDir = DATA_DIR + "/results/.done"
    IF exists(doneDir):
      archivedDirs = readdirSync(doneDir)
      for dir in archivedDirs:
        dirPath = doneDir + "/" + dir
        IF directoryAge(dirPath) > RESULTS_ARCHIVE_TTL:
          rmRfSync(dirPath)
          log.debug("Purged archived result", { dir })
```

### Helper: Job Status

```
jobIsFinished(job):
  RETURN job.status.succeeded >= 1 OR job.status.failed >= 1

jobIsFailed(job):
  RETURN job.status.failed >= 1
    OR (job.status.conditions || []).some(c =>
         c.type === "Failed" AND c.status === "True"
       )
```

---

## State Management

### activeJobs

```
removeFromActiveJobs(group):
  IF group in activeJobs:
    log.debug("Removed from activeJobs", { group, job: activeJobs[group].jobName })
    delete activeJobs[group]

# Rebuild on startup
rebuildActiveJobs():
  jobs = await k8sClient.listNamespacedJob(NAMESPACE, {
    labelSelector: "app=kubeclaw-agent"
  })
  for job in jobs.items:
    IF not jobIsFinished(job):
      group = job.metadata.labels.group
      activeJobs[group] = {
        jobName:   job.metadata.name,
        sessionId: extractSessionId(job),    # from env var or label
        createdAt: Date.parse(job.metadata.creationTimestamp)
      }
  log.info("Rebuilt activeJobs from K8s", { count: Object.keys(activeJobs).length })
```

### groups.json Reload

```
stateReloadLoop():
  every 30000 ms:
    try:
      newConfig = JSON.parse(readFileSync(DATA_DIR + "/state/groups.json", "utf8"))
      groupsConfig = newConfig
      log.debug("Reloaded groups.json", { groups: Object.keys(newConfig.groups).length })
    catch error:
      log.warn("Failed to reload groups.json", { error: error.message })
      # Keep using previous config
```

### Archive Helper

```
archiveResult(dirName):
  src  = DATA_DIR + "/results/" + dirName
  dest = DATA_DIR + "/results/.done/" + dirName
  renameSync(src, dest)
  log.debug("Archived result", { dir: dirName })
```

---

## K8s Name Sanitization

```
sanitizeK8sName(name):
  result = name
    .toLowerCase()
    .replace(/[^a-z0-9-]/g, "-")       # replace invalid chars with hyphens
    .replace(/-+/g, "-")               # collapse multiple hyphens
    .replace(/^-|-$/g, "")             # trim leading/trailing hyphens
  IF result.length > 63:
    # K8s name limit is 63 chars
    # Keep prefix + hash of full name
    hash = sha256(name).substring(0, 8)
    result = result.substring(0, 54) + "-" + hash
  RETURN result
```

---

## RBAC

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: kubeclaw-gateway
  namespace: kubeclaw
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: kubeclaw-gateway
  namespace: kubeclaw
rules:
# Create and manage Agent Jobs
- apiGroups: ["batch"]
  resources: ["jobs"]
  verbs: ["create", "get", "list", "watch", "delete"]

# Check Job pod status
- apiGroups: [""]
  resources: ["pods"]
  verbs: ["get", "list", "watch"]

# Events (optional: for recording events)
- apiGroups: [""]
  resources: ["events"]
  verbs: ["create", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: kubeclaw-gateway
  namespace: kubeclaw
subjects:
- kind: ServiceAccount
  name: kubeclaw-gateway
roleRef:
  kind: Role
  name: kubeclaw-gateway
  apiGroup: rbac.authorization.k8s.io
```

---

## Deployment Manifest

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: kubeclaw-gateway
  namespace: kubeclaw
  labels:
    app: kubeclaw-gateway
spec:
  replicas: 1
  selector:
    matchLabels:
      app: kubeclaw-gateway
  template:
    metadata:
      labels:
        app: kubeclaw-gateway
    spec:
      serviceAccountName: kubeclaw-gateway
      containers:
      - name: gateway
        image: kubeclaw-gateway:latest
        env:
        - name: DATA_DIR
          value: /data
        - name: IMAGE_AGENT
          value: kubeclaw-agent:latest
        - name: API_KEYS_SECRET
          value: kubeclaw-api-keys
        - name: JOB_ACTIVE_DEADLINE
          value: "300"
        - name: JOB_TTL_AFTER_FINISHED
          value: "300"
        - name: JOB_IDLE_TIMEOUT
          value: "120"
        - name: JOB_MAX_TURNS
          value: "25"
        - name: JOB_CPU_REQUEST
          value: "250m"
        - name: JOB_CPU_LIMIT
          value: "1"
        - name: JOB_MEMORY_REQUEST
          value: "256Mi"
        - name: JOB_MEMORY_LIMIT
          value: "1Gi"
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
            port: 8082
          initialDelaySeconds: 10
          periodSeconds: 20
        readinessProbe:
          httpGet:
            path: /readyz
            port: 8082
          initialDelaySeconds: 5
          periodSeconds: 10
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: kubeclaw-data
```

---

## Health Endpoints

Port 8082 (internal only):

| Endpoint | Returns 200 when |
|---|---|
| `GET /healthz` | Process running, loops not deadlocked |
| `GET /readyz` | All three loops have completed at least one cycle, K8s client is connected |
| `GET /status` | JSON with `{ activeJobs, groupCount, inboundPending, uptimeSeconds }` — for debugging |

---

## Logging

```jsonc
// Startup
{"level":"info","msg":"Gateway starting","namespace":"kubeclaw","imageAgent":"kubeclaw-agent:latest"}
{"level":"info","msg":"Rebuilt activeJobs from K8s","count":1}
{"level":"info","msg":"Loaded groups.json","groups":3}

// Inbound — cold path
{"level":"info","msg":"COLD PATH: creating Agent Job","group":"family-chat","sessionId":"family-chat-1708801290","jobName":"agent-family-chat-1708801290"}
{"level":"info","msg":"Agent Job created","jobName":"agent-family-chat-1708801290","group":"family-chat"}

// Inbound — hot path
{"level":"info","msg":"HOT PATH: forwarding to running Job","group":"family-chat","jobName":"agent-family-chat-1708801290"}
{"level":"info","msg":"Follow-up enqueued to group queue","group":"family-chat"}

// Inbound — skip
{"level":"debug","msg":"Message did not match trigger, skipping","group":"work-team","trigger_mode":"mention","trigger_pattern":"@Andy"}

// Results
{"level":"info","msg":"Response routed to outbox","group":"family-chat","channel":"telegram-bot-1","responseLength":142}
{"level":"warn","msg":"Error result routed to outbox","group":"family-chat","dir":"family-chat-1708801290"}

// Cleanup
{"level":"info","msg":"Deleted completed Job","job":"agent-family-chat-1708801290","age":310}
{"level":"warn","msg":"Active Job disappeared from K8s","group":"work-team","job":"agent-work-team-1708801305"}
{"level":"info","msg":"Discarded stale follow-up","group":"work-team","file":",1708801310.abc123.nq"}

// Errors
{"level":"error","msg":"Failed to create Job","group":"family-chat","error":"quota exceeded"}
{"level":"warn","msg":"Invalid inbound message, discarding","file":",1708801290.xyz.nq","reason":"missing group field"}
```

---

## Error Handling

| Scenario | Detection | Action |
|---|---|---|
| Malformed inbound JSON | JSON.parse throws SyntaxError | Discard file, log warning |
| Inbound missing required fields | Validation check (Protocol §4.1) | Discard file, log warning |
| Stale inbound message (>5min old) | Timestamp comparison | Discard file, log warning |
| Unregistered group | Not in groups.json | Skip message, log info |
| K8s Job creation fails (quota, API error) | K8s API error | Log error. File deleted (not retried — message is lost). Consider: write error to outbox. |
| K8s Job creation fails (name conflict) | 409 Conflict | Job already exists — treat as hot path instead |
| Results dir missing request.json | File check | Archive dir, log error |
| Results dir missing response.jsonl | File check | Archive dir, send error text to outbox |
| response.jsonl has no extractable text | Parse logic returns null | Send "(Agent returned empty response)" to outbox |
| Active Job disappears from K8s | 404 on Job read in cleanup loop | Write "error" to status, remove from activeJobs |
| Orphaned follow-up files | Group queue files exist but no active Job | Discard if stale (>5min) |

---

## Testing Checklist

### Inbound Loop
- [ ] Valid inbound message + trigger match → Cold path: Job created
- [ ] Valid inbound message + trigger match + Job running → Hot path: follow-up enqueued
- [ ] Message does not match trigger → skipped, file deleted
- [ ] Message from unregistered group → skipped, file deleted
- [ ] Malformed JSON → discarded
- [ ] Missing required fields → discarded
- [ ] Stale message (>5min) → discarded
- [ ] Trigger stripping: `@Andy what's the weather?` → `what's the weather?`
- [ ] Multiple messages in one poll cycle → all processed in FIFO order

### Cold Path
- [ ] Job spec has correct labels, env vars, resource limits
- [ ] results/{session-id}/request.json created with routing info
- [ ] groups/{group}/ directory created
- [ ] activeJobs updated
- [ ] K8s name sanitized (lowercase, max 63 chars)

### Hot Path
- [ ] Follow-up written to /data/groups/{group}/ with atomic rename
- [ ] request.json messages array appended
- [ ] activeJobs NOT modified (same Job still tracked)

### Results Loop
- [ ] status=done + response.jsonl → outbound message written to correct outbox
- [ ] status=error → error message routed to outbox
- [ ] Result dir archived to .done/
- [ ] activeJobs entry removed
- [ ] Reply threading metadata built correctly (Telegram reply_to, Slack thread_ts)
- [ ] response.jsonl with "result" type line → extracted correctly
- [ ] response.jsonl with only "assistant" lines → fallback extraction works
- [ ] Empty response → "(Agent returned empty response)" sent

### Cleanup Loop
- [ ] Completed Jobs older than TTL → deleted
- [ ] Failed Job detected → status set to "error", removed from activeJobs
- [ ] Disappeared Job (404) → status set to "error", removed from activeJobs
- [ ] Orphaned stale follow-up files → discarded
- [ ] Archived results older than 24h → purged

### State Management
- [ ] activeJobs rebuilt correctly on startup from K8s Jobs list
- [ ] groups.json reload picks up changes without restart
- [ ] Gateway survives groups.json being temporarily malformed
