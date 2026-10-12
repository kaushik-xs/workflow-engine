-- What started each run: a webhook call, a schedule, or a WorkflowCall node in another run.
-- Existing rows were all started by a webhook or a WorkflowCall; they are recorded as
-- `webhook`, since the two cannot be told apart after the fact.
ALTER TABLE workflow_executions
    ADD COLUMN trigger TEXT NOT NULL DEFAULT 'webhook'
    CHECK (trigger IN ('webhook', 'schedule', 'workflow_call'));

-- The schedule that started a `schedule` run. No foreign key: a run keeps its history
-- after the schedule is deleted.
ALTER TABLE workflow_executions ADD COLUMN schedule_id UUID;

CREATE INDEX idx_executions_trigger ON workflow_executions(trigger);
CREATE INDEX idx_executions_schedule_id ON workflow_executions(schedule_id) WHERE schedule_id IS NOT NULL;
