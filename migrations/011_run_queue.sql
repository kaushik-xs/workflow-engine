-- Runs waiting for a worker (see `run_queue`). A row is a run that has been accepted but not
-- started: `POST /webhook/:id?async=true` now, schedules later. Its id is the execution id the
-- run will have, so the caller can follow it before it starts. A worker claims a row by
-- deleting it and then runs it; there are no retries, so a run starts at most once.
CREATE TABLE workflow_run_queue (
    id UUID PRIMARY KEY,
    workflow_id UUID NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
    -- `Webhook` and `Trigger`, the trigger's part of the initial context.
    trigger_context JSONB NOT NULL,
    trigger TEXT NOT NULL CHECK (trigger IN ('webhook', 'schedule', 'workflow_call')),
    schedule_id UUID,
    -- Overrides the workflow's persistence mode. NULL: use the workflow's.
    persistence TEXT CHECK (persistence IN ('full', 'errors_only', 'none')),
    trace_id TEXT NOT NULL,
    -- Not started before this time.
    available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_run_queue_available_at ON workflow_run_queue(available_at);
