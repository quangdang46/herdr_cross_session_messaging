-- V3: rescue_jobs.captured_at — when the TUI snapshot was taken.
-- Distinct from created_at/updated_at so consumers can reason about
-- snapshot freshness independently of job bookkeeping. Existing rows are
-- backfilled with created_at (snapshot and job were created together).

ALTER TABLE rescue_jobs ADD COLUMN captured_at INTEGER;
UPDATE rescue_jobs SET captured_at = created_at WHERE captured_at IS NULL;
