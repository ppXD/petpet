# Ceremony Spec

Ceremonies are little scripted moments — egg-hatching, level-up celebrations, special endgame cutscenes — that play when a pet enters a new stage. They are **data, not code**: declared as JSON in the database, dispatched by a small frontend handler registry. Adding a new ceremony to an existing species is a SQL change with zero code. Adding a new effect *kind* is one small frontend handler.

This doc is the contract between (a) the seed.sql authors, (b) the frontend dispatcher, and (c) future community contributors building their own pet species.

## Where ceremonies live

A ceremony is one entry in a `pet_stage.metadata.on_enter` array. The shape:

```json
{
  "idle": "flicker_idle",
  "on_enter": [
    { "kind": "shake", "for": "2s" },
    { "kind": "burst", "after": "1s", "count": 30 },
    { "kind": "modal", "after": "5s",
      "title":  "Your egg hatched into a Flicker!",
      "calls":  "pet_finalize_naming" }
  ]
}
```

| Field | Purpose |
|---|---|
| `idle` | Frame-tag name in the species's sprite sheet, used for the looping idle animation while sitting at this stage |
| `on_enter` | Array of effect specs, fired in parallel when a `pet://level_up` event lands with `level_after` matching this stage |

That is the whole top-level shape. Two fields. Everything else lives inside each entry of `on_enter`.

A second location, `species.metadata`, holds species-wide data:

```json
{
  "default_pet_name": "Burny",
  "difficulty":       "medium"
}
```

`default_pet_name` is consumed by `pick_egg` when the user skips the name prompt. Other keys are free for future use.

## Effect entry shape

Every entry has two universal fields plus kind-specific fields:

```json
{ "kind": "...",  "after": "0.5s",  "for": "2s",  ... }
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `kind` | string | — required | Picks the handler |
| `after` | duration string | `"0s"` | Delay from `level_up` event before this effect fires |
| `for` | duration string | kind-dependent | How long this effect plays |

**Duration strings** parse `<number><unit>` with units `ms` or `s`. Examples: `"600ms"`, `"2s"`, `"0.7s"`, `"1.5s"`. Numbers may be fractional.

**Colors** are CSS hex strings: `"#ff7028"`. Always 7 chars (`#RRGGBB`) — no shorthand, no alpha (use the kind's own opacity controls if it has them).

## Effect kinds — full catalog

### `shake` — sprite trembles in place
```json
{ "kind": "shake", "for": "1.5s", "intensity": 3 }
```
| Field | Default | Notes |
|---|---|---|
| `for` | `"1.5s"` | Shake fades out toward the end |
| `intensity` | `3` | Pixel-amplitude (1 = subtle, 5 = violent) |

### `crack` — eggshell cracks progressively appear
```json
{ "kind": "crack", "after": "1s", "for": "1.2s" }
```
| Field | Default | Notes |
|---|---|---|
| `for` | `"1.2s"` | Cracks reveal one stroke at a time |
| | | (Auto-themed to match species sprite colors.) |

### `flash` — radial color flash from sprite center
```json
{ "kind": "flash", "color": "#fff8c8", "for": "0.5s" }
```
| Field | Default | Notes |
|---|---|---|
| `color` | `"#fff8c8"` | Center color |
| `for` | `"0.5s"` | Fade-out duration |
| `size` | `"medium"` | `"small"` / `"medium"` / `"large"` |

### `ring` — expanding pulse ring
```json
{ "kind": "ring", "color": "#ffc850", "for": "1s" }
```
| Field | Default | Notes |
|---|---|---|
| `color` | `"#fff8c8"` | Ring color |
| `for` | `"1s"` | Expansion + fade |

### `burst` — particle explosion outward
```json
{ "kind": "burst",
  "count": 30,
  "colors": ["#ff7028","#ffc850","#fff8c8"],
  "for": "1.2s",
  "gravity": 200 }
```
| Field | Default | Notes |
|---|---|---|
| `count` | `20` | Number of particles |
| `colors` | species theme | Random pick per particle |
| `for` | `"1.2s"` | Particle lifetime |
| `gravity` | `200` | Vertical pull (px/s²); `0` = no gravity |

### `sparkle` — drifting upward star pixels
```json
{ "kind": "sparkle", "for": "1.5s",
  "colors": ["#fde047","#ffffff"] }
```
| Field | Default | Notes |
|---|---|---|
| `count` | `12` | Number of sparkles |
| `colors` | `["#fde047","#ffffff"]` | Twinkle alternates star/dot shape |
| `for` | `"1.5s"` | Drift duration |

### `confetti` — colorful rain falling from above
```json
{ "kind": "confetti", "for": "2.5s",
  "colors": ["#ff7028","#a78bfa","#6ee7b7","#fde047"] }
```
| Field | Default | Notes |
|---|---|---|
| `count` | `30` | Confetti pieces |
| `colors` | rainbow | Rotates as they fall |
| `for` | `"2.5s"` | How long pieces live before fading |

### `bubble` — pixel speech-bubble text above sprite
```json
{ "kind": "bubble", "text": "Hi I am {{pet.name}}!", "for": "3s" }
```
| Field | Default | Notes |
|---|---|---|
| `text` | required | Supports template variables (see below) |
| `for` | `"3s"` | How long the bubble stays before fading |
| `position` | `"above"` | `"above"` / `"below"` / `"right"` |

### `modal` — blocking React modal dialog
```json
{ "kind": "modal",
  "title":  "Your egg hatched into a Wisp!",
  "body":   "Last chance to name your companion.",
  "calls":  "pet_finalize_naming",
  "confirm_label": "Confirm name",
  "cancel_label":  "Skip",
  "skippable": true }
```
| Field | Default | Notes |
|---|---|---|
| `title` | required | Heading text (template vars supported) |
| `body` | — | Body text (template vars supported) |
| `calls` | — | Tauri command name to invoke on Confirm. The command receives `{ pet_id, name? }`. Omit for info-only modals. |
| `confirm_label` | `"OK"` | Confirm button label |
| `cancel_label` | `"Skip"` | Cancel/Skip button label |
| `skippable` | `true` | If `false`, no Skip button; user must Confirm |

### `tint` — full-sprite color overlay (damage flash etc.)
```json
{ "kind": "tint", "color": "#ff4040", "for": "0.6s" }
```
| Field | Default | Notes |
|---|---|---|
| `color` | required | Overlay color |
| `for` | `"0.6s"` | Fade-out time |
| `opacity` | `0.7` | Peak opacity at fire moment |

### `custom` — hardcoded React component (escape hatch)
```json
{ "kind": "custom",
  "id":   "voidlord_apotheosis",
  "sheet":"onyx",
  "tag":  "voidlord_apotheosis",
  "for":  "30s" }
```
| Field | Default | Notes |
|---|---|---|
| `id` | required | Identifies a registered handler in the frontend dispatcher |
| (other) | — | All fields pass through as props to the handler |

Use `custom` only when the moment is truly unique to a species (an apotheosis sequence, a multi-stage cutscene with branching logic). For everything else, compose the generic kinds.

## Template variables

Any string field — `text`, `title`, `body` — supports `{{ ... }}` substitution. Available variables:

| Variable | Expands to |
|---|---|
| `{{pet.name}}` | The pet's current name |
| `{{pet.age}}` | Days since `pet.born_at` (e.g. `"12 days"`) |
| `{{stage.name}}` | The stage display name (e.g. `"Flicker"`) |
| `{{stage.level}}` | The numeric level (e.g. `3`) |
| `{{species.name}}` | The species display name (e.g. `"Ember"`) |

Unknown variables stay literal (`{{foo}}` if `foo` is undefined) — no crash, just visible junk so authors notice typos.

## Authoring a new species

1. **Add a `species` row** with `id`, `name`, `egg_sprite_key`, and optional `metadata.default_pet_name`.
2. **Add nine `pet_stage` rows** (one per level 0-8), each with `xp_required`, `sprite_key`, and `metadata` declaring `idle` + `on_enter`.
3. **Add `xp_rule` rows** to differentiate your species. Strategies:
   - **Minimal** — define zero rules. Your species falls back to global rules (tier-based multipliers, model-specific bonuses). Difficulty is purely the `pet_stage.xp_required` curve.
   - **Themed** — add a few species rules (priority ≥ 150) that boost certain token weights for your archetype. See seed.sql for Ember (output-heavy) and Onyx (reasoning-heavy) examples.
4. **Drop a `sheet.png` + `sheet.json`** in `desktop/src/assets/species/<egg_sprite_key>/`. Frame-tag names must match the `idle` + ceremony `tag` references.

That is the entire checklist. No code change needed unless you also want a new `kind` of effect.

## Authoring a new effect kind (frontend)

If your ceremony needs an effect not in the catalog above:

1. **Pick a name** — short, lowercase, descriptive: `snow`, `rain`, `glitch`.
2. **Write a 30-50 line React component** that accepts the effect's config object as props and renders the visual. Example shape:
   ```tsx
   export function SnowFx({ count = 20, for: dur = "2s" }: SnowProps) {
     // canvas-based pixel rain, decays after `dur`
   }
   ```
3. **Register it**:
   ```tsx
   const handlers: Record<string, CeremonyHandler> = {
     overlay:  GenericOverlay,
     ring:     RingPulse,
     // ...
     snow:     SnowFx,   // ← your new kind
   };
   ```
4. **Use it in seed.sql or any species pack**:
   ```json
   { "kind": "snow", "count": 50, "for": "3s" }
   ```

That is the entire flow. Backend never knew the new kind existed — it just shipped the JSON to the frontend, which found `handlers.snow` in its registry.

## Dispatcher contract (frontend side)

The dispatcher is a tiny function. Reproduced here so authors know exactly what happens to their JSON:

```ts
function dispatchCeremony(spec: CeremonySpec, ctx: CeremonyContext) {
  const delayMs = parseDuration(spec.after ?? "0s");
  setTimeout(() => {
    const handler = handlers[spec.id as string]  // 1. specific id wins
                 ?? handlers[spec.kind];          // 2. fallback to kind
    if (!handler) {
      console.warn(`No handler for ceremony`, spec);
      return;
    }
    const interpolated = interpolateStrings(spec, ctx);
    handler.fire(interpolated, ctx);
  }, delayMs);
}

// On pet://level_up event:
const ceremonies = stageMetadata?.on_enter ?? [];
ceremonies.forEach(spec => dispatchCeremony(spec, { pet, stage, species }));
```

Five lines of substance. Each entry in `on_enter` runs in parallel — each handles its own `after` delay. There is no inter-entry "wait for previous to finish" — schedule everything off the same `t0` (the level_up moment) using each entry's `after`. If you need sequencing, use `after` to space them out yourself.

## Style guide for community packs

- **Time language**: prefer `"0.5s"` over `"500ms"` for sub-second values readers can scan quickly. Use `"600ms"` only when you genuinely mean "less than a second but more than half."
- **Don't fire heavy effects faster than every 200ms** — particles + flashes overlap fine, but a `burst` every frame will look like a glitch, not a celebration.
- **Last-resort `custom`** — if you can compose the moment from generic kinds, do that. `custom` is for moments that are truly one-of-a-kind in your pack (a one-time apotheosis, a story branching cutscene). Overusing `custom` defeats the data-driven design.
- **Keep `text` short** — pixel bubbles overflow at ~20 characters. Use `\n` for two-line bubbles only when essential.
- **Match colors to the species palette** — every species's `sheet.png` defines a 4-5 color palette; cite those hex codes in burst/confetti `colors` arrays so the celebration feels of-the-species rather than off-the-shelf.

## Example: minimal-to-elaborate

**Minimal one-effect ceremony** — just a sparkle when reaching this stage:
```json
{ "idle": "wisp_idle", "on_enter": [ { "kind": "sparkle" } ] }
```

**Standard level-up** (compose 3-4 effects):
```json
{ "idle": "sparkling_idle",
  "on_enter": [
    { "kind": "flash",   "color": "#fff8c8", "for": "0.5s" },
    { "kind": "ring",    "color": "#ffc850", "for": "1s" },
    { "kind": "sparkle", "after": "0.3s" },
    { "kind": "bubble",  "after": "0.5s", "text": "{{pet.name}} grew up!", "for": "2.5s" }
  ] }
```

**Full hatch** (the 10-second cutscene users see once):
```json
{ "idle": "flicker_idle",
  "on_enter": [
    { "kind": "shake",    "for": "3s", "intensity": 4 },
    { "kind": "crack",    "after": "1s",   "for": "2s" },
    { "kind": "burst",    "after": "3s",   "count": 30,
      "colors": ["#ff7028","#ffc850","#ff9050","#7a1a08"] },
    { "kind": "flash",    "after": "5s",   "color": "#fff8c8", "for": "0.7s" },
    { "kind": "ring",     "after": "5s",   "color": "#ffc850", "for": "1.2s" },
    { "kind": "confetti", "after": "5.5s", "for": "3s" },
    { "kind": "modal",    "after": "5s",
      "title":  "Your egg hatched into a Flicker!",
      "body":   "Last chance to name your companion.",
      "calls":  "pet_finalize_naming",
      "confirm_label": "Confirm name",
      "cancel_label":  "Skip" }
  ] }
```

**Hardcoded epic** (Onyx L8 — community is unlikely to need this):
```json
{ "idle": "voidlord_idle",
  "on_enter": [
    { "kind": "custom",
      "id":    "voidlord_apotheosis",
      "sheet": "onyx",
      "tag":   "voidlord_apotheosis",
      "for":   "30s" },
    { "kind": "modal", "after": "30s",
      "title": "Voidlord ascends.",
      "body":  "{{pet.name}} has reached the apex. No further evolution is possible.",
      "skippable": false,
      "confirm_label": "Bow" }
  ] }
```

## Versioning

The schema is additive: new effect kinds, new optional fields, new template variables can be added without breaking existing packs. If your pack uses an unknown kind, the dispatcher logs a warning and skips it — your other ceremonies still play.

If a future release renames or removes a field, it will be announced in `CHANGELOG.md` with a migration note. Removed kinds will remain in the dispatcher as deprecated for at least one minor version.

## See also

- `src/db/seed.sql` — the canonical example, three species with the full range of ceremony patterns
- `demo/pixel_animation_demo.html` — interactive preview of every effect kind
- `desktop/src-tauri/src/lib.rs` — `pet_finalize_naming` and other backend commands callable from `modal.calls`
