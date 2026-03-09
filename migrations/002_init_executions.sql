-- Workflow executions: one row per run
CREATE TABLE workflow_executions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workflow_id UUID NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
    workflow_version INTEGER,
    status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed')),
    context JSONB NOT NULL DEFAULT '{}',
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ
);

CREATE INDEX idx_executions_workflow_id ON workflow_executions(workflow_id);
CREATE INDEX idx_executions_workflow_version ON workflow_executions(workflow_version);
CREATE INDEX idx_executions_status ON workflow_executions(status);
CREATE INDEX idx_executions_started_at ON workflow_executions(started_at);
