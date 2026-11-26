-- V2: peer TUI rescue jobs.
-- One open rescue job per blocked source agent (partial unique index);
-- helper_agent_id is the reservation — at most one open job may name a
-- given helper at a time (enforced in code; the index keeps lookups fast).
-- Status flow (enforced in code, CHECK is a backstop):
--   pending -> assigned -> running -> resolved
--   pending|assigned|running -> needs_human (timeout / unsafe / no helper)
--   pending|assigned|running -> failed (source gone, helper unusable)
-- `snapshot`/`snapshot_source` hold the captured TUI context handed to the
-- helper; `snapshot_source` records which read produced it ('visible',
-- 'recent-unwrapped') so consumers never assume full-transcript fidelity.

CREATE TABLE rescue_jobs (
    rescue_id       TEXT PRIMARY KEY,
    scope_key       TEXT NOT NULL,
    source_agent_id TEXT NOT NULL,
    source_pane_id  TEXT,
    helper_agent_id TEXT,
    helper_pane_id  TEXT,
    snapshot        TEXT,
    snapshot_source TEXT,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'assigned', 'running',
                                      'verifying', 'resolved', 'needs_human',
                                      'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    resolved_at     INTEGER
);

-- At most one open job per source agent — prevents duplicate rescues for
-- the same blocked episode and rescue loops (A helping B while B helps A
-- is impossible because a blocked agent is never an eligible helper).
CREATE UNIQUE INDEX idx_rescue_open_source ON rescue_jobs(source_agent_id)
    WHERE status IN ('pending', 'assigned', 'running', 'verifying');

-- Dispatch order: oldest pending first.
CREATE INDEX idx_rescue_pending ON rescue_jobs(status, created_at);
-- Helper reservation lookup: which open job (if any) names this helper.
CREATE INDEX idx_rescue_helper ON rescue_jobs(helper_agent_id, status);
-- One open job per helper — the reservation that prevents two sources
-- being rescued by the same peer concurrently (MVP: one rescue per helper).
CREATE UNIQUE INDEX idx_rescue_open_helper ON rescue_jobs(helper_agent_id)
    WHERE helper_agent_id IS NOT NULL
      AND status IN ('assigned', 'running', 'verifying');
