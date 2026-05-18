# `.petpet` archive format — specification

`.petpet` is the file extension for petpet's shareable archive. It's a
**renamed `.zip`** — same pattern as `.vsix` (VS Code extensions),
`.crx` (Chrome), `.vrm` (avatars), `.jar` (Java).

Two kinds, discriminated by the manifest:

| `kind` | Contents | Use case |
|---|---|---|
| `template` | design only — sprites, JSON | publish a pet design to the community |
| `pet` | template + per-user snapshot + XP history | move a raised companion between machines |

The same import flow handles both — the manifest tells the importer
which lane.

## Archive layout

```
example.petpet  (= example/ zipped)
├── manifest.json                ← REQUIRED
├── template.json                ← REQUIRED
├── levels.json                  ← REQUIRED
├── rules.json                   ← REQUIRED
├── stages/
│   ├── stage_0/
│   │   ├── stage.json           ← REQUIRED
│   │   ├── on_enter.json        ← optional — ceremony script
│   │   └── sprite.png           ← optional (egg often has none)
│   ├── stage_1/ ...
│   └── stage_9/ ...
├── pet/                         ← REQUIRED when kind == "pet"
│   ├── pet.json                 ← the PetDoc snapshot
│   └── xp_events.jsonl          ← one JSON event per line, replayable
└── README.md                    ← optional, displayed in-app
```

## `manifest.json`

Deliberately tiny. Only the fields the importer needs to make routing
and compatibility decisions **before reading any other file**:

```json
{
  "$schema": "petpet/v1",
  "kind": "template"
}
```

For pet archives, an additional `pet_summary` block lets the import
confirmation dialog show "Restore Tofu (Lv. 34, 12 days)?" without
unzipping `pet/pet.json` first:

```json
{
  "$schema": "petpet/v1",
  "kind": "pet",
  "pet_summary": {
    "level": 34,
    "total_xp": 12345,
    "days_raised": 12
  }
}
```

### Required vs optional

| Field | Required | Purpose |
|---|---|---|
| `$schema` | **yes** | `"petpet/v<major>[.<minor>]"`. Compat gate. |
| `kind` | **yes** | `"template"` or `"pet"`. Import lane selector. |
| `pet_summary` | only when `kind == "pet"` | Cached stats for the confirmation dialog. |

**Everything else** about the template (`id`, `name`, `version`,
`author`, `description`, `tags`, `license`, `homepage`,
`min_petpet_version`) lives in `template.json.meta` — the file
authors are already editing. No duplication.

## `template.json.meta` — author metadata

The same file that defines species / stages / theme also carries the
display + discovery metadata:

```json
{
  "schema": "petpet-pet/v1",
  "meta": {
    "id": "mars.drakon",
    "name": "Drakon",
    "version": "1.0.0",
    "description": "A mist-born dragon that grows with every prompt.",
    "author": { "name": "Mars", "url": "https://github.com/mars" },
    "tags": ["dragon", "mist", "fantasy"],
    "license": "MIT",
    "homepage": "https://github.com/mars/drakon-template",
    "min_petpet_version": "0.1.0"
  },
  "species": { ... },
  "levels": { ... },
  "stages": [ ... ]
}
```

**Required in `meta`**: `id`, `name`, `version`. The rest is optional.

`id` follows VS Code marketplace convention: `<author>.<name>`,
lowercase, ASCII, dot-separated. Globally unique-ish, namespaced so
two authors' "dragon" templates don't collide.

## `pet/pet.json`

The `PetDoc` snapshot (existing format, no changes). Carries the
pet's name, birth date, full stage graph, level curve, theme — all
frozen at export time so the recipient gets the exact pet, even if
the template is later updated upstream.

## `pet/xp_events.jsonl`

One JSON event per line. Chronological (oldest first). Same shape as
the in-DB `xp_event` row:

```jsonl
{"id":"01HX...","pet_id":"src","occurred_at":"2026-05-04T09:30:12Z","source_type":"usage","source_ref":"u_abc","xp_delta":12,"reason":"prompt","rule_id":"r1","origin_device_id":"laptop-1"}
{"id":"01HX...","pet_id":"src","occurred_at":"2026-05-04T09:30:14Z","source_type":"activity","source_ref":null,"xp_delta":8,"reason":"tool_use_end","rule_id":"r2","origin_device_id":"laptop-1"}
```

On import the receiving machine:
1. Allocates a fresh local `pet_id`
2. Reads each line, rewrites `pet_id` to the local one
3. Inserts via the same `XpEventWriter::replay` path that lives
   ingestion uses — no schema special-cases

**Self-healing**: a single malformed or unreadable line is *skipped
with a warning*, not a hard error. A user's long-raised companion
should never become un-importable because one row decoded oddly.

## Versioning and backward compatibility

### Schema axes (independent)

| Axis | Format | Bumps when |
|---|---|---|
| Archive `$schema` major | `v1`, `v2`, … | the **shape** of the format changes incompatibly |
| Archive `$schema` minor | `v1.1`, `v1.2`, … | new **optional** fields / files added |
| Template `version` | semver (`1.0.0`) | author-controlled, independent of schema |

### Compatibility promise

- **Forward** (old file → new app): any `v1.*` file loads in any
  future `v1.*` build. Future `v2` builds keep `v1` support for at
  least **two major releases** so old exports never become bricks.
- **Backward** (new file → old app): old app refuses with a clear
  *"made for petpet ≥ X — update to load"* error if the `$schema`
  major is unknown. Unknown minor → load it; ignore unknown fields.

### Rules for evolving the format

```
✅ ALLOWED (minor bump)
   - Add optional fields to manifest.json or any subfile
   - Add new files (e.g. stages/stage_N/combat.json)
   - Add new `kind` values
   - Add new `source_type` values in xp_events.jsonl

❌ FORBIDDEN (would require major bump + multi-version deprecation)
   - Remove or rename any existing field
   - Make an optional field required
   - Change the semantics of an existing field
   - Repurpose a `kind` value
```

### Migration path when `v2` ships (someday)

1. Build a CLI tool: `petpet migrate old.petpet --to v2` rewrites a
   `v1` archive in-place.
2. `v2` apps continue reading `v1` archives directly for at least two
   major releases after `v2`'s release.
3. Deprecation timeline published before `v1` read support is
   dropped.

## Safety / validation at import

| Check | Action on failure |
|---|---|
| `manifest.json` missing | refuse — *"not a petpet archive"* |
| `$schema` newer major | refuse — *"made by a newer petpet — update"* |
| `$schema` malformed | refuse — *"archive schema malformed"* |
| Total archive > 50 MB | refuse — *"archive too large"* |
| Per-file > 10 MB | skip that file with warning |
| Zip-slip path (`..` / absolute) | skip with warning, continue with the rest |
| `template.json` missing | refuse — *"missing template.json"* |
| `pet/pet.json` missing (when `kind == pet`) | refuse — *"pet archive missing pet.json"* |
| `xp_events.jsonl` corrupt line | skip line with warning, continue replay |
| Same `id` + same `version` already installed | no-op (matches `npm install` semantics) |
| Same `id`, newer `version` imported | replace with confirmation |
| Same `id`, older `version` | keep installed, warn that downgrade was skipped |

## Authoring workflow

A "template author" lives in a folder under
`~/.petpet/templates/<id>/` — identical to the runtime layout. They
edit JSON in any editor, drop PNGs into `stages/stage_N/`, and click
**Export…** in the egg picker to bundle the folder into a `.petpet`
for distribution. No build step.

For new authors, **Create new template…** in the egg picker
scaffolds a starter folder.

## Distribution

For v1 of petpet, **no central registry**. `.petpet` files are
distributed via:

- Direct file transfer (Discord, email, USB)
- GitHub releases attached to a template repo
- Personal websites

A future community registry is possible without changing the file
format — the registry just aggregates pointers to `.petpet` URLs.
