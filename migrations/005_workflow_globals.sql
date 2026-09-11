-- Workflow globals: per-tenant key/value store exposed to executions as `global.*`.
-- Values are JSONB so a global can hold any JSON (string, number, object, array).
CREATE TABLE workflow_globals (
    tenant TEXT NOT NULL,
    key TEXT NOT NULL,
    value JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, key)
);

CREATE INDEX idx_workflow_globals_tenant ON workflow_globals(tenant);
