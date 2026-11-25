-- V1: initial pool schema.
-- One database per project scope at <state>/pools/<scope_key>/pool.db.
-- All *_at columns are unix epoch milliseconds (i64).
-- Message status flow (enforced in code, CHECK is a backstop):
--   pending -> delivered -> acknowledged
--   pending|failed -> failed (retry scheduled via next_retry_at)
--   failed -> dead after MAX_DELIVERY_ATTEMPTS failures
--   failed|dead -> pending via explicit retry()

CREATE TABLE agents (
    agent_id          TEXT PRIMARY KEY,
    scope_key         TEXT NOT NULL,
    terminal_id       TEXT,
    pane_id           TEXT,
    agent_kind        TEXT,
    name              TEXT,
    cwd               TEXT,
    status            TEXT NOT NULL DEFAULT 'unknown',
    bootstrap_version INTEGER,
    first_seen_at     INTEGER NOT NULL,
    last_seen_at      INTEGER NOT NULL,
    offline_at        INTEGER
);

CREATE TABLE threads (
    thread_id         TEXT PRIMARY KEY,
    scope_key         TEXT NOT NULL,
    subject           TEXT,
    created_at        INTEGER NOT NULL,
    last_activity_at  INTEGER NOT NULL
);

CREATE TABLE messages (
    id             TEXT PRIMARY KEY,
    thread_id      TEXT REFERENCES threads(thread_id),
    from_agent     TEXT NOT NULL,
    to_agent       TEXT NOT NULL,
    kind           TEXT NOT NULL DEFAULT 'message',
    content        TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending'
                   CHECK (status IN ('pending', 'delivered', 'acknowledged', 'failed', 'dead')),
    attempt_count  INTEGER NOT NULL DEFAULT 0,
    next_retry_at  INTEGER,
    delivered_at   INTEGER,
    acked_at       INTEGER,
    created_at     INTEGER NOT NULL
);

-- Inbox: everything addressed to an agent.
CREATE INDEX idx_messages_to_agent ON messages(to_agent, status, created_at);
-- Thread history.
CREATE INDEX idx_messages_thread ON messages(thread_id, created_at);
-- Outbox drain: due pending/failed rows.
CREATE INDEX idx_messages_outbox ON messages(status, next_retry_at);
-- Sent-mail lookup.
CREATE INDEX idx_messages_from_agent ON messages(from_agent, created_at);

CREATE INDEX idx_agents_pane ON agents(pane_id);
CREATE INDEX idx_agents_terminal ON agents(terminal_id);
CREATE INDEX idx_agents_name ON agents(name);
CREATE INDEX idx_agents_status ON agents(status);

CREATE INDEX idx_threads_activity ON threads(last_activity_at);
