CREATE INDEX IF NOT EXISTS idx_usage_provider_ts ON usage_event(provider, timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_session     ON usage_event(session_id);
CREATE INDEX IF NOT EXISTS idx_usage_project     ON usage_event(project_path);
CREATE INDEX IF NOT EXISTS idx_usage_model       ON usage_event(model);
CREATE INDEX IF NOT EXISTS idx_usage_ts          ON usage_event(timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_client      ON usage_event(client);

-- Growth system indexes (snapshot-isolated model)
CREATE INDEX IF NOT EXISTS idx_pet_active        ON pet(is_active);
CREATE INDEX IF NOT EXISTS idx_pet_template      ON pet(template_id);
CREATE INDEX IF NOT EXISTS idx_xp_event_pet_time ON xp_event(pet_id, occurred_at);
CREATE INDEX IF NOT EXISTS idx_xp_event_source   ON xp_event(source_type, source_ref);
CREATE UNIQUE INDEX IF NOT EXISTS idx_xp_event_dedup ON xp_event(pet_id, source_type, source_ref);
