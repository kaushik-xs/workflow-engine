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
| DELETE | /workflows/:id | Delete workflow by id. Also deletes all of the workflow's executions and their steps (cascade). Returns `{ "id", "deleted": true }`. |
| POST | /webhook/:id | Trigger by UUID or name. Optional query `?version=1` when triggering by name; optional `?step=true` for step-by-step (debug) mode. Without version, the workflow marked latest is used. Execution records `workflow_version`. |
| GET | /executions/:id | Get execution (includes `workflow_version` that was run) |
| POST | /executions/:id/step | Run the next step for a paused execution (step-by-step mode). Returns the execution with updated status and steps. |
| GET | /globals | List the tenant's globals (requires **X-Tenant-ID**). Returns `{ "globals": [{ "key", "value", "created_at", "updated_at" }] }`. |
| PUT | /globals | Upsert a global (requires **X-Tenant-ID**; body `{ "key", "value" }`; `value` is any JSON). |
| GET | /globals/:key | Get one global by key (requires **X-Tenant-ID**). |
| PUT | /globals/:key | Upsert a global by key (requires **X-Tenant-ID**; body is the raw value, or `{ "value": ... }`). |
| DELETE | /globals/:key | Delete a global (requires **X-Tenant-ID**). Returns `{ "key", "deleted": true }`. |

## Versioning and latest

- **Workflows** have a numeric `version` (1, 2, 3, …). Set it in the create body or in `definition.version`; default is `1`. Unique key is `(tenant, name, version)`.
- **is_latest**: For each `(tenant, name)`, one workflow can be marked as latest. New workflows are created with `is_latest: true` (and others with the same name are unmarked). Use **PUT /workflows/:id** with `"is_latest": true` to mark a different version as latest.
- **Executions** store the `workflow_version` (integer) that was run.
- When triggering by **name** without `?version=`, the workflow with `is_latest = true` is used; with `?version=1` the specified version is used.

## Node types (initial)

- **HttpTrigger** – Entry point; Webhook context is set by the HTTP layer.
- **HttpRequest** – Calls an external HTTP API (config: `method`, `url` or `path`, optional `body`/`headers`/`bodyMode`; see [HTTP request bodies](#http-request-bodies)).
- **ServiceCall** – Calls an internal service (config: `serviceSlug`, `operation`). Uses the registered service registry (stub `authrs` by default).
- **WorkflowCall** – Runs another workflow as a nested execution and returns its response (config: `workflowId` or `workflow`/`workflowName` with optional `version`/`tenant`; payload via `rawBody`/`body`). Guarded against self-calls and cycles (max depth 10).
- **SetVariable** – Writes into the workflow's `local` scope during execution (config: `variables` object, or a single `key`/`value`; values support `{{ }}`). Updated `{{ local.* }}` values are visible to downstream nodes and later steps.
- **If** – Two-way conditional branch. Evaluates one condition and activates the `true` or `false` output port (config: `condition` as `{ left, operator, right }` or any truthy value, or top-level `left`/`operator`/`right`; optional `trueHandle`/`falseHandle` port labels).
- **Switch** – Multi-way conditional branch over a `value` (config: `cases` array of `{ handle, value }` / `{ handle, operator, value }` / `{ handle, condition }`; `mode` `"first"` (default) or `"all"`; `default` handle when nothing matches).

### Branching

`If` and `Switch` route the flow by activating only the output port(s) they select. Connect
their outgoing edges from the matching **source handle** (`true`/`false`, or a Switch case
label). At runtime, a node is executed only if it is reachable through an active edge;
nodes that sit only on a branch that was not taken are recorded as **`skipped`** (a
`workflow_steps` row with `status = "skipped"`, no output), and skipping cascades to
everything downstream of them. A join reached from either branch (e.g. via a `Merge`) still
runs. Supported operators: `eq`/`ne`, `gt`/`gte`/`lt`/`lte`, `contains`/`notContains`,
`in`/`notIn`, `startsWith`/`endsWith`, `exists`/`empty` (comparisons coerce numeric strings;
`{{ }}` expressions in the condition are interpolated before evaluation). Loops/cycles are
not supported — workflows are DAGs.

### HTTP request bodies

`HttpRequest` encodes `body` according to the optional `bodyMode`:

| `bodyMode` | `body` | Sent as |
|---|---|---|
| *(omitted)* | object / array / string | JSON for objects and arrays; strings as-is. Default `Content-Type: application/json` (unchanged behaviour). |
| `none` | ignored | No body. |
| `raw` | string | The string as-is. Default `Content-Type: text/plain`. |
| `formdata` | array of rows | `multipart/form-data` (aliases: `form-data`, `multipart`). |

A `Content-Type` in `headers` always wins over the defaults above, and is sent once. The
exception is `formdata`: the engine generates the boundary, so it sets
`multipart/form-data; boundary=…` itself and ignores any `Content-Type` you provide.

`formdata` rows follow Postman's shape — `{ "key", "type": "text" | "file", "value", "disabled"? }`
(`type` defaults to `text`), sent in row order:

- **text** – a string, number or boolean sends one part. An array sends **one part per
  element** under the same key, so `{{ list[*].field }}` loops without templating.
  `null` sends nothing.
- **file** – an object `{ "base64", "filename"?, "contentType"? }` or an array of them (one
  part each). The base64 is decoded, so the server receives the real bytes. Whitespace,
  missing padding and a `data:<type>;base64,` prefix are tolerated. `contentType` defaults
  to `application/octet-stream`.

```json
{
  "type": "httpRequest",
  "config": {
    "method": "POST",
    "url": "https://api.example.com/tickets/attachments",
    "bodyMode": "formdata",
    "body": [
      { "key": "ticketId", "type": "text", "value": "{{ local.parentTicketId }}" },
      { "key": "attachmentName", "type": "text", "value": "{{ not_null(Webhook.body.attachments, Webhook.body.ticketAttachments, `[]`)[*].attachmentName }}" },
      { "key": "attachmentPath", "type": "file", "value": "{{ not_null(Webhook.body.attachments, Webhook.body.ticketAttachments, `[]`)[*].{filename: attachmentName, contentType: fileType, base64: contentBase64} }}" }
    ]
  }
}
```

In the step output, `request.body` lists the parts sent. File parts show their `filename`,
`contentType` and `size` in place of the content.

## Step-by-step execution (debug mode)

You can run a workflow one node at a time for debugging. Execution state is stored in the database so you can continue later.

1. **Start in step mode**: Trigger the webhook with `?step=true` (e.g. `POST /webhook/:id?step=true`). The server creates an execution with status `paused` and does not run any nodes. The response returns `execution_id` and `status: "paused"`.

2. **Advance one step**: Call `POST /executions/:id/step` with the execution id. The server runs the next runnable node (by workflow order), updates the execution context and step row, then sets status to `paused` again if more nodes remain, or `completed` / `failed` otherwise. The response is the same shape as GET /executions/:id (execution with steps).

3. **Resume**: Keep calling `POST /executions/:id/step` until the workflow is completed or failed. You can stop and resume later; state is persisted in `workflow_executions.context` and `workflow_steps`.

If you call **Run next step** when the execution is not `paused` (e.g. already completed or failed), the API returns 400 with message "Execution is not in paused state."

## Expressions

Node inputs support `{{ JMESPath }}` expressions evaluated against the execution context (e.g. `{{ Webhook.body.customer_name }}`, `{{ nodes.some_node_id.body }}`).

The execution context exposes these roots:

- `Webhook` – the trigger request (`body`, `headers`).
- `nodes.<node_id>` – each completed node's output.
- `current` – the previous node's output.
- `global` – the tenant's stored globals (see the `/globals` API), snapshotted at execution start. Reference as `{{ global.API_BASE }}`.
- `local` – workflow-scoped variables. Reference as `{{ local.counter }}`.

> The OS environment is **not** exposed to workflows. Anything that was previously read from `env` should be stored as a `global` (managed per tenant) or a `local` variable.

### Global scope

Globals are a per-tenant key/value store managed through the `/globals` API (values are any JSON). Every execution for that tenant sees the same values under `global.*`, captured as a snapshot when the execution is created.

### Local scope

Local variables are scoped to a single execution:

- **Declare defaults in the workflow definition** under `data.variables` (or top-level `variables`). Each value may use `{{ }}` and is interpolated against `Webhook` and `global` at execution start to seed `local.*`:

  ```json
  { "data": { "variables": { "attempts": 0, "customer": "{{ Webhook.body.customer_name }}" }, "nodes": [], "edges": [] } }
  ```

- **Mutate at runtime** with a `SetVariable` node. Its `variables` (or `key`/`value`) are merged into `local`, so later nodes and steps read the updated values.

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
- **DELETE /workflows/:id** – Returns 404 if the workflow’s tenant does not match (nothing is deleted in that case).
- **POST /webhook/:id** – When triggering by name, lookup is scoped to that tenant; when by UUID, workflow must belong to that tenant.
- **GET /executions/:id** – Returns 404 if the execution’s workflow belongs to a different tenant.

Omit the header to see all tenants when listing; for create, tenant must be provided (body or header).

## Configuration

- `DATABASE_URL` – Postgres connection string (default: `postgres://localhost/workflow_engine`)
- `PORT` – Server port (default: 3000)
- `RUST_LOG` – Log level (default: `workflow_engine=info,tower_http=info`)
