-- V6: message_nudges — persisted unanswered-mail reminder bookkeeping.
--
-- The router's nudge sweep sends `[herdr-pool] reminder` prompts for
-- delivered messages the recipient never answered. Previously the
-- per-message throttle lived only in daemon memory, so every restart
-- re-nudged the entire backlog and there was no cap on total reminders —
-- busy recipients got spammed once per thread per interval, forever.
--
-- Persisting (nudge_count, last_nudge_at) per message makes the throttle
-- survive restarts and lets the sweeper give up after a bounded number of
-- reminders.

CREATE TABLE message_nudges (
    message_id    TEXT PRIMARY KEY REFERENCES messages(id),
    nudge_count   INTEGER NOT NULL DEFAULT 0,
    last_nudge_at INTEGER NOT NULL
);
