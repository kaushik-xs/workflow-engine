# Workflow Engine (Rust)

Extensible workflow execution engine with REST API. Executes user-defined workflows (e.g. from a React Flow visual editor) with pluggable node types, JMESPath expressions, and persistent state in Postgres.

## Requirements

- Rust 1.70+
- PostgreSQL

## Setup

1. Create a database and set `DATABASE_URL`:

   ```bash
   export DATABASE_URL="postgres://user:pass@localhost/workflow_engine"
   ```

2. Run migrations (automatically on startup, or manually):

   ```bash
   sqlx migrate run
   ```

3. Build and run:

   ```bash
   cargo run
   ```

   Server listens on `0.0.0.0:3000` by default. Set `PORT` to override.

## API

| Method | Path | Description |
|--------|------|-------------|
| POST | /workflows | Create workflow (body: React Flow JSON or `{ "name", "tenant", "version", "definition" }`; **tenant** is required; version default `1`; new workflow is set as latest for that name) |
| GET | /workflows | List workflows (each includes `version`, `is_latest`) |
| GET | /workflows/:id | Get workflow by id (includes `version`, `is_latest`) |
| PUT | /workflows/:id | Update workflow (body: `{ "definition"?, "is_latest"? }`; set `is_latest: true` to mark as latest for that name) |
| POST | /webhook/:id | Trigger by UUID or name. Optional query `?version=1` when triggering by name. Without version, the workflow marked latest is used. Execution records `workflow_version`. |
| GET | /executions/:id | Get execution (includes `workflow_version` that was run) |

## Versioning and latest

- **Workflows** have a numeric `version` (1, 2, 3, …). Set it in the create body or in `definition.version`; default is `1`. Unique key is `(tenant, name, version)`.
- **is_latest**: For each `(tenant, name)`, one workflow can be marked as latest. New workflows are created with `is_latest: true` (and others with the same name are unmarked). Use **PUT /workflows/:id** with `"is_latest": true` to mark a different version as latest.
- **Executions** store the `workflow_version` (integer) that was run.
- When triggering by **name** without `?version=`, the workflow with `is_latest = true` is used; with `?version=1` the specified version is used.

## Node types (initial)

- **HttpTrigger** – Entry point; Webhook context is set by the HTTP layer.
- **HttpRequest** – Calls an external HTTP API (config: `method`, `url` or `path`, optional `body`/`headers`).
- **ServiceCall** – Calls an internal service (config: `serviceSlug`, `operation`). Uses the registered service registry (stub `authrs` by default).

## Expressions

Node inputs support `{{ JMESPath }}` expressions evaluated against the execution context (e.g. `{{ Webhook.body.customer_name }}`, `{{ nodes.some_node_id.body }}`).

## Docker

Build and run with Postgres:

```bash
docker compose up --build
```

- API: http://localhost:3000
- Postgres: localhost:5432 (user `workflow`, password `workflow`, db `workflow_engine`)

Build image only:

```bash
docker build -t workflow-engine .
```

## Postman

Import the collection from `postman/Workflow-Engine-API.postman_collection.json`.

**Collection variables:**

| Variable        | Default             | Description |
|----------------|---------------------|-------------|
| `base_url`     | http://localhost:3000 | API base URL |
| `workflow_id`  | (set by Create Workflow) | Used by Get Workflow, Trigger Webhook |
| `execution_id` | (set by Trigger Webhook) | Used by Get Execution |
| `tenant`       | (empty)              | Required for create. Set to a tenant value (body or X-Tenant-ID header). Optional for list/get to scope by tenant. |

Run **Create Workflow** then **Trigger Workflow** then **Get Execution** to exercise the full flow; variables are set automatically by test scripts.

## Multi-tenant (tenant)

**Tenant is mandatory** and has no default. Workflows store a **`tenant`** value (string).

- **POST /workflows** – **tenant** is required: send it in the request body or in the **X-Tenant-ID** header (non-empty). The value is stored in the workflow table.
- **PUT /workflows/:id** – Optional `tenant` in the body updates the workflow's stored tenant.

All workflow and execution endpoints accept an optional **X-Tenant-ID** header to scope requests. When set:

- **GET /workflows** – Only workflows for that tenant are returned.
- **GET /workflows/:id** – Returns 404 if the workflow’s tenant does not match.
- **POST /webhook/:id** – When triggering by name, lookup is scoped to that tenant; when by UUID, workflow must belong to that tenant.
- **GET /executions/:id** – Returns 404 if the execution’s workflow belongs to a different tenant.

Omit the header to see all tenants when listing; for create, tenant must be provided (body or header).

## Configuration

- `DATABASE_URL` – Postgres connection string (default: `postgres://localhost/workflow_engine`)
- `PORT` – Server port (default: 3000)
- `RUST_LOG` – Log level (default: `workflow_engine=info,tower_http=info`)
