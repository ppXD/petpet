PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS usage_event (
    id                     TEXT PRIMARY KEY,
    provider               TEXT NOT NULL,
    client                 TEXT,
    session_id             TEXT NOT NULL,
    project_path           TEXT,
    git_branch             TEXT,
    model                  TEXT NOT NULL,
    timestamp              TEXT NOT NULL,
    kind                   TEXT NOT NULL,
    stop_reason            TEXT,
    tool_name              TEXT,
    tool_exit_code         INTEGER,
    tokens_input           INTEGER NOT NULL DEFAULT 0,
    tokens_output          INTEGER NOT NULL DEFAULT 0,
    tokens_cache_read      INTEGER NOT NULL DEFAULT 0,
    tokens_cache_creation  INTEGER NOT NULL DEFAULT 0,
    tokens_reasoning       INTEGER NOT NULL DEFAULT 0,
    source_file            TEXT NOT NULL,
    source_offset          INTEGER NOT NULL,
    source_line            INTEGER NOT NULL,
    ingested_at            TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS file_cursor (
    provider       TEXT NOT NULL,
    file_path      TEXT NOT NULL,
    byte_offset    INTEGER NOT NULL,
    line_index     INTEGER NOT NULL DEFAULT 0,
    updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (provider, file_path)
);

-- Heartbeat: petpet writes a row every ~30s while alive.
CREATE TABLE IF NOT EXISTS app_heartbeat (
    id          INTEGER PRIMARY KEY,
    last_alive  TEXT NOT NULL
);

-- ═══════════════════════════════════════════════════════════════
-- Growth system tables (snapshot-isolated model)
-- ═══════════════════════════════════════════════════════════════
-- Pets are filesystem-first: each pet has `~/.petpet/pets/<uuid>/`
-- containing `pet.json` (stages + rules + theme + identity) plus
-- asset copies. The DB only holds runtime state — XP event log +
-- level cache + the pet's identity row pointing at its folder.

-- Pet instances. is_active selects the one currently displayed/fed.
CREATE TABLE IF NOT EXISTS pet (
    id                  TEXT PRIMARY KEY,
    name                TEXT NOT NULL DEFAULT 'Pet',
    template_id         TEXT NOT NULL,        -- e.g. "sun" (the source template)
    snapshot_path       TEXT NOT NULL,        -- abs path to ~/.petpet/pets/<id>/
    born_at             TEXT NOT NULL,
    is_active           INTEGER NOT NULL DEFAULT 0,
    name_finalized_at   TEXT,
    origin_device_id    TEXT NOT NULL,
    metadata            TEXT NOT NULL DEFAULT '{}',
    created_at          TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
);

-- XP events (append-only, source of truth for pet growth)
CREATE TABLE IF NOT EXISTS xp_event (
    id               TEXT PRIMARY KEY,
    pet_id           TEXT NOT NULL REFERENCES pet(id) ON DELETE CASCADE,
    occurred_at      TEXT NOT NULL,
    source_type      TEXT NOT NULL,
    source_ref       TEXT,
    xp_delta         INTEGER NOT NULL,
    reason           TEXT,
    rule_id          TEXT,
    origin_device_id TEXT NOT NULL,
    metadata         TEXT NOT NULL DEFAULT '{}',
    created_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Cached state. Always recomputable from xp_event via SUM.
CREATE TABLE IF NOT EXISTS pet_state (
    pet_id         TEXT PRIMARY KEY REFERENCES pet(id) ON DELETE CASCADE,
    total_xp       INTEGER NOT NULL DEFAULT 0,
    current_level  INTEGER NOT NULL DEFAULT 0,
    last_active_at TEXT,
    recomputed_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Install identity: this device's UUID, tags every xp_event for
-- multi-device sync attribution.
CREATE TABLE IF NOT EXISTS petpet_install (
    id              TEXT PRIMARY KEY,
    first_seen_at   TEXT NOT NULL DEFAULT (datetime('now')),
    schema_version  INTEGER NOT NULL DEFAULT 1
);
