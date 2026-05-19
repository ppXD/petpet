# petpet — Repo Conventions

Project-specific guidance for AI agents and human contributors. Composes
with `~/.claude/CLAUDE.md` global rules; this file pins the petpet
specifics.

---

## Commit / PR / comment hygiene — no local-decision markers

Every artifact that lands in `git log`, on the PR page, or in source
comments must read cleanly to a cold reader years later. Internal
sequencing, sprint codes, and phase labels age into noise — they
describe where the work sat in a plan that no longer exists, not what
the change actually does.

### Banned in PR titles + commit subjects + branch names

| Pattern | Example |
|---|---|
| Phase markers | `Phase 2 B5: …` / `Phase 1.1: …` / `xp/phase-3-…` |
| Step counters | `Step 3/6: …` / `PR-5 of N: …` |
| Local PR codes | `PR XP-1: …` / `PR B2: …` (treat letter+number sequences as suspect) |
| Sprint / ticket IDs | `Sprint 5 …` / `[TICKET-123] …` |
| Bracketed initiative prefixes | `[V2 rollout] …` / `[Phase 2.3] …` |

### Banned in PR descriptions + commit bodies + code comments

- "This is the Nth PR in the X rollout"
- "Part of the Y initiative"
- References to internal docs / tracking spreadsheets that won't
  exist publicly
- Internal status emoji (🟢 / 🟡 / ⚪ etc.)

### Examples from this repo

| ❌ Banned | ✅ Good |
|---|---|
| `Phase 2 B5: import-progress banner + auto-refresh` | `Import-progress banner + auto-refresh on completion` |
| `PR XP-1: tighten divisor` | `Tighten XP algorithm divisor + caps + floor` |
| `xp/algorithm-rebalance-step-3-of-4` | `xp/algorithm-rebalance` |
| `Phase 2 B1: add EmitMode::ImportHistorical` | `Add EmitMode::ImportHistorical + dual cursor lanes` |

### Where to put the local sequencing instead

| Purpose | Tool |
|---|---|
| Grouping related PRs | GitHub **labels** (`feature`, `release`, etc. via `.github/release.yml`) |
| Tracking sequence / status | GitHub **project board** or **milestone** |
| WIP indicator | GitHub **draft PR** (don't put `[WIP]` in the title) |
| Linking related work | `Closes #N`, `Refs #N` in the body |

### Why the rule

A maintainer running `git blame` in 3 years sees `Phase 2 B5` and has
no context. They see `Import-progress banner + auto-refresh on
completion` and know exactly what changed. The artifact must stand
alone for the cold reader.

This rule applies to AI-generated commits, comments, and PR text
without exception. Pick a description that captures *what the change
does* and *why* — never *where it sits in the plan*.

---

## Versioning + release artifact names

Bundle filenames must be tag-driven, not derived from `tauri.conf.json`
state. `desktop/src-tauri/tauri.conf.json` may carry any version during
development (typically the next planned tag); the **release workflow
auto-syncs that version from the git tag** before invoking
`tauri-action`. This makes accidental version drift impossible — push
`v0.3.0`, get `petpet_0.3.0_*` assets.

The `tidy-asset-names` job then strips locale + arch suffixes:

- `petpet_0.X.Y_universal.dmg` → `petpet_0.X.Y.dmg`
- `petpet_0.X.Y_x64_en-US.msi` → `petpet_0.X.Y_x64.msi`
- `petpet_universal.app.tar.gz` → `petpet_0.X.Y.app.tar.gz`

If a release ships with `_universal` or `_en-US` still in the names,
the tidy job's `${VERSION}` glob didn't match — usually because the
version-sync step didn't run or `tauri-action` produced files at the
wrong version. Fix the workflow, don't manually rename in the GitHub
UI.
