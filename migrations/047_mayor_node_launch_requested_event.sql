-- Mayor node launch lifecycle: document new chain event type.
-- Event types are enforced in Rust (`QuazarEventType` + EventValidator), not via DB CHECK.
--
-- MayorNodeLaunchRequested — citizen apply for mayor node launch (host/IP/domain).
-- Followed by existing: NodeStarted → MayorConfirmed (or MayorApplicationTimedOut).
--
-- No schema change required; this migration is a no-op marker for operators.

SELECT 1;
