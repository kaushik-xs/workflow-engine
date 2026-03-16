-- Workflows table: store workflow definitions (React Flow JSON) as JSONB
CREATE TABLE workflows (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant TEXT NOT NULL,
    name TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    is_latest BOOLEAN NOT NULL DEFAULT true,
    definition JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_workflows_tenant ON workflows(tenant);
CREATE INDEX idx_workflows_created_at ON workflows(created_at);
CREATE INDEX idx_workflows_version ON workflows(version);
CREATE UNIQUE INDEX idx_workflows_tenant_name_version ON workflows (tenant, name, version);
CREATE INDEX idx_workflows_latest ON workflows (tenant, name) WHERE is_latest = true;
