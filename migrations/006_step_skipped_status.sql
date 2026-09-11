-- Allow steps to be recorded as 'skipped' when a node is on a branch that the
-- flow did not take (see If / Switch nodes and reachability-based execution).
ALTER TABLE workflow_steps DROP CONSTRAINT workflow_steps_status_check;
ALTER TABLE workflow_steps
    ADD CONSTRAINT workflow_steps_status_check
    CHECK (status IN ('pending', 'running', 'completed', 'failed', 'skipped'));
