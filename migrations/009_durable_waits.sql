-- Durable waits: a node can suspend its run until a time passes and/or a signal arrives
-- (see the Wait node). The run is saved as `waiting` and resumed later by the timer worker
-- or by POST /signals; `cancelled` ends a waiting or paused run without finishing it.
ALTER TABLE workflow_executions DROP CONSTRAINT workflow_executions_status_check;
ALTER TABLE workflow_executions
    ADD CONSTRAINT workflow_executions_status_check
    CHECK (status IN ('running', 'completed', 'failed', 'paused', 'waiting', 'cancelled'));

ALTER TABLE workflow_steps DROP CONSTRAINT workflow_steps_status_check;
ALTER TABLE workflow_steps
    ADD CONSTRAINT workflow_steps_status_check
    CHECK (status IN ('pending', 'running', 'completed', 'failed', 'skipped', 'waiting'));

-- What a waiting run is waiting for. Runs are sequential, so an execution waits on at most
-- one node at a time. The row is deleted by whoever resumes the run (timer or signal), so
-- deleting it is what decides a race between the two.
CREATE TABLE workflow_waits (
    execution_id UUID PRIMARY KEY REFERENCES workflow_executions(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    -- Loop iteration of the waiting node ("" at the top level), as in workflow_steps.
    iteration TEXT NOT NULL DEFAULT '',
    -- Resume at this time (a delay, or a timeout for a signal wait). NULL: no timer.
    wake_at TIMESTAMPTZ,
    -- Resume when a signal with this key arrives. NULL: no signal.
    correlation_key TEXT,
    -- Optional expression over the signal (`signal.payload`) that must be truthy to resume.
    filter TEXT,
    -- The node's own data, handed back to it on resume.
    state JSONB NOT NULL DEFAULT '{}',
    -- Resume one step and pause again (the run was started in step mode).
    step_mode BOOLEAN NOT NULL DEFAULT false,
    trace_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (wake_at IS NOT NULL OR correlation_key IS NOT NULL)
);

CREATE INDEX idx_waits_wake_at ON workflow_waits(wake_at) WHERE wake_at IS NOT NULL;
CREATE INDEX idx_waits_correlation_key ON workflow_waits(correlation_key) WHERE correlation_key IS NOT NULL;
