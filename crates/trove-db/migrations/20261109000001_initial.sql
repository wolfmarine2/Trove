-- Portable schema: runs unchanged on SQLite and PostgreSQL (incl. openGauss).
-- Conventions: TEXT UUID primary keys, BIGINT epoch-millisecond timestamps,
-- BIGINT 0/1 booleans, JSON stored as TEXT.

CREATE TABLE repositories (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    format      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    online      BIGINT NOT NULL DEFAULT 1,
    description TEXT NOT NULL DEFAULT '',
    config      TEXT NOT NULL DEFAULT '{}',
    created_at  BIGINT NOT NULL,
    updated_at  BIGINT NOT NULL
);

CREATE TABLE blobs (
    sha256     TEXT PRIMARY KEY,
    sha1       TEXT NOT NULL,
    md5        TEXT NOT NULL,
    sha512     TEXT NOT NULL,
    size       BIGINT NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE TABLE assets (
    id                 TEXT PRIMARY KEY,
    repo_id            TEXT NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    path               TEXT NOT NULL,
    blob_sha256        TEXT NOT NULL REFERENCES blobs (sha256),
    content_type       TEXT NOT NULL,
    uploader           TEXT NOT NULL DEFAULT '',
    attributes         TEXT NOT NULL DEFAULT '{}',
    created_at         BIGINT NOT NULL,
    updated_at         BIGINT NOT NULL,
    last_downloaded_at BIGINT,
    UNIQUE (repo_id, path)
);

CREATE INDEX idx_assets_blob ON assets (blob_sha256);
CREATE INDEX idx_assets_updated ON assets (updated_at);

CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE,
    display_name  TEXT NOT NULL DEFAULT '',
    email         TEXT NOT NULL DEFAULT '',
    role          TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    disabled      BIGINT NOT NULL DEFAULT 0,
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL
);

CREATE TABLE tokens (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    token_hash   TEXT NOT NULL UNIQUE,
    prefix       TEXT NOT NULL,
    created_at   BIGINT NOT NULL,
    expires_at   BIGINT,
    last_used_at BIGINT
);

CREATE INDEX idx_tokens_user ON tokens (user_id);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE audit_events (
    id        TEXT PRIMARY KEY,
    ts        BIGINT NOT NULL,
    actor     TEXT NOT NULL,
    action    TEXT NOT NULL,
    target    TEXT NOT NULL,
    detail    TEXT NOT NULL DEFAULT '',
    client_ip TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_audit_ts ON audit_events (ts);
