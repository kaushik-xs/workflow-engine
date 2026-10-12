-- Schedules: start a workflow on a cron expression (see `schedule`). Each engine (one per
-- environment) has its own; they are not part of a workflow's definition.
CREATE TABLE workflow_schedules (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant TEXT NOT NULL,
    -- Unique within the tenant; config sync matches schedules by name.
    name TEXT NOT NULL,
    -- The workflow is named, not referenced by id, so the schedule follows new versions.
    workflow_name TEXT NOT NULL,
    -- NULL: whichever version is latest when the schedule fires.
    workflow_version INTEGER,
    cron TEXT NOT NULL,
    -- IANA name, e.g. Asia/Kolkata. The cron expression is wall-clock time in this zone.
    timezone TEXT NOT NULL DEFAULT 'UTC',
    -- Sent to each run as `Webhook.body`.
    input JSONB NOT NULL DEFAULT '{}',
    enabled BOOLEAN NOT NULL DEFAULT true,
    overlap TEXT NOT NULL DEFAULT 'skip' CHECK (overlap IN ('skip', 'allow')),
    catch_up TEXT NOT NULL DEFAULT 'once' CHECK (catch_up IN ('skip', 'once')),
    -- Overrides the workflow's persistence mode. NULL: use the workflow's.
    persistence TEXT CHECK (persistence IN ('full', 'errors_only', 'none')),

    -- When it fires next. NULL while disabled.
    next_run_at TIMESTAMPTZ,
    -- The scheduled run still going, for `overlap = 'skip'`, and how long to assume it is
    -- (covers a worker that died mid-run).
    active_execution_id UUID,
    active_until TIMESTAMPTZ,
    -- What happened the last time it came due.
    last_run_at TIMESTAMPTZ,
    last_scheduled_for TIMESTAMPTZ,
    last_execution_id UUID,
    last_outcome TEXT CHECK (last_outcome IN ('queued', 'skipped_overlap', 'skipped_missed', 'failed_to_start')),
    last_error TEXT,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant, name),
    CHECK (enabled OR next_run_at IS NULL)
);

-- The scheduler's query: enabled schedules that are due.
CREATE INDEX idx_schedules_due ON workflow_schedules(next_run_at) WHERE enabled;
CREATE INDEX idx_schedules_workflow ON workflow_schedules(tenant, workflow_name);
