-- V4: rescue_jobs.dispatch_token — fencing token for at-least-once dispatch.
--
-- `agent.prompt` has an ambiguous outcome window: the request can reach
-- Herdr (and the helper) while the response is lost to a timeout, a closed
-- connection, or a daemon crash before `running` is recorded. When the job
-- is later re-dispatched, a fresh token is minted; the stale helper's
-- `claim`/report calls are then rejected, so two helpers cannot both
-- believe they own the rescue. NULL on rows assigned before this migration
-- (treated as unfenced — claim/report validate the helper id only).

ALTER TABLE rescue_jobs ADD COLUMN dispatch_token TEXT;
