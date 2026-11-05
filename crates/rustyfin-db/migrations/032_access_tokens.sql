CREATE TABLE IF NOT EXISTS access_tokens (
    token_hash   TEXT PRIMARY KEY NOT NULL,
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id    TEXT NOT NULL DEFAULT '',
    device_name  TEXT NOT NULL DEFAULT '',
    app_name     TEXT NOT NULL DEFAULT '',
    app_version  TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_access_tokens_user ON access_tokens(user_id, device_id);
