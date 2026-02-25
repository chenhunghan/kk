# kk: Skill

> **Cross-references:**
> [Communication Protocol](kk-protocol.md) — file paths §5.2, §5.3; ownership §3
> [Controller](kk-controller.md) — reconciles Skill CRs, clones repos, writes to PVC
> [Agent Job](kk-agent.md) — Phase 0 symlinks skills into session directory
> [Connector](kk-connector.md) ·
> [Gateway](kk-gateway.md)

---

## Summary

A Skill is not a running process — it is a **data artifact** managed by a CRD. The Skill CRD declares that a particular skill from a git repo should be available to all Agent Jobs. The Controller handles the lifecycle (fetch on create, remove on delete), and Agent Jobs consume skills by symlinking them into the session directory.

### Touchpoints

| Component | Role with Skills |
|---|---|
| **User** | Creates/deletes Skill CRs via kubectl |
| **Controller** | Clones repo, validates SKILL.md, copies to PVC, updates CR status |
| **Agent Job** | Phase 0: symlinks `/data/skills/*` into session `.{agent}/skills/` and `.agents/skills/` |
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

The Controller parses only `name` and `description` from YAML frontmatter (see `SkillFrontmatter` in `crates/kk-controller/src/reconcilers/skill.rs`). All other frontmatter fields are silently ignored by serde.

| Field | Required | Type | Description |
|---|---|---|---|
| `name` | **Yes** | string | Unique identifier. Lowercase, hyphens allowed. e.g., `frontend-design` |
| `description` | **Yes** | string | What the skill does and when to use it. The LLM reads this to decide relevance. |

Both fields must be present and non-empty, or the Controller sets the CR to `Error`.

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

Defined in `crates/kk-controller/src/crd.rs` using `kube::CustomResource`:

```rust
#[kube(
    group = "kk.io",
    version = "v1alpha1",
    kind = "Skill",
    plural = "skills",
    shortname = "sk",
    status = "SkillStatus",
    namespaced
)]
pub struct SkillSpec {
    pub source: String,
}

pub struct SkillStatus {
    pub phase: String,
    pub message: String,
    pub skill_name: String,         // serialized as skillName
    pub skill_description: String,  // serialized as skillDescription
    pub last_fetched: String,       // serialized as lastFetched
}
```

---

## Source Format

The `spec.source` field uses a GitHub shorthand path:

```
owner/repo/path/to/skill
```

The Controller parses this with `splitn(3, '/')`:

| Part | Extraction | Example |
|---|---|---|
| Owner | `parts[0]` | `vercel-labs` |
| Repo | `parts[1]` | `agent-skills` |
| Skill path | `parts[2]` (everything after second `/`) | `skills/frontend-design` |

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
| `vercel-labs` | `expected owner/repo/path, got 'vercel-labs'` |
| `vercel-labs/agent-skills` | `expected owner/repo/path, got 'vercel-labs/agent-skills'` |
| `/repo/path` or `owner//path` or `owner/repo/` | `empty segment in source '...'` |

---

## PVC Storage

### Location

```
/data/skills/{skill-cr-metadata-name}/
```

The directory name is the **Skill CR's `metadata.name`**, NOT the `name` from SKILL.md frontmatter. This prevents collisions if two CRs reference skills with the same frontmatter name from different repos.

### Example Layout

After installing three skills:

```
/data/skills/
├── frontend-design/                   ← CR name: frontend-design
│   └── SKILL.md                       ← 0o644
│
├── vercel-deploy/                     ← CR name: vercel-deploy
│   ├── SKILL.md                       ← 0o644
│   └── scripts/
│       └── deploy.sh                  ← 0o755 (executable)
│
└── agent-browser/                     ← CR name: agent-browser
    ├── SKILL.md                       ← 0o644
    ├── scripts/
    │   └── agent-browser.sh           ← 0o755 (executable)
    └── references/
        └── api-reference.md           ← 0o644
```

### Permissions

Set by `set_skill_permissions()` in `crates/kk-controller/src/reconcilers/skill.rs` after copying:

| Path | Mode | Why |
|---|---|---|
| Directories | `0o755` (rwxr-xr-x) | World-traversable; Agent Jobs may run as a different user |
| Files named `*.sh`, `*.py`, or `run` | `0o755` (rwxr-xr-x) | Executable by agent |
| All other files | `0o644` (rw-r--r--) | World-readable |

Executable detection matches filenames anywhere in the skill tree (not just under `scripts/`).

---

## Lifecycle

### Install

```
User                    Controller              PVC                    Agent Job
 │                          │                    │                         │
 │  kubectl apply           │                    │                         │
 │  Skill CR ──────────────►│                    │                         │
 │                          │                    │                         │
 │                          │  set status:       │                         │
 │                          │  Fetching          │                         │
 │                          │                    │                         │
 │                          │  git clone         │                         │
 │                          │  --depth 1         │                         │
 │                          │  --single-branch   │                         │
 │                          │  --filter=blob:none│                         │
 │                          │  → /tmp/kk-skill-* │                         │
 │                          │                    │                         │
 │                          │  validate SKILL.md │                         │
 │                          │  parse frontmatter │                         │
 │                          │                    │                         │
 │                          │  cp -r to PVC      │                         │
 │                          │  ──────────────────►│                        │
 │                          │                    │  /data/skills/{name}/   │
 │                          │                    │                         │
 │                          │  set permissions   │                         │
 │                          │  update CR status  │                         │
 │                          │  phase: Ready      │                         │
 │                          │  skillName: ...    │                         │
 │                          │  skillDescription: │                         │
 │                          │  lastFetched: ...  │                         │
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
 │                          │                    │    → .{agent}/skills/   │
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

There is no in-place update mechanism. Delete and recreate the CR:

```bash
kubectl delete skill frontend-design -n kk
kubectl apply -f skill-frontend-design.yaml
```

This triggers:
1. Delete event → Controller removes `/data/skills/frontend-design/`
2. Create event → Controller clones fresh, validates, copies to PVC

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

Skills may include scripts executable from within the agent session. Since the session dir symlinks to `/data/skills/`, Claude can reference:
```
.claude/skills/vercel-deploy/scripts/deploy.sh
```
or the underlying path:
```
/data/skills/vercel-deploy/scripts/deploy.sh
```

Both resolve to the same file. Files matching `*.sh`, `*.py`, or named `run` are made executable by the Controller.

### Reference Files

Skills may include reference files for deep context:

```markdown
## Detailed API Reference

For full API details, read `references/api-reference.md`.
```

Claude reads `references/api-reference.md` only when it needs the detailed reference — not on every invocation.

---

## User Experience

### Install Skills

```bash
# Single skill
cat <<EOF | kubectl apply -f -
apiVersion: kk.io/v1alpha1
kind: Skill
metadata:
  name: frontend-design
  namespace: kk
spec:
  source: vercel-labs/agent-skills/skills/frontend-design
EOF

# Multiple skills (single YAML with ---)
kubectl apply -f skills.yaml
```

### List Installed Skills

```bash
kubectl get skills -n kk

# NAME               SOURCE                                              SKILL              STATUS   AGE
# frontend-design    vercel-labs/agent-skills/skills/frontend-design      frontend-design    Ready    5d
# vercel-deploy      vercel-labs/agent-skills/skills/vercel-deploy        vercel-deploy      Ready    5d
# agent-browser      vercel-labs/agent-browser/skills/agent-browser       agent-browser      Ready    3d
```

### Inspect a Skill

```bash
kubectl describe skill frontend-design -n kk

# Name:         frontend-design
# Namespace:    kk
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
kubectl exec -n kk deploy/kk-gateway -- ls /data/skills/
# frontend-design
# vercel-deploy
# agent-browser

# Read the SKILL.md
kubectl exec -n kk deploy/kk-gateway -- cat /data/skills/frontend-design/SKILL.md

# Check if scripts are executable
kubectl exec -n kk deploy/kk-gateway -- ls -la /data/skills/vercel-deploy/scripts/
# -rwxr-xr-x  1 root root  2048 Feb 20 10:30 deploy.sh
```

### Update a Skill

```bash
kubectl delete skill frontend-design -n kk
kubectl apply -f skill-frontend-design.yaml
```

### Remove a Skill

```bash
kubectl delete skill agent-browser -n kk
# Controller removes /data/skills/agent-browser/
# Next Agent Job won't symlink it
# Running Agent Jobs: dangling symlink, agent ignores
```

### Remove All Skills

```bash
kubectl delete skills --all -n kk
```

---

## Status Phases

| Phase | Meaning |
|---|---|
| (empty) | CR just created; reconcile not yet run |
| `Fetching` | Controller is cloning the repository |
| `Ready` | Skill validated and installed on PVC |
| `Error` | Something went wrong; user must delete + recreate |

```
                ┌────────┐
   CR created → │        │ (phase is empty string)
                └───┬────┘
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
             │        back to empty
             │
             │  (user deletes)
             ▼
          CR gone, PVC cleaned
```

Invalid source sets `Error` and requeues after 300 seconds. Git clone and validation failures return an error to the controller which requeues after 60 seconds.

### Error Messages

| Cause | `status.message` |
|---|---|
| Source has < 3 path segments | `expected owner/repo/path, got '{source}'` |
| Empty segment in source | `empty segment in source '{source}'` |
| Git clone exit non-zero | `git clone failed: {stderr}` |
| Git clone OS error | `git clone command error: {e}` |
| Git clone timeout | `git clone timed out after {N}s` |
| Skill path not found in repo | `skill path '{path}' not found in repo` |
| SKILL.md not in directory | `SKILL.md not found in {path}` |
| Frontmatter YAML parse error | `invalid frontmatter YAML: {e}` |
| Missing or empty `name` | `frontmatter 'name' is required` |
| Missing or empty `description` | `frontmatter 'description' is required` |
| PVC write failure | `io error: {OS error}` (propagated) |

`SKILL_CLONE_TIMEOUT` env var (default `60`): overrides the clone timeout in seconds.

---

## Phase 0: Agent Symlink Injection

Implemented in `crates/kk-agent/src/phases.rs:phase_0_skills()`.

For each entry in `/data/skills/` that is a directory containing a `SKILL.md`:

1. Create `{session_dir}/.{agent}/skills/` (e.g. `.claude/skills/`)
2. Create `{session_dir}/.agents/skills/`
3. Symlink `{session_dir}/.{agent}/skills/{name}` → `/data/skills/{name}`
4. Symlink `{session_dir}/.agents/skills/{name}` → `{session_dir}/.{agent}/skills/{name}` (chained)

The `{agent}` directory name is determined by agent type:

| Agent | Directory |
|---|---|
| Claude | `.claude` |
| Gemini | `.gemini` |
| Codex | `.codex` |

If `/data/skills/` does not exist, Phase 0 is a no-op. Existing symlinks at the target paths are removed before each new symlink is created.

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

If reproducibility is needed later, an optional `ref` field could be added:
```yaml
spec:
  source: vercel-labs/agent-skills/skills/frontend-design
  ref: v1.2.0   # optional: tag, branch, or commit SHA
```

### No Auto-Update

Skills do not automatically pull new versions. The version on disk is exactly what was fetched at CR creation time. Update = delete + recreate.

### All Skills Available to All Groups

There is no per-group skill filtering. Every Agent Job gets every skill. The LLM self-selects based on descriptions.

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
- The PVC is scoped to the kk namespace
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
