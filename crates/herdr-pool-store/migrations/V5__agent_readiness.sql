-- V5: agents.interactive_ready / agents.launch_pending — TUI readiness.
--
-- Herdr reports whether an agent's interactive session is ready to accept
-- prompts (`interactive_ready`) and whether its launch is still pending
-- (`launch_pending`) — e.g. while `devin --resume` restores a session.
-- The bootstrapper must not send `agent.prompt` until the agent is ready,
-- otherwise the instruction can be pasted into a half-restored TUI.
--
-- Defaults preserve pre-V5 behavior: existing rows are treated as ready
-- (1) with no pending launch (0); the next AgentInfo sighting overwrites
-- both with authoritative values.

ALTER TABLE agents ADD COLUMN interactive_ready INTEGER NOT NULL DEFAULT 1;
ALTER TABLE agents ADD COLUMN launch_pending INTEGER NOT NULL DEFAULT 0;
