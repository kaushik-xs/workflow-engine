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
| POST | /workflows | Create workflow (body: React Flow JSON or `{ "name", "definition" }`) |
| GET | /workflows | List workflows |
| GET | /workflows/:id | Get workflow by id |
| POST | /webhook/:id | Trigger workflow by workflow UUID or by name (body/headers become Webhook context) |
| GET | /executions/:id | Get execution by id (status, context, steps) |

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
| `tenant_id`    | (empty)              | Optional. Set to a tenant UUID to scope requests via X-Tenant-ID header. |

Run **Create Workflow** then **Trigger Workflow** then **Get Execution** to exercise the full flow; variables are set automatically by test scripts.

## Multi-tenant (X-Tenant-ID)

All workflow and execution endpoints accept an optional **`X-Tenant-ID`** header (UUID). When set:

- **POST /workflows** – New workflow is created under that tenant (otherwise default tenant).
- **GET /workflows** – Only workflows for that tenant are returned.
- **GET /workflows/:id** – Returns 404 if the workflow’s tenant does not match.
- **POST /webhook/:id** – When triggering by name, lookup is scoped to that tenant; when by UUID, workflow must belong to that tenant.
- **GET /executions/:id** – Returns 404 if the execution’s workflow belongs to a different tenant.

Omit the header to use the default tenant (e.g. single-tenant mode).

## Configuration

- `DATABASE_URL` – Postgres connection string (default: `postgres://localhost/workflow_engine`)
- `PORT` – Server port (default: 3000)
- `RUST_LOG` – Log level (default: `workflow_engine=info,tower_http=info`)
