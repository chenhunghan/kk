# KubeClaw: Plan — Connector

> **Cross-references:**
> [Communication Protocol](protocol.md) — message formats §4.1, §4.2; nq conventions §2; polling intervals §7
> [Plan: Controller](plan-controller.md) — creates Connector Deployments
> [Plan: Gateway](plan-gateway.md) — reads inbound queue, writes outbox queue
> [Plan: Agent Job](plan-agent-job.md) ·
> [Plan: Skill](plan-skill.md)

---

## Summary

A Connector is a single-replica Deployment created by the Controller (one per Channel CR). It bridges an external messaging platform to the internal nq file queues on the shared PVC. A single image handles all platform types — behavior is selected by the `CHANNEL_TYPE` env var.

The Connector runs two concurrent loops:
1. **Inbound loop** — listens to the platform, writes normalized messages to `/data/inbound/`
2. **Outbound loop** — polls `/data/outbox/{channel-name}/`, sends responses back to the platform

The Connector has **no knowledge** of the Gateway, Agent Jobs, Skills, or any other KubeClaw internals. It only knows about its platform and the two queue directories.

---

## Image

```
kubeclaw-connector:latest

Base:    node:20-alpine
Install: nq (optional — can use atomic rename directly)
Runtime: Node.js with platform SDKs:
         - telegraf or node-telegram-bot-api  (Telegram)
         - @whiskeysockets/baileys            (WhatsApp)
         - @slack/bolt                        (Slack)
         - discord.js                         (Discord)
         - signal-cli or libsignal            (Signal)
Size:    ~150MB
```

### Entrypoint

```bash
#!/bin/bash
# kubeclaw-connector entrypoint

set -e

case "$CHANNEL_TYPE" in
  telegram)  node /app/connectors/telegram.js  ;;
  whatsapp)  node /app/connectors/whatsapp.js  ;;
  slack)     node /app/connectors/slack.js     ;;
  discord)   node /app/connectors/discord.js   ;;
  signal)    node /app/connectors/signal.js    ;;
  *)
    echo "Unknown CHANNEL_TYPE: $CHANNEL_TYPE" >&2
    exit 1
    ;;
esac
```

Each platform connector file (`telegram.js`, `slack.js`, etc.) imports shared modules for inbound writing and outbound reading, then implements the platform-specific client logic.

---

## Environment Variables

Set by Controller (see [Controller Plan](plan-controller.md) — Channel Reconciliation §"Build desired Deployment"):

| Var | Source | Example |
|---|---|---|
| `CHANNEL_TYPE` | Channel CR `spec.type` | `telegram` |
| `CHANNEL_NAME` | Channel CR `metadata.name` | `telegram-bot-1` |
| `INBOUND_DIR` | Hardcoded by Controller | `/data/inbound` |
| `OUTBOX_DIR` | Built from CHANNEL_NAME | `/data/outbox/telegram-bot-1` |
| `GROUPS_FILE` | Hardcoded | `/data/state/groups.json` |

Platform credentials come from the Secret via `envFrom`:

| Platform | Required Env Vars |
|---|---|
| Telegram | `TELEGRAM_BOT_TOKEN` |
| WhatsApp | `WHATSAPP_AUTH_STATE` (Baileys session, base64) |
| Slack | `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN` |
| Discord | `DISCORD_BOT_TOKEN` |
| Signal | `SIGNAL_PHONE_NUMBER`, `SIGNAL_AUTH_DATA` |

Config vars from Channel CR `spec.config` (prefixed `CONFIG_` by Controller):

| Config | Env Var | Type | Description |
|---|---|---|---|
| `allowedChatIds` | `CONFIG_ALLOWED_CHAT_IDS` | JSON array of strings | Telegram: only process messages from these chat IDs |
| `allowedChannels` | `CONFIG_ALLOWED_CHANNELS` | JSON array of strings | Slack: only process messages from these channel names |

---

## Startup Sequence

```
1. Read env vars: CHANNEL_TYPE, CHANNEL_NAME, INBOUND_DIR, OUTBOX_DIR
2. Validate required env vars present (exit 1 if missing)
3. Ensure directories exist:
   mkdir -p $INBOUND_DIR
   mkdir -p $OUTBOX_DIR
4. Load group mapping:
   groupMap = loadGroupMapping($GROUPS_FILE, CHANNEL_NAME)
   → builds reverse lookup: platform_group_id → group_name
   (see §Group Mapping below)
5. Parse config env vars:
   allowedIds = JSON.parse(process.env.CONFIG_ALLOWED_CHAT_IDS || "[]")
6. Initialize platform client (platform-specific — see §Platform Clients)
7. Start concurrent loops:
   a. inboundLoop()  — platform events → /data/inbound/
   b. outboundLoop() — /data/outbox/{channel}/ → platform API
   c. groupMapReloadLoop() — reload groups.json every 30s
8. Register SIGTERM handler:
   - Flush any pending outbound messages
   - Close platform connection gracefully
   - Exit 0
```

---

## Group Mapping

The Connector needs to translate platform-specific group/chat IDs into KubeClaw group names. This mapping comes from `/data/state/groups.json` (see [Protocol §6](protocol.md#6-group-registry)).

### Load Logic

```
loadGroupMapping(groupsFile, channelName):
  IF not exists(groupsFile):
    log.warn("groups.json not found — messages will use platform ID as group name")
    RETURN empty map

  data = JSON.parse(readFile(groupsFile))
  reverseMap = {}

  for groupName, groupConfig in data.groups:
    IF channelName in groupConfig.channels:
      platformId = groupConfig.channels[channelName].platform_group_id
      reverseMap[platformId] = groupName

  RETURN reverseMap

  # Example result for CHANNEL_NAME="telegram-bot-1":
  # {
  #   "-1001234567890": "family-chat",
  #   "-1009876543210": "work-team"
  # }
```

### Resolving Group Name

```
resolveGroup(platformGroupId, reverseMap):
  IF platformGroupId in reverseMap:
    RETURN reverseMap[platformGroupId]
  ELSE:
    # Unknown group — use a slugified version of the platform ID
    # This allows messages to flow even if groups.json isn't fully configured
    RETURN slugify(platformGroupId)
    # e.g. "-1001234567890" → "chat-1001234567890"
```

### Reload Loop

```
groupMapReloadLoop():
  every 30 seconds:
    try:
      newMap = loadGroupMapping(GROUPS_FILE, CHANNEL_NAME)
      reverseMap = newMap   # atomic swap (single-threaded JS — safe)
      log.debug("Reloaded group mapping", { groups: Object.keys(newMap).length })
    catch error:
      log.warn("Failed to reload groups.json", { error })
      # Keep using previous map
```

---

## Inbound Loop (Platform → PVC)

### Overview

```
Platform (Telegram/Slack/etc.)
  │
  │  WebSocket / long-poll / webhook
  ▼
Connector: inboundLoop()
  │
  │  1. Receive raw platform message
  │  2. Filter (allowed IDs, bot messages, etc.)
  │  3. Normalize to InboundMessage format (Protocol §4.1)
  │  4. Atomic write to /data/inbound/
  │
  ▼
/data/inbound/,{ts}.{id}.nq
```

### Detailed Logic

```
inboundLoop():
  platformClient.onMessage(raw):

    # 1. Filter
    IF isBotMessage(raw):
      RETURN                              # skip own messages and other bots
    IF allowedIds.length > 0 AND getPlatformGroupId(raw) NOT in allowedIds:
      RETURN                              # skip messages from non-allowed groups/chats
    IF getMessageText(raw) is empty:
      RETURN                              # skip non-text messages (images, stickers, etc.)

    # 2. Resolve group
    platformGroupId = getPlatformGroupId(raw)
    groupName = resolveGroup(platformGroupId, reverseMap)

    # 3. Normalize to InboundMessage (Protocol §4.1)
    msg = {
      channel:      CHANNEL_NAME,
      channel_type: CHANNEL_TYPE,
      group:        groupName,
      sender:       getSenderName(raw),
      text:         getMessageText(raw),
      timestamp:    getTimestamp(raw),
      platform_meta: buildPlatformMeta(raw)
    }

    # 4. Validate before writing
    IF not msg.channel or not msg.group or not msg.text or not msg.timestamp:
      log.warn("Skipping invalid message", { raw: summarize(raw) })
      RETURN

    # 5. Atomic write to inbound queue
    json = JSON.stringify(msg)
    uniqueId = randomHex(6)              # e.g. "a1b2c3"
    tmpPath  = INBOUND_DIR + "/.tmp-" + uniqueId
    nqPath   = INBOUND_DIR + "/," + msg.timestamp + "." + uniqueId + ".nq"

    writeFileSync(tmpPath, json)
    renameSync(tmpPath, nqPath)          # atomic — Gateway never sees partial file

    log.info("Enqueued inbound message", {
      group: groupName,
      sender: msg.sender,
      textLength: msg.text.length
    })
```

### Platform-Specific Normalization

#### Telegram

```
getPlatformGroupId(raw):  String(raw.chat.id)
getSenderName(raw):       raw.from.first_name + (raw.from.last_name ? " " + raw.from.last_name : "")
getMessageText(raw):      raw.text || raw.caption || ""
getTimestamp(raw):         raw.date                         # already Unix seconds
isBotMessage(raw):        raw.from.is_bot === true

buildPlatformMeta(raw):
  {
    chat_id:    String(raw.chat.id),
    message_id: raw.message_id
  }
```

#### WhatsApp (Baileys)

```
getPlatformGroupId(raw):  raw.key.remoteJid
getSenderName(raw):       raw.pushName || raw.key.participant || "Unknown"
getMessageText(raw):      raw.message?.conversation
                          || raw.message?.extendedTextMessage?.text
                          || ""
getTimestamp(raw):         Number(raw.messageTimestamp)      # may be string, coerce
isBotMessage(raw):        raw.key.fromMe === true

buildPlatformMeta(raw):
  {
    jid:        raw.key.remoteJid,
    message_id: raw.key.id,
    participant: raw.key.participant                         # sender in group
  }
```

#### Slack (Bolt)

```
getPlatformGroupId(raw):  raw.channel
getSenderName(raw):       lookupUsername(raw.user)           # Slack user ID → display name
getMessageText(raw):      raw.text || ""
getTimestamp(raw):         Math.floor(parseFloat(raw.ts))    # Slack ts is "epoch.micro"
isBotMessage(raw):        raw.bot_id != null || raw.subtype === "bot_message"

buildPlatformMeta(raw):
  {
    channel_id: raw.channel,
    ts:         raw.ts,
    thread_ts:  raw.thread_ts || null                       # for threaded replies
  }
```

#### Discord

```
getPlatformGroupId(raw):  raw.channelId
getSenderName(raw):       raw.author.globalName || raw.author.username
getMessageText(raw):      raw.content || ""
getTimestamp(raw):         Math.floor(raw.createdTimestamp / 1000)
isBotMessage(raw):        raw.author.bot === true

buildPlatformMeta(raw):
  {
    channel_id: raw.channelId,
    message_id: raw.id,
    guild_id:   raw.guildId
  }
```

---

## Outbound Loop (PVC → Platform)

### Overview

```
/data/outbox/{channel-name}/,{ts}.{id}.nq
  │
  │  Connector polls every 1 second (Protocol §7)
  ▼
Connector: outboundLoop()
  │
  │  1. List pending nq files, sort by timestamp (FIFO)
  │  2. Read OutboundMessage (Protocol §4.2)
  │  3. Validate channel matches own CHANNEL_NAME
  │  4. Send via platform API
  │  5. Delete file on success
  │
  ▼
Platform (Telegram/Slack/etc.) → User sees response
```

### Detailed Logic

```
outboundLoop():
  POLL_INTERVAL = 1000    # ms — Protocol §7

  loop:
    await sleep(POLL_INTERVAL)

    # 1. List pending files
    files = readdirSync(OUTBOX_DIR)
      .filter(f => f.startsWith(",") && f.endsWith(".nq"))
      .sort()              # lexicographic sort = timestamp order = FIFO

    IF files.length === 0:
      CONTINUE

    # 2. Process each file
    for filename in files:
      filePath = OUTBOX_DIR + "/" + filename

      try:
        # Read and parse
        content = readFileSync(filePath, "utf8")
        msg = JSON.parse(content)

        # Validate
        IF msg.channel !== CHANNEL_NAME:
          log.error("Outbound message channel mismatch", {
            expected: CHANNEL_NAME,
            got: msg.channel,
            file: filename
          })
          unlinkSync(filePath)         # discard — wrong channel
          CONTINUE

        IF not msg.text or not msg.platform_meta:
          log.error("Outbound message missing required fields", { file: filename })
          unlinkSync(filePath)         # discard — malformed
          CONTINUE

        # Send via platform
        await sendToPlatform(msg)

        # Success — delete file
        unlinkSync(filePath)

        log.info("Sent outbound message", {
          group: msg.group,
          textLength: msg.text.length,
          platform: CHANNEL_TYPE
        })

      catch error:
        log.error("Failed to send outbound message", {
          file: filename,
          error: error.message
        })
        # DO NOT delete file — will be retried next poll
        # But if file is older than 5 minutes, log a warning
        IF fileAge(filePath) > 300:
          log.warn("Outbound message stuck for >5min", { file: filename })
```

### Platform-Specific Sending

#### Telegram

```
sendToPlatform(msg):
  options = {}
  IF msg.platform_meta.reply_to_message_id:
    options.reply_to_message_id = msg.platform_meta.reply_to_message_id

  # Telegram has a 4096 char limit per message
  chunks = splitText(msg.text, 4096)
  for i, chunk in enumerate(chunks):
    await bot.sendMessage(
      msg.platform_meta.chat_id,
      chunk,
      i === 0 ? options : {}           # only reply-thread the first chunk
    )
    IF chunks.length > 1 AND i < chunks.length - 1:
      await sleep(100)                 # rate limit buffer between chunks
```

#### WhatsApp (Baileys)

```
sendToPlatform(msg):
  # WhatsApp has a ~65536 char limit but best practice is <4096
  chunks = splitText(msg.text, 4096)
  for chunk in chunks:
    await sock.sendMessage(
      msg.platform_meta.jid,
      { text: chunk }
    )
```

#### Slack (Bolt)

```
sendToPlatform(msg):
  options = {
    channel: msg.platform_meta.channel_id,
    text:    msg.text
  }
  IF msg.platform_meta.thread_ts:
    options.thread_ts = msg.platform_meta.thread_ts

  # Slack has a 40000 char limit
  IF msg.text.length > 40000:
    chunks = splitText(msg.text, 40000)
    for chunk in chunks:
      await client.chat.postMessage({ ...options, text: chunk })
  ELSE:
    await client.chat.postMessage(options)
```

#### Discord

```
sendToPlatform(msg):
  channel = await client.channels.fetch(msg.platform_meta.channel_id)

  # Discord has a 2000 char limit
  chunks = splitText(msg.text, 2000)
  for chunk in chunks:
    await channel.send(chunk)
```

### Text Splitting

```
splitText(text, maxLength):
  IF text.length <= maxLength:
    RETURN [text]

  chunks = []
  remaining = text
  WHILE remaining.length > 0:
    IF remaining.length <= maxLength:
      chunks.push(remaining)
      BREAK

    # Try to split at last newline before maxLength
    splitIndex = remaining.lastIndexOf("\n", maxLength)
    IF splitIndex < maxLength * 0.5:
      # No good newline — split at last space
      splitIndex = remaining.lastIndexOf(" ", maxLength)
    IF splitIndex < maxLength * 0.5:
      # No good space — hard cut
      splitIndex = maxLength

    chunks.push(remaining.substring(0, splitIndex))
    remaining = remaining.substring(splitIndex).trimStart()

  RETURN chunks
```

---

## Platform Client Initialization

### Telegram

```
initTelegram():
  token = process.env.TELEGRAM_BOT_TOKEN
  IF not token: exit 1

  bot = new TelegramBot(token, { polling: true })

  bot.on("message", (raw) => {
    inboundHandler(raw)
  })

  # Healthcheck: bot.getMe() — verifies token is valid
  me = await bot.getMe()
  log.info("Telegram bot connected", { username: me.username })

  RETURN { client: bot, me }
```

### WhatsApp (Baileys)

```
initWhatsApp():
  authState = decodeBase64(process.env.WHATSAPP_AUTH_STATE)
  { state, saveCreds } = await useMultiFileAuthState(authState)

  sock = makeWASocket({
    auth: state,
    printQRInTerminal: false           # headless — no QR scan
  })

  sock.ev.on("messages.upsert", ({ messages }) => {
    for raw in messages:
      inboundHandler(raw)
  })

  sock.ev.on("creds.update", saveCreds)

  log.info("WhatsApp connected")
  RETURN { client: sock }
```

### Slack (Bolt)

```
initSlack():
  app = new App({
    token:    process.env.SLACK_BOT_TOKEN,
    appToken: process.env.SLACK_APP_TOKEN,
    socketMode: true                   # no public URL needed
  })

  app.message(async ({ message }) => {
    inboundHandler(message)
  })

  await app.start()
  log.info("Slack bot connected via Socket Mode")
  RETURN { client: app }
```

### Discord

```
initDiscord():
  client = new Client({
    intents: [
      GatewayIntentBits.Guilds,
      GatewayIntentBits.GuildMessages,
      GatewayIntentBits.MessageContent
    ]
  })

  client.on("messageCreate", (raw) => {
    inboundHandler(raw)
  })

  await client.login(process.env.DISCORD_BOT_TOKEN)
  log.info("Discord bot connected", { user: client.user.tag })
  RETURN { client }
```

---

## Error Handling

### Inbound Errors

| Error | Handling |
|---|---|
| Platform connection lost | SDK auto-reconnects (Telegram polling, WhatsApp WS, Slack Socket Mode). Log warning. |
| Platform rate limited | Back off per SDK defaults. Log warning. Messages buffered by platform. |
| PVC write fails (disk full) | Log error. Message lost. Connector stays running for future messages. |
| groups.json missing/corrupt | Use fallback group name (slugified platform ID). Log warning. |
| Non-text message (image, sticker) | Silently skip (text field is empty). |

### Outbound Errors

| Error | Handling |
|---|---|
| Platform API error (transient) | Leave nq file in place → retry on next poll (1s later). |
| Platform API error (permanent, e.g., chat deleted) | Log error. File remains for 5 min → log stuck warning. Manual cleanup needed. |
| Malformed outbound message | Log error, delete file (discard). |
| Channel mismatch in outbound message | Log error, delete file. Should never happen if Gateway is correct. |

### Connection Recovery

All platform SDKs handle reconnection internally:

| Platform | Reconnection |
|---|---|
| Telegram | `node-telegram-bot-api` polling auto-restarts on error |
| WhatsApp | Baileys WebSocket reconnects automatically |
| Slack | Bolt Socket Mode reconnects automatically |
| Discord | discord.js Gateway reconnects automatically |

The Connector process itself should NOT exit on transient connection errors. It only exits on fatal errors (invalid token, missing env vars).

---

## Graceful Shutdown

```
process.on("SIGTERM", async () => {
  log.info("Received SIGTERM, shutting down gracefully")

  # 1. Stop accepting inbound messages
  stopInbound()                          # close polling/WebSocket

  # 2. Flush remaining outbound messages (best effort, 5s timeout)
  await flushOutbound(timeoutMs: 5000)

  # 3. Close platform connection
  await platformClient.close()

  # 4. Exit
  log.info("Connector shutdown complete")
  process.exit(0)
})
```

K8s sends SIGTERM, waits `terminationGracePeriodSeconds` (default 30s), then SIGKILL. 5s outbound flush is well within that window.

---

## File Structure (in image)

```
/app/
├── entrypoint.sh                      ← selects connector by CHANNEL_TYPE
├── connectors/
│   ├── telegram.js
│   ├── whatsapp.js
│   ├── slack.js
│   ├── discord.js
│   └── signal.js
├── lib/
│   ├── inbound.js                     ← shared: atomic write to inbound queue
│   ├── outbound.js                    ← shared: poll + send loop
│   ├── groups.js                      ← shared: group mapping loader
│   ├── text-split.js                  ← shared: message chunking
│   └── logger.js                      ← shared: structured JSON logging
└── package.json
```

### Shared Modules

**`lib/inbound.js`** — used by all platform connectors:
```
exports.enqueueInbound(inboundDir, message):
  json = JSON.stringify(message)
  id = randomHex(6)
  tmp = path.join(inboundDir, ".tmp-" + id)
  nq  = path.join(inboundDir, "," + message.timestamp + "." + id + ".nq")
  fs.writeFileSync(tmp, json)
  fs.renameSync(tmp, nq)
```

**`lib/outbound.js`** — used by all platform connectors:
```
exports.startOutboundLoop(outboxDir, channelName, sendFn, pollInterval = 1000):
  setInterval(() => {
    files = listNqFiles(outboxDir)
    for file of files:
      msg = readAndParse(file)
      if msg.channel !== channelName: skip
      await sendFn(msg)
      fs.unlinkSync(file)
  }, pollInterval)
```

---

## Logging

Structured JSON logs, same format as Controller:

```jsonc
// Startup
{"level":"info","msg":"Connector starting","channel":"telegram-bot-1","type":"telegram"}
{"level":"info","msg":"Telegram bot connected","username":"andy_bot"}
{"level":"info","msg":"Loaded group mapping","groups":2}

// Inbound
{"level":"info","msg":"Enqueued inbound message","group":"family-chat","sender":"John","textLength":28}
{"level":"debug","msg":"Skipped non-text message","type":"sticker","chatId":"-1001234567890"}
{"level":"debug","msg":"Skipped message from non-allowed chat","chatId":"-999999"}

// Outbound
{"level":"info","msg":"Sent outbound message","group":"family-chat","textLength":142,"platform":"telegram"}
{"level":"warn","msg":"Outbound message stuck for >5min","file":",1708801310.abc123.nq"}
{"level":"error","msg":"Failed to send outbound","error":"chat not found","file":",1708801310.abc123.nq"}

// Connection
{"level":"warn","msg":"Platform connection lost, reconnecting","platform":"telegram"}
{"level":"info","msg":"Platform connection restored","platform":"telegram"}

// Shutdown
{"level":"info","msg":"Received SIGTERM, shutting down gracefully"}
{"level":"info","msg":"Connector shutdown complete"}
```

---

## Testing Checklist

### Inbound Tests
- [ ] Telegram message → normalized InboundMessage written to `/data/inbound/`
- [ ] WhatsApp message → normalized InboundMessage written to `/data/inbound/`
- [ ] Slack message → normalized InboundMessage written to `/data/inbound/`
- [ ] Discord message → normalized InboundMessage written to `/data/inbound/`
- [ ] Bot's own messages filtered out (not enqueued)
- [ ] Messages from non-allowed chat IDs filtered out
- [ ] Non-text messages (images, stickers) filtered out
- [ ] Group mapping resolves platform ID → group name correctly
- [ ] Unknown platform group ID → fallback slug name
- [ ] Atomic write: no partial files visible in inbound dir
- [ ] groups.json missing → connector still works with fallback names
- [ ] groups.json reload picks up new mappings without restart

### Outbound Tests
- [ ] Outbound nq file processed → message sent via platform API
- [ ] File deleted after successful send
- [ ] File retained on transient platform error → retried next poll
- [ ] Channel mismatch in outbound message → file discarded with error log
- [ ] Malformed outbound JSON → file discarded with error log
- [ ] Long message split correctly per platform limits (4096/2000/40000)
- [ ] Reply threading works (Telegram reply_to, Slack thread_ts)

### Connection Tests
- [ ] Platform connection lost → auto-reconnects
- [ ] Invalid token → connector exits 1 (no retry)
- [ ] SIGTERM → graceful shutdown within 5s
- [ ] Multiple rapid messages → all enqueued without data loss
