# KubeClaw: Plan — Skill

> **Cross-references:**
> [Communication Protocol](protocol.md) — file paths §5.2, §5.3; ownership §3
> [Plan: Controller](plan-controller.md) — reconciles Skill CRs, clones repos, writes to PVC
> [Plan: Agent Job](plan-agent-job.md) — Phase 0 symlinks skills into session directory
> [Plan: Connector](plan-connector.md) ·
> [Plan: Gateway](plan-gateway.md)

---

## Summary

A Skill is not a running process — it is a **data artifact** managed by a CRD. The Skill CRD declares that a particular skill from a git repo should be available to all Agent Jobs. The Controller handles the lifecycle (fetch on create, remove on delete), and Agent Jobs consume skills by symlinking them into the session directory.

This plan covers the Skill as a concept: what it is, how it's structured, how users interact with it, and how it flows through the system end-to-end.

### Touchpoints

| Component | Role with Skills |
|---|---|
| **User** | Creates/deletes Skill CRs via kubectl |
| **Controller** | Clones repo, validates SKILL.md, copies to PVC, updates CR status |
| **Agent Job** | Phase 0: symlinks `/data/skills/*` into session `.claude/skills/` and `.agents/skills/` |
| **LLM (Claude Code)** | Discovers skills natively, reads metadata, loads full content when relevant |
| **Gateway** | No interaction with skills — transparent |
| **Connector** | No interaction with skills — transparent |

---

## What is a Skill?

A skill is a directory containing at minimum a `SKILL.md` file. It follows the [Agent Skills specification](https://agentskills.io).

### Minimal Skill

```
my-skill/
└── SKILL.md
```

### Full Skill

```
my-skill/
├── SKILL.md                    ← Required: instructions + metadata
├── scripts/                    ← Optional: executable helpers
│   ├── deploy.sh
│   └── fetch-data.sh
├── references/                 ← Optional: supporting docs (loaded on demand)
│   ├── api-reference.md
│   └── style-guide.md
└── assets/                     ← Optional: templates, examples, artifacts
    ├── template.html
    └── example-config.yaml
```

### SKILL.md Format

```markdown
---
name: frontend-design
description: React and Next.js performance optimization guidelines from Vercel Engineering. Contains 40+ rules across 8 categories, prioritized by impact.
---

# Frontend Design

Instructions for the agent to follow when this skill is activated.

## When to Use

Use this skill when:
- Reviewing React components for performance
- Building new Next.js pages or routes
- Optimizing rendering, hydration, or bundle size

## Guidelines

### Category 1: Component Architecture
...

### Category 2: Data Fetching
...
```

### YAML Frontmatter Fields

| Field | Required | Type | Description |
|---|---|---|---|
| `name` | **Yes** | string | Unique identifier. Lowercase, hyphens allowed. e.g., `frontend-design` |
| `description` | **Yes** | string | What the skill does and when to use it. This is the primary routing signal — the LLM reads this to decide relevance. |
| `metadata` | No | object | Arbitrary key-value pairs |
| `metadata.internal` | No | boolean | If `true`, skill is hidden from normal discovery |
| `allowed-tools` | No | list | Tools the skill is allowed to use (agent-specific) |
| `license` | No | string | License identifier |
| `compatibility` | No | string | Max 500 chars. Environment requirements. |

### The Description is the Routing Rule

The `description` field is critical. The LLM agent uses it to decide whether to load the skill's full content for the current task. Good descriptions include:

- **What** the skill does
- **When** to use it (trigger phrases, scenarios)
- **Domain** it covers

Example (good):
```
description: "Extract text and tables from PDFs, fill forms, merge documents. Use when the user mentions PDFs, forms, scanning, or document extraction."
```

Example (bad):
```
description: "A useful skill for documents."
```

---

## CRD Definition

```yaml
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: skills.kubeclaw.io
spec:
  group: kubeclaw.io
  versions:
  - name: v1alpha1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            required: [source]
            properties:
              source:
                type: string
                description: >
                  GitHub shorthand path to a specific skill directory.
                  Format: owner/repo/path/to/skill
                  Examples:
                    - vercel-labs/agent-skills/skills/frontend-design
                    - vercel-labs/agent-skills/skills/vercel-deploy
                    - myorg/internal-skills/skills/code-review
          status:
            type: object
            properties:
              phase:
                type: string
                enum: [Pending, Fetching, Ready, Error]
              message:
                type: string
              skillName:
                type: string
                description: "name field parsed from SKILL.md frontmatter"
              skillDescription:
                type: string
                description: "description field parsed from SKILL.md frontmatter"
              lastFetched:
                type: string
                format: date-time
    subresources:
      status: {}
    additionalPrinterColumns:
    - name: Source
      type: string
      jsonPath: .spec.source
    - name: Skill
      type: string
      jsonPath: .status.skillName
    - name: Status
      type: string
      jsonPath: .status.phase
    - name: Age
      type: date
      jsonPath: .metadata.creationTimestamp
  scope: Namespaced
  names:
    plural: skills
    singular: skill
    kind: Skill
    shortNames: [sk]
```

---

## Source Format

The `spec.source` field uses a GitHub shorthand path:

```
owner/repo/path/to/skill
```

The Controller parses this into three parts:

| Part | Extraction | Example |
|---|---|---|
| Owner | `parts[0]` | `vercel-labs` |
| Repo | `parts[1]` | `agent-skills` |
| Skill path | `parts[2:]` joined by `/` | `skills/frontend-design` |

Clone URL is constructed as:
```
https://github.com/{owner}/{repo}.git
```

The skill directory is then found at:
```
{clone_root}/{skill_path}/
```

### Source Examples

| Source | Repo cloned | Skill dir in repo |
|---|---|---|
| `vercel-labs/agent-skills/skills/frontend-design` | `vercel-labs/agent-skills` | `skills/frontend-design/` |
| `vercel-labs/agent-skills/skills/vercel-deploy` | `vercel-labs/agent-skills` | `skills/vercel-deploy/` |
| `vercel-labs/agent-browser/skills/agent-browser` | `vercel-labs/agent-browser` | `skills/agent-browser/` |
| `myorg/internal-skills/code-review` | `myorg/internal-skills` | `code-review/` |
| `myorg/mono-repo/packages/ai/skills/pr-review` | `myorg/mono-repo` | `packages/ai/skills/pr-review/` |

### Invalid Sources

| Source | Error |
|---|---|
| `vercel-labs` | "Invalid source format. Expected: owner/repo/path/to/skill" (< 3 segments) |
| `vercel-labs/agent-skills` | Same — no skill path segment |
| (empty string) | CRD validation rejects (required field) |

---

## PVC Storage

### Location

```
/data/skills/{skill-cr-metadata-name}/
```

The directory name is the **Skill CR's `metadata.name`**, NOT the `name` from SKILL.md frontmatter (Protocol §5.2). This prevents collisions if two CRs reference skills with the same frontmatter name from different repos.

### Example Layout

After installing three skills:

```
/data/skills/
├── frontend-design/                   ← CR name: frontend-design
│   └── SKILL.md                       ← from vercel-labs/agent-skills/skills/frontend-design
│
├── vercel-deploy/                     ← CR name: vercel-deploy
│   ├── SKILL.md                       ← from vercel-labs/agent-skills/skills/vercel-deploy
│   └── scripts/
│       └── deploy.sh                  ← executable (chmod a+x set by Controller)
│
└── agent-browser/                     ← CR name: agent-browser
    ├── SKILL.md                       ← from vercel-labs/agent-browser/skills/agent-browser
    ├── scripts/
    │   └── agent-browser.sh
    └── references/
        └── api-reference.md
```

### Permissions

Set by Controller after copying (see [Controller Plan](plan-controller.md) — Skill Reconciliation §7–8):

| Path | Permission | Why |
|---|---|---|
| Skill directory (recursive) | `a+rX` | Agent Jobs may run as a different user. All files must be world-readable, dirs world-executable. |
| `scripts/` files | `a+x` | Skills may reference scripts that the LLM agent executes. They must be executable. |

### Disk Usage

Skills are typically small:
- A SKILL.md is usually 1–50 KB
- Scripts are 1–10 KB each
- References can be up to 500 KB for large docs

Estimated disk usage per skill: **5–100 KB typical**, up to **1 MB** for large skills with many references.

With 50 skills installed, total disk usage: **~5–50 MB**. Negligible on a 10 Gi PVC.

---

## Lifecycle

### Install

```
User                    Controller              PVC                    Agent Job
 │                          │                    │                         │
 │  kubectl apply           │                    │                         │
 │  Skill CR ──────────────►│                    │                         │
 │                          │                    │                         │
 │                          │  git clone         │                         │
 │                          │  (shallow, depth 1)│                         │
 │                          │  ──────────────────►                         │
 │                          │                    │                         │
 │                          │  validate SKILL.md │                         │
 │                          │  parse frontmatter │                         │
 │                          │                    │                         │
 │                          │  cp -r to PVC      │                         │
 │                          │  ──────────────────►│                        │
 │                          │                    │  /data/skills/{name}/   │
 │                          │                    │                         │
 │                          │  update CR status  │                         │
 │                          │  phase: Ready      │                         │
 │                          │  skillName: ...    │                         │
 │                          │  skillDescription: │                         │
 │                          │                    │                         │
 │  kubectl get skills      │                    │                         │
 │  ◄──────────────────────┤                    │                         │
 │  NAME    STATUS  AGE     │                    │                         │
 │  foo     Ready   10s     │                    │                         │
 │                          │                    │                         │
 │                          │                    │  (next Job starts)      │
 │                          │                    │         │               │
 │                          │                    │  Phase 0: symlink       │
 │                          │                    │  /data/skills/{name}/   │
 │                          │                    │    → .claude/skills/    │
 │                          │                    │    → .agents/skills/    │
 │                          │                    │         │               │
 │                          │                    │  Claude discovers skill │
 │                          │                    │  reads description      │
 │                          │                    │  loads if relevant      │
```

### Uninstall

```
User                    Controller              PVC
 │                          │                    │
 │  kubectl delete          │                    │
 │  Skill CR ──────────────►│                    │
 │                          │                    │
 │                          │  rm -rf from PVC   │
 │                          │  ──────────────────►│
 │                          │                    │  /data/skills/{name}/ GONE
 │                          │                    │
 │                          │                    │  (next Job starts)
 │                          │                    │  Phase 0: symlink loop
 │                          │                    │  skips — dir no longer exists
 │                          │                    │
 │                          │                    │  (running Jobs)
 │                          │                    │  existing symlinks → dangling
 │                          │                    │  agent ignores gracefully
```

### Update (pull latest)

There is no in-place update mechanism. The user deletes and recreates the CR:

```bash
kubectl delete skill frontend-design -n kubeclaw
kubectl apply -f skill-frontend-design.yaml
```

This triggers:
1. Delete event → Controller removes `/data/skills/frontend-design/`
2. Create event → Controller clones fresh, validates, copies to PVC

**Why no auto-update?**
- Simplicity: no polling git repos, no diffing, no cache invalidation
- Predictability: the skill on disk matches exactly what was fetched at create time
- Kubernetes-native: delete + apply is the standard declarative pattern

---

## How the LLM Discovers and Uses Skills

### Discovery (Progressive Disclosure)

Claude Code uses a two-phase discovery model:

```
Phase A: Metadata scan (always happens)
  Claude reads all SKILL.md files in .claude/skills/
  For each, it reads ONLY the YAML frontmatter:
    - name
    - description
  This is lightweight — a few hundred bytes per skill.

Phase B: Full load (on demand)
  If the current prompt/task matches a skill's description,
  Claude loads the FULL SKILL.md body (instructions, guidelines, steps).
  It may also read files from references/ or execute scripts from scripts/.
```

This means:
- 50 installed skills ≠ 50 skills loaded into context
- Only relevant skills consume LLM context tokens
- Bad descriptions → skills never get activated → wasted install

### Example: Skill Activation Flow

```
User asks: "Review this React component for performance issues"

Claude scans .claude/skills/ metadata:
  - frontend-design: "React and Next.js performance optimization..."  ← MATCH
  - vercel-deploy: "Deploy applications and websites to Vercel..."    ← no match
  - agent-browser: "Browser automation CLI for AI agents..."          ← no match

Claude loads full content of frontend-design/SKILL.md
Claude applies the 40+ performance rules from the skill
Claude provides a review citing specific guideline violations
```

### Script Execution

If a SKILL.md references scripts:

```markdown
## How It Works

1. Run the deploy script:
   ```bash
   /mnt/skills/user/vercel-deploy/scripts/deploy.sh
   ```
```

In KubeClaw, the script path would be:
```
/data/skills/vercel-deploy/scripts/deploy.sh
```

Since the session dir symlinks to `/data/skills/`, Claude can also reference:
```
.claude/skills/vercel-deploy/scripts/deploy.sh
```

Both resolve to the same file via symlinks. Scripts must be executable (handled by Controller).

### Reference Files

Skills may include reference files for deep context:

```markdown
## Detailed API Reference

For full API details, read `references/api-reference.md`.
```

Claude reads `references/api-reference.md` only when it needs the detailed reference — not on every invocation. This keeps the base context light.

---

## User Experience

### Install Skills

```bash
# Single skill
cat <<EOF | kubectl apply -f -
apiVersion: kubeclaw.io/v1alpha1
kind: Skill
metadata:
  name: frontend-design
  namespace: kubeclaw
spec:
  source: vercel-labs/agent-skills/skills/frontend-design
EOF

# Multiple skills (single YAML with ---)
kubectl apply -f skills.yaml
```

### List Installed Skills

```bash
kubectl get skills -n kubeclaw

# NAME               SOURCE                                              SKILL              STATUS   AGE
# frontend-design    vercel-labs/agent-skills/skills/frontend-design      frontend-design    Ready    5d
# vercel-deploy      vercel-labs/agent-skills/skills/vercel-deploy        vercel-deploy      Ready    5d
# agent-browser      vercel-labs/agent-browser/skills/agent-browser       agent-browser      Ready    3d
```

### Inspect a Skill

```bash
kubectl describe skill frontend-design -n kubeclaw

# Name:         frontend-design
# Namespace:    kubeclaw
# Spec:
#   Source:  vercel-labs/agent-skills/skills/frontend-design
# Status:
#   Phase:             Ready
#   Skill Name:        frontend-design
#   Skill Description: React and Next.js performance optimization guidelines...
#   Last Fetched:      2026-02-20T10:30:00Z
```

### Debug a Skill

```bash
# Check what's on the PVC
kubectl exec -n kubeclaw deploy/kubeclaw-gateway -- ls /data/skills/
# frontend-design
# vercel-deploy
# agent-browser

# Read the SKILL.md
kubectl exec -n kubeclaw deploy/kubeclaw-gateway -- cat /data/skills/frontend-design/SKILL.md

# Check if scripts are executable
kubectl exec -n kubeclaw deploy/kubeclaw-gateway -- ls -la /data/skills/vercel-deploy/scripts/
# -rwxr-xr-x  1 root root  2048 Feb 20 10:30 deploy.sh
```

### Update a Skill

```bash
kubectl delete skill frontend-design -n kubeclaw
kubectl apply -f skill-frontend-design.yaml
```

### Remove a Skill

```bash
kubectl delete skill agent-browser -n kubeclaw
# Controller removes /data/skills/agent-browser/
# Next Agent Job won't symlink it
# Running Agent Jobs: dangling symlink, agent ignores
```

### Remove All Skills

```bash
kubectl delete skills --all -n kubeclaw
```

---

## Status Phases

| Phase | Meaning | Transitions to |
|---|---|---|
| `Pending` | CR created, not yet processed | `Fetching` |
| `Fetching` | Controller is cloning the repo | `Ready` or `Error` |
| `Ready` | Skill validated and installed on PVC | (terminal — stays here until deleted) |
| `Error` | Something went wrong | (terminal — user must delete + recreate) |

```
                ┌─────────┐
   CR created → │ Pending │
                └────┬────┘
                     │
                     ▼
                ┌──────────┐
                │ Fetching │
                └────┬─────┘
                     │
              ┌──────┴──────┐
              │             │
              ▼             ▼
         ┌────────┐    ┌────────┐
         │ Ready  │    │ Error  │
         └────────┘    └────────┘
              │             │
              │             │  (user deletes + recreates)
              │             ▼
              │        back to Pending
              │
              │  (user deletes)
              ▼
           CR gone, PVC cleaned
```

### Error Messages

| Cause | status.message |
|---|---|
| Source has < 3 path segments | `Invalid source format. Expected: owner/repo/path/to/skill. Got: {source}` |
| Git clone 404 | `git clone failed: repository 'https://github.com/...' not found` |
| Git clone network error | `git clone failed: Could not resolve host: github.com` |
| Git clone timeout | `Clone timed out after {N} seconds` |
| Skill path doesn't exist in repo | `Path 'skills/bad-name' not found in repo owner/repo` |
| SKILL.md not in directory | `SKILL.md not found at skills/bad-name/SKILL.md` |
| Missing `name` in frontmatter | `SKILL.md missing required 'name' field in YAML frontmatter` |
| Missing `description` in frontmatter | `SKILL.md missing required 'description' field in YAML frontmatter` |
| PVC write failure | `Failed to copy skill to PVC: {OS error}` |

---

## Design Decisions

### One CRD = One Skill (not one repo)

A repo like `vercel-labs/agent-skills` contains multiple skills. Each installed skill is a separate CR. This gives fine-grained control: install only what you need, remove individual skills, track status per skill.

The trade-off is that two CRs from the same repo trigger two independent `git clone` operations. This is acceptable because:
- Clones are shallow (`--depth 1`) — fast and small
- Skill repos are typically < 10 MB
- Install is a one-time operation (not continuous)
- No shared state to invalidate

### No Git Ref Pinning

Always clones the default branch (HEAD). This keeps the CRD simple — one field (`source`), no version tracking.

If reproducibility is needed later, we can add an optional `ref` field:
```yaml
spec:
  source: vercel-labs/agent-skills/skills/frontend-design
  ref: v1.2.0   # optional: tag, branch, or commit SHA
```

### No Auto-Update

Skills do not automatically pull new versions. The version on disk is exactly what was fetched at CR creation time. Update = delete + recreate.

If periodic updates are needed later, options include:
- A CronJob that deletes + recreates Skill CRs on a schedule
- An `update` annotation that triggers re-fetch on the next reconcile
- A `skills update` kubectl plugin

### All Skills Available to All Groups

There is no per-group skill filtering. Every Agent Job gets every skill. The LLM self-selects based on descriptions.

This is the simplest model and works well because:
- Skills are lightweight (metadata scan is cheap)
- The LLM is good at relevance matching
- Adding per-group filtering adds CRD complexity (selector fields, label matching)

If needed later, per-group filtering could be added via:
- A `groups` field on the Skill CRD (whitelist)
- Group labels matching skill labels
- Gateway passing a skill list as env var to the Job

### Skills on PVC (not ConfigMaps)

Skills live on the PVC filesystem, not in K8s ConfigMaps or Secrets. Reasons:
- Skills may contain scripts that need `chmod +x` — ConfigMaps can't set execute permissions reliably
- Skills may be large (references, assets) — ConfigMap has a 1 MB limit
- The PVC is already shared by all components — no new infrastructure
- Agent Jobs already mount the PVC — no additional volumeMounts needed

---

## Relationship to Memory

Skills and memory files serve different purposes:

| | Skills (`/data/skills/`) | Memory (`/data/memory/`) |
|---|---|---|
| **What** | Reusable instruction sets from external repos | Custom context written by the user |
| **Scope** | All groups (universal) | Global (`SOUL.md`) or per-group (`{group}/CLAUDE.md`) |
| **Managed by** | Controller (CRD lifecycle) | Human (manual edit, kubectl cp) |
| **Content** | Agent capabilities, guidelines, scripts | Personality, relationships, preferences |
| **Discovery** | Native skill loading (progressive) | Concatenated into prompt (always loaded) |
| **Examples** | "Frontend design guidelines", "Deploy to Vercel" | "You are Andy. Be friendly.", "John likes weather." |

They are complementary:
- **Memory** tells the agent *who it is* and *who it's talking to*
- **Skills** tell the agent *what it can do* and *how to do it*

---

## Security Considerations

### Arbitrary Code Execution

Skills can include scripts that the LLM agent may execute. This is inherent to the Agent Skills spec — skills are designed to extend agent capabilities, which includes running code.

**Mitigations:**
- Agent Jobs run with resource limits (CPU, memory, deadline)
- Agent Jobs have no K8s API access (no ServiceAccount privileges)
- Agent Jobs only mount the shared PVC — no host filesystem access
- The PVC is scoped to the kubeclaw namespace
- Review skills before installing (same as any code you run)

### Untrusted Skill Sources

The Controller clones from any GitHub URL the user specifies. If the user installs a malicious skill, the LLM agent may follow harmful instructions.

**Mitigations:**
- Only cluster admins can create Skill CRs (standard K8s RBAC)
- Skills from well-known repos (vercel-labs, etc.) are generally safe
- For sensitive environments: restrict `source` to approved orgs via admission webhooks
- Review SKILL.md content before installing (`kubectl exec ... cat /data/skills/{name}/SKILL.md`)

### Private Repos

The current design only supports public GitHub repos (`https://github.com/...`). For private repos, future options include:
- A `secretRef` field on the Skill CRD (containing a GitHub token)
- SSH clone URLs with a deploy key Secret
- Git credential helper configured in the Controller image

---

## Testing Checklist

### CRD
- [ ] Skill CR with valid source → accepted by API server
- [ ] Skill CR without source → rejected (required field)
- [ ] `kubectl get skills` shows Source, Skill, Status, Age columns
- [ ] `kubectl describe skill {name}` shows full status fields
- [ ] Short name `sk` works: `kubectl get sk`

### Install Flow
- [ ] Create Skill CR → Controller clones repo → files appear at `/data/skills/{name}/`
- [ ] SKILL.md frontmatter parsed → status.skillName + status.skillDescription populated
- [ ] status.phase transitions: Pending → Fetching → Ready
- [ ] status.lastFetched set to current time
- [ ] Scripts have executable permissions
- [ ] All files are world-readable

### Error Cases
- [ ] Source with < 3 segments → status=Error with clear message
- [ ] Non-existent repo → status=Error with git stderr
- [ ] Valid repo, non-existent path → status=Error
- [ ] Path exists but no SKILL.md → status=Error
- [ ] SKILL.md missing `name` → status=Error
- [ ] SKILL.md missing `description` → status=Error
- [ ] Git clone timeout → status=Error

### Uninstall Flow
- [ ] Delete Skill CR → `/data/skills/{name}/` removed from PVC
- [ ] Next Agent Job → skill not symlinked
- [ ] Running Agent Job → dangling symlink, agent ignores

### Update Flow
- [ ] Delete + recreate CR → old files removed, fresh clone, new files installed
- [ ] Status shows new lastFetched timestamp

### Multi-Skill from Same Repo
- [ ] Two CRs from `vercel-labs/agent-skills` → both installed independently
- [ ] Delete one → other unaffected
- [ ] Both show correct skillName/skillDescription from their own SKILL.md

### Agent Integration
- [ ] Skills symlinked into `.claude/skills/` in session dir
- [ ] Skills symlinked into `.agents/skills/` in session dir
- [ ] Claude Code discovers skills (metadata scan)
- [ ] Relevant skill loaded when prompt matches description
- [ ] Irrelevant skills NOT loaded (verify via agent.log token usage)
- [ ] Skill scripts executable from within agent session
- [ ] Skill references readable from within agent session
