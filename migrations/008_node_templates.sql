-- Node templates: a per-tenant registry of reusable, versioned node definitions.
-- A workflow node `{ "type": "template", "data": { "template": "<slug>", "version"?, "params" } }`
-- points at one and is expanded into the template's node at execution start, so the
-- template is the single source of truth. Versions are immutable: publishing a change
-- adds version N+1 and moves `is_latest`.
CREATE TABLE node_templates (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant TEXT NOT NULL,
    slug TEXT NOT NULL,
    version INTEGER NOT NULL,
    is_latest BOOLEAN NOT NULL DEFAULT true,
    name TEXT,
    description TEXT,
    node_type TEXT NOT NULL,
    config JSONB NOT NULL DEFAULT '{}',
    params JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant, slug, version)
);

CREATE INDEX idx_node_templates_latest ON node_templates (tenant, slug) WHERE is_latest = true;

-- Which workflow nodes reference which template, rewritten whenever a workflow is saved.
-- `version` NULL means the node follows the latest version.
CREATE TABLE workflow_template_usages (
    workflow_id UUID NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    tenant TEXT NOT NULL,
    slug TEXT NOT NULL,
    version INTEGER,
    PRIMARY KEY (workflow_id, node_id)
);

CREATE INDEX idx_workflow_template_usages_slug ON workflow_template_usages (tenant, slug);
