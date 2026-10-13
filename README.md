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
| POST | /webhook/:id | Trigger by UUID or name. Optional query `?version=1` when triggering by name; optional `?step=true` for step-by-step (debug) mode; optional `?persist=full|errors_only|none` overrides the workflow's persistence for this call; optional `?async=true` [queues the run](#queued-runs-and-workers) and returns at once with `status: "queued"` and its `execution_id`. Without version, the workflow marked latest is used. Execution records `workflow_version`. |
| GET | /executions | List executions, newest first. Optional query: `workflow_id`, `last_minutes`, `limit` (default 100, max 500), `offset`, `trigger` (`webhook`, `schedule` or `workflow_call`), `schedule_id`. Returns `{ executions, total, limit, offset }`. |
| GET | /executions/:id | Get execution (includes `workflow_version` that was run, and `trigger`: `webhook`, `schedule` or `workflow_call`) |
| POST | /executions/:id/step | Run the next step for a paused execution (step-by-step mode). Returns the execution with updated status and steps. |
| POST | /executions/:id/cancel | Cancel a `queued`, `waiting` or `paused` execution so it never starts or resumes (status `cancelled`). 409 in any other state. |
| GET | /schedules | List the tenant's [schedules](#schedules); optional `?workflow=<name>`. |
| POST | /schedules | Create a schedule (body: a [schedule](#schedules-api)). 409 if the name exists. |
| GET | /schedules/:id | Get a schedule by id or name, with its next run, last outcome and the next 5 times it fires (`upcoming`). |
| PUT | /schedules/:id | Create or replace a schedule by name (or replace by id). 201 when created, 200 when replaced. |
| DELETE | /schedules/:id | Delete a schedule (by id or name). Its runs keep their history. |
| POST | /schedules/:id/run | Run a schedule once now, with its input. Optional `?async=true` queues it. |
| POST | /schedules/preview | Check a cron expression and timezone (body: `{ "cron", "timezone"?, "count"? }`); returns its description and the next times it fires. |
| POST | /signals | Deliver a signal to the tenant's runs [waiting](#waiting-durable-timers-and-signals) on its key (requires **X-Tenant-ID**; body `{ "key", "payload"? }`). Returns `{ "key", "resumed": [execution ids] }`; resumed runs continue in the background. |
| GET | /globals | List the tenant's globals (requires **X-Tenant-ID**). Returns `{ "globals": [{ "key", "value", "created_at", "updated_at" }] }`. |
| PUT | /globals | Upsert a global (requires **X-Tenant-ID**; body `{ "key", "value" }`; `value` is any JSON). |
| GET | /globals/:key | Get one global by key (requires **X-Tenant-ID**). |
| PUT | /globals/:key | Upsert a global by key (requires **X-Tenant-ID**; body is the raw value, or `{ "value": ... }`). |
| DELETE | /globals/:key | Delete a global (requires **X-Tenant-ID**). Returns `{ "key", "deleted": true }`. |
| GET | /node-templates | List the tenant's [node templates](#node-templates) (latest version of each; `?all_versions=true` for all). Requires **X-Tenant-ID**. |
| POST | /node-templates | Create a node template, or publish the next version if `slug` exists (body `{ "slug", "node_type", "config", "params"?, "name"?, "description"? }`). |
| GET | /node-templates/:slug | Get the latest version, or `?version=N`. |
| PUT | /node-templates/:slug | Publish the body as the next version (no-op when unchanged). `?force=true` skips the breaking-change check. |
| GET | /node-templates/:slug/versions | Every version, newest first. |
| GET | /node-templates/:slug/usages | Workflow nodes using the template, with `floating`/`pinned` counts. |
| DELETE | /node-templates/:slug | Delete the template (or `?version=N`). 409 while workflows still use it. |
| GET | /browser-automations | The [browser automations](#browser-automations) a `BrowserAutomation` node can run (hawk's published steps), each with its `params` schema. Filter with `?system=<manifest system>` / `?process=<hawk process>`. Returns `{ "automations": [{ "id", "name", "label", "system", "summary", "process", "version", "writes", "irreversible", "params", "manifest", "publishedAt" }] }`. 502 when hawk is unreachable. |
| GET | /browser-automations/:id | One automation from that list (404 if it has no published version). |
| GET | /browser-automations/runs/:run_id | A hawk run (`status`, `output`, `error`, `artifacts`, …), as hawk reports it. |
| GET | /browser-automations/runs/:run_id/artifacts/:name | A file the run produced (screenshot, download), with its content type. |

## Versioning and latest

- **Workflows** have a numeric `version` (1, 2, 3, …). Set it in the create body or in `definition.version`; default is `1`. Unique key is `(tenant, name, version)`.
- **is_latest**: For each `(tenant, name)`, one workflow can be marked as latest. New workflows are created with `is_latest: true` (and others with the same name are unmarked). Use **PUT /workflows/:id** with `"is_latest": true` to mark a different version as latest.
- **Executions** store the `workflow_version` (integer) that was run.
- When triggering by **name** without `?version=`, the workflow with `is_latest = true` is used; with `?version=1` the specified version is used.

## Node types (initial)

- **HttpTrigger** – Entry point; Webhook context is set by the HTTP layer.
- **HttpRequest** – Calls an external HTTP API (config: `method`, `url` or `path`, optional `body`/`headers`/`bodyMode`; see [HTTP request bodies](#http-request-bodies); optional `timeout`, see [Request timeouts](#request-timeouts)).
- **ServiceCall** – Calls an internal service (config: `serviceSlug`, `operation`; optional `rawBody`/`body`/`headers`/`bodyMode`, see [HTTP request bodies](#http-request-bodies); optional `timeout`, see [Request timeouts](#request-timeouts)). Uses the registered service registry (stub `authrs` by default).
- **WorkflowCall** – Runs another workflow as a nested execution and returns its response (config: `workflowId` or `workflow`/`workflowName` with optional `version`/`tenant`; payload via `rawBody`/`body`). Guarded against self-calls and cycles (max depth 10).
- **SetVariable** – Writes into the workflow's `local` scope during execution (config: `variables` object, or a single `key`/`value`; values support `{{ }}`). Updated `{{ local.* }}` values are visible to downstream nodes and later steps.
- **If** – Two-way conditional branch. Evaluates one condition and activates the `true` or `false` output port (config: `condition` as `{ left, operator, right }` or any truthy value, or top-level `left`/`operator`/`right`; optional `trueHandle`/`falseHandle` port labels).
- **Switch** – Multi-way conditional branch over a `value` (config: `cases` array of `{ handle, value }` / `{ handle, operator, value }` / `{ handle, condition }`; `mode` `"first"` (default) or `"all"`; `default` handle when nothing matches).
- **Loop** – Container that runs the nodes inside it once per item (config: `items`, an expression returning a list). See [Loops](#loops).
- **Wait** – Suspends the run for a time, until a time, or until a signal arrives (with an optional timeout). See [Waiting](#waiting-durable-timers-and-signals).
- **Template** – A reference to a shared [node template](#node-templates) (config: `template`, optional `version`, `params`).
- **BrowserAutomation** – Runs a hawk browser automation (config: `automation`, `params`, optional `session`, `mode`, …). See [Browser automations](#browser-automations).

### Branching

`If` and `Switch` route the flow by activating only the output port(s) they select. Connect
their outgoing edges from the matching **source handle** (`true`/`false`, or a Switch case
label). At runtime, a node is executed only if it is reachable through an active edge;
nodes that sit only on a branch that was not taken are recorded as **`skipped`** (a
`workflow_steps` row with `status = "skipped"`, no output), and skipping cascades to
everything downstream of them. A join reached from either branch (e.g. via a `Merge`) still
runs. Supported operators: `eq`/`ne`, `gt`/`gte`/`lt`/`lte`, `contains`/`notContains`,
`in`/`notIn`, `startsWith`/`endsWith`, `exists`/`empty` (comparisons coerce numeric strings;
`{{ }}` expressions in the condition are interpolated before evaluation). Cycles are not
supported — workflows are DAGs; repeat work with a [Loop](#loops).

### Loops

A `Loop` node is a container: nodes whose React Flow `parentId` is the Loop form its
**body**, which runs once per element of `items`, sequentially. The body is its own small DAG:
its entry nodes are the ones with no incoming edge from another body node, and If/Switch
branching works inside it as usual.

- Inside the body, `{{ item }}` is the current element and `{{ index }}` its 0-based position
  (nested loops: the innermost wins). Entry nodes also get `current` = the item.
  `nodes.<id>` holds only the current iteration's outputs.
- `local` changes (SetVariable) carry over to later iterations and past the loop.
- `items` resolving to `null` (e.g. a missing field) runs nothing; any other non-list is an error.
- The first failure stops the loop and fails the Loop node, e.g.
  `item 1: upload: formdata row 2 ("attachmentPath"): invalid base64 …`.
- Output: `{ "count", "results": [ { "<body node id>": <output>, … } per item ] }`, so after
  the loop use e.g. `{{ nodes.loop.results[*].upload.body.id }}`.

Edges must stay inside one scope: body nodes connect only to each other, and the Loop's
outgoing edges lead to what runs after all items. The one exception is an edge from the Loop
to a node directly inside it (the builder's *start* connector), which is ignored. A Loop with
no nodes inside, or an edge crossing the boundary, fails the execution before anything runs.

Each body node run is its own step, tagged with `iteration` (`"0"`, `"1"`, … or `"2.1"` for
nested loops) in `GET /executions/:id`. In step-by-step mode a Loop runs all its items in one
step. The execution context is persisted once per Loop, not per item.

```json
{
  "nodes": [
    { "id": "loop", "type": "loop", "data": { "items": "{{ not_null(Webhook.body.attachments, `[]`) }}" } },
    { "id": "upload", "type": "serviceCall", "parentId": "loop", "data": {
        "method": "POST", "serviceSlug": "core", "path": "/api/v1/package/manufacturing_essential/ticket_attachments",
        "bodyMode": "formdata",
        "rawBody": [
          { "key": "ticketId", "value": "{{ local.parentTicketId }}" },
          { "key": "attachmentName", "value": "{{ item.attachmentName }}" },
          { "key": "attachmentPath", "type": "file",
            "value": "{{ {filename: item.attachmentName, contentType: item.fileType, base64: item.contentBase64} }}" }
        ] } }
  ],
  "edges": []
}
```

### HTTP request bodies

`HttpRequest` and `ServiceCall` encode the request body according to the optional `bodyMode`.
The body field is `body`/`payload` for `HttpRequest` and `rawBody` (or `body`) for `ServiceCall`:

| `bodyMode` | `body` | Sent as |
|---|---|---|
| *(omitted)* | object / array / string | JSON for objects and arrays; strings as-is (unchanged behaviour). `HttpRequest` defaults string bodies to `Content-Type: application/json`. |
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

### Request timeouts

`HttpRequest` and `ServiceCall` give up on a call after `timeout`, which covers everything from
connecting until the response body is read. It takes the same durations as `Wait`: a number of
seconds or e.g. `"90s"`, `"5m"`, `"1h"`, and accepts `{{ }}` expressions. The default is `30s`
and the maximum `1h`, since the run keeps its worker busy for as long as the call takes. A call
that runs out of time fails the step with
``HttpRequest timed out after 5m (set `timeout` to allow longer)``; an invalid `timeout` fails it before anything is sent. `HttpRequest` reports the
timeout it used in its step output as `request.timeout`.

```json
{ "type": "httpRequest", "data": { "method": "POST", "url": "{{ global.REPORTS_URL }}/rebuild", "timeout": "5m" } }
```

## Waiting (durable timers and signals)

A **Wait** node pauses a run without holding anything open: the run is saved with status
`waiting` and resumed later, so waits of minutes or days survive restarts and deploys.

| Mode | Config | Resumes | Output ports |
|------|--------|---------|--------------|
| `duration` | `duration`: `"90s"`, `"15m"`, `"1h30m"`, `"2d"`, or seconds | after that long | one (plain node) |
| `until` | `until`: RFC 3339 timestamp or epoch milliseconds | at that time | one (plain node) |
| `event` | `correlationKey`, optional `filter`, optional `timeout` | on a matching signal, or at the timeout | `received` / `timed_out` |

`mode` can be omitted: it is inferred from the keys given. A time already in the past does
not wait. All values accept `{{ }}` expressions, e.g. `"timeout": "{{ item.sla }}"`.

```json
{ "id": "wait-ack", "type": "wait", "data": {
    "correlationKey": "ticket-{{ Webhook.body.id }}",
    "filter": "signal.payload.status == 'ack'",
    "timeout": "30m"
} }
```

- **Signals**: `POST /signals` with `{ "key": "ticket-42", "payload": { "status": "ack" } }` resumes
  every run of the tenant waiting on that key whose `filter` accepts it. The filter is a JMESPath
  expression over `{ signal: { key, payload } }`, written without `{{ }}` (any `{{ }}` parts in it
  are filled in when the wait starts). The payload is in the node's output as `signal`.
- **Timeout**: without one, an event wait lasts until a signal arrives or the run is cancelled.
  Route `received` and `timed_out` like an `If`'s ports; the output has `resumedBy`
  (`timer`/`signal`), `startedAt`, `resumedAt`, and `timedOut: true` or `signal`.
- **Once only**: whichever of the timer and the signal comes first wins; the other does nothing.
- **Inside a Loop**: a Wait can sit in a Loop body. On resume the run replays from the top,
  reusing the recorded outputs of everything that already ran, and continues from the waiting
  iteration. This is how an escalation matrix is modelled: a Loop over the levels, each one
  notifying and then waiting for an acknowledgement with that level's SLA as the timeout.
- **Persistence**: a run that waits is always saved when it suspends, whatever its
  `persistence` mode, and recorded in full from then on. `GET /executions/:id` shows what a
  waiting run waits for under `wait` (`node_id`, `iteration`, `wake_at`, `correlation_key`).
- **Step mode**: a Wait suspends the step; once resumed, that step completes and the run is
  `paused` again.
- **Cancel**: `POST /executions/:id/cancel`.
- **WorkflowCall**: a called workflow cannot wait; if it suspends, it is cancelled and the
  `WorkflowCall` node fails.

Due timers are polled by processes with the `worker` [role](#queued-runs-and-workers) every `WAIT_POLL_INTERVAL_MS` (so a wait may run up to that much late),
and at most `WAIT_RESUME_CONCURRENCY` resumed runs execute at a time. Several replicas can poll
the same database safely. Custom node types can suspend too: implement `NodeExecutor::run` to
return `NodeOutcome::Suspend(SuspendSpec { wake_at, correlation_key, filter, state })`, and
`resume` to continue from `state`.

## Queued runs and workers

`POST /webhook/:id?async=true` does not run the workflow in the request. It queues the run and
returns at once:

```json
{ "execution_id": "…", "status": "queued", "result": null, "error": null }
```

A worker takes the run from the queue (`workflow_run_queue`) and runs it under that
`execution_id`. Until then, `GET /executions/:id` shows it with status `queued` and no steps, and
`POST /executions/:id/cancel` removes it from the queue (it is then recorded as `cancelled`).
After it starts, it is recorded like any other run, according to its persistence mode: under the
default `errors_only`, a successful run leaves no execution behind, so pass `?persist=full` to
follow a queued run to its result.

Each run is taken by one worker and started at most once: it leaves the queue when a worker
claims it, so a worker that dies before the run is recorded loses that run rather than starting it
twice. Step mode cannot be queued.

**Roles.** `ENGINE_ROLES` (comma-separated) sets what a process does:

| Role | Does |
|---|---|
| `api` | Serves the HTTP API. Synchronous webhook calls run in this process; signals resume waiting runs here. |
| `worker` | Runs queued runs and resumes runs whose wait timer is due. Serves no HTTP. |
| `scheduler` | Fires due [schedules](#schedules), queueing their runs for workers. Serves no HTTP. |

The default, `api,worker,scheduler`, is a single process doing everything. At scale, run
separate deployments against the same database (`ENGINE_ROLES=api`, `ENGINE_ROLES=worker`,
`ENGINE_ROLES=scheduler`) and scale each on its own: API and workers by load, the scheduler
usually one or two replicas (more are safe, but not faster). An environment that should not
run its schedules (e.g. a local engine pointed at a shared database) leaves out `scheduler`. Any number of workers can share the queue: they claim runs
with `FOR UPDATE SKIP LOCKED`, so each run is taken once. A worker keeps up to
`RUN_WORKER_CONCURRENCY` runs going and takes the next run as soon as one finishes; it only
waits `RUN_QUEUE_IDLE_POLL_MS` when the queue is empty. Size `DATABASE_MAX_CONNECTIONS` to the
concurrency of the roles a process runs.

## Schedules

A schedule starts a workflow on a cron expression, in a timezone. Schedules belong to the
engine (one per environment), not to the workflow definition, so the same workflow can run on
different schedules, or none, in each environment, and still be called by webhook.

- **Cron and timezone**: standard five-field cron (`minute hour day-of-month month
  day-of-week`, plus `@daily`-style nicknames), read as wall-clock time in the schedule's IANA
  `timezone` (default `UTC`). `0 9 * * 1-5` in `America/New_York` is 09:00 New York time all
  year. A fixed time skipped when clocks go forward (02:30) runs at the first time after the
  gap (03:00); one repeated when clocks go back runs once. Schedules fire at most once a minute.
- **Input**: each run gets the schedule's `input` as `Webhook.body` (headers empty), so a
  workflow written for webhook calls runs unchanged. `Trigger` is
  `{ "type": "schedule", "schedule_id", "schedule_name", "scheduled_for" }`, where
  `scheduled_for` is the slot in the schedule's timezone, e.g. `2026-10-13T09:00:00+05:30`.
  Executions record `trigger = "schedule"` and `schedule_id`.
- **Version**: the workflow is found by name when the schedule fires: the latest version, or a
  fixed `version`.
- **Firing**: the scheduler queues a run for a [worker](#queued-runs-and-workers) and moves the
  schedule to its next slot in one transaction, so each slot fires at most once, however many
  schedulers run.
- **Missed slots** (due more than `SCHEDULE_MISFIRE_GRACE_SECS` ago, e.g. no scheduler was
  running): `catch_up: "once"` (default) runs once, for the latest missed slot; `"skip"` runs
  none and waits for the next slot.
- **Overlap**: with `overlap: "skip"` (default), a slot is skipped while the schedule's previous
  run is queued, waiting or paused, or started less than `SCHEDULE_RUN_LEASE_SECS` ago;
  `"allow"` starts it anyway. Webhook calls of the same workflow never block a schedule, nor
  the other way round.
- **Last outcome**: each schedule records when it last came due, the slot, what happened
  (`queued`, `skipped_overlap`, `skipped_missed` or `failed_to_start`, e.g. the workflow does
  not exist) and the run it queued. A schedule whose cron or timezone can no longer be read
  stops until it is saved again.

### Schedules API

All `/schedules` endpoints need `X-Tenant-ID` (except `preview`). A schedule is:

```json
{
  "name": "daily-report-in",
  "workflow": "daily-report",
  "version": null,
  "cron": "0 9 * * *",
  "timezone": "Asia/Kolkata",
  "input": { "region": "IN" },
  "enabled": true,
  "overlap": "skip",
  "catch_up": "once",
  "persist": null
}
```

Only `name`, `workflow` and `cron` are required; the rest default as shown. `name` is unique
per tenant (letters, digits, `-`, `_`, `.`) and cannot be changed. `persist` overrides the
workflow's [persistence](#saving-executions-persistence) for scheduled runs. The camelCase
names used in config files (`workflowName`, `workflowVersion`, `catchUp`, `persistence`) are
accepted too. Saving checks the cron, the timezone and that the workflow exists.

`PUT /schedules/:name` creates or replaces, which is what config sync uses: it keeps the
schedule's next run unless the schedule is new, re-enabled, or its cron or timezone changed,
so re-saving an unchanged schedule never moves or drops a due slot. Setting `enabled: false`
stops it without deleting it.

Responses add the schedule's state: `id`, `description` (the cron in words), `next_run_at`
(UTC) and `next_run_local` (in its timezone), `last_run_at`, `last_scheduled_for`,
`last_outcome`, `last_error`, `last_execution_id` and `active_execution_id`.

To run the same workflow in several timezones (e.g. 09:00 local in India, the UK and the US),
create one schedule per timezone, each with its own `input`.

## Browser automations

A **BrowserAutomation** node runs any automation published in hawk (`hawkd`, the
browser-automation service in the `automation/hawk` repo) in a headless browser, and returns its
result. The node is generic: it knows nothing about particular sites or automations. Whatever
hawk can run (form filling, scraping, report downloads, uploads, on any website) the node can
call, passing the automation id and the inputs it declares. The engine only uses hawkd's existing
HTTP API (`POST /v1/runs`, `GET /v1/runs/:id`, …); hawk itself is unchanged, and its other
clients keep using it as before.

**Where hawk is:** the `hawk` entry of the service registry if there is one, otherwise
`HAWK_URL`, otherwise `http://127.0.0.1:7878`. hawkd has no auth of its own, so keep it on a
private network.

**Choosing an automation:** `GET /browser-automations` lists what can run, with each one's
`params` schema (`{ name: { type, required, default, description, enum } }`) for building the
node's form. Only published versions are listed. A new automation is a `hawk/v1` config created
with hawk's `POST /v1/automations` and published with `POST /v1/automations/:id/versions/:v/publish`
(or pushed with `npm run hawk:push -- --publish`); it shows up here once published. Automations not
tied to one of hawk's existing processes go under its `shared` process.

| Config | Default | |
|---|---|---|
| `automation` | (required) | Automation id, as listed by `GET /browser-automations`. |
| `params` | `{}` | The automation's params. hawk checks them (and its guards) before opening a browser. |
| `version` | published | Run a specific version, e.g. a draft being tried out. |
| `inputs` | – | Upstream outputs, available to the automation as `inputs.*`. |
| `env` / `secrets` | – | Objects available as `env.*` / `secrets.*`. hawk masks secrets and never stores them; reference them with `{{ global.x }}` rather than writing them into the definition. |
| `environment` | – | A stored hawk environment to take `env` from (`env` overrides it). |
| `session` | fresh browser | A live session handle, or `{ "slug", "key"?, "params"?, "secrets"?, "env"?, "environment"? }` to sign in through hawk's stored session first. Without `key` that session is closed when the run ends; with one it is kept for reuse. Use this for sites that need a login. |
| `mode` | `live` | `dry` stops before the first write (and counts as success); `mock` opens no browser. |
| `actor` | `workflow-engine:<workflow id>` | Recorded on the hawk run. |
| `timeout` | `30m` | Give up and cancel the run after this long. |
| `inlineFor` | `60s` | Wait this long in the request; a run still going then suspends the workflow. `0` suspends at once. |
| `pollInterval` / `durablePollInterval` | `2s` / `15s` | How often the run is checked in the request / while suspended. |
| `failOnError` | `true` | Fail the node unless the run passed. With `false` it completes and `ok` tells the outcome (route it with an `If`). |
| `downloadArtifacts` | `false` | Add each artifact's bytes as `{ filename, contentType, base64 }` (up to 20 MB each), the shape a `formdata` file row takes. |

Durations use the [Wait](#waiting-durable-timers-and-signals) format. All values accept `{{ }}`.

```json
{ "id": "eta", "type": "browserAutomation", "data": {
    "automation": "download-report",
    "params": {
      "from": "{{ Webhook.body.from }}",
      "to": "{{ Webhook.body.to }}",
      "fileName": "report-{{ Webhook.body.from }}.pdf"
    },
    "session": { "slug": "portal", "key": "{{ global.portalUser }}", "secrets": { "password": "{{ global.portalPassword }}" } },
    "downloadArtifacts": true
} }
```

Output, available as `{{ nodes.<id>.* }}`:

```json
{ "runId": "…", "automation": "download-report", "version": 4, "mode": "live",
  "status": "passed", "ok": true, "output": { … }, "error": null,
  "artifacts": [{ "name": "report-2026-10-01.pdf", "kind": "download", "bytes": 48213, "sha256": "…", "url": "http://hawk/v1/runs/…/artifacts/report-2026-10-01.pdf" }],
  "startedAt": "…", "endedAt": "…", "session": "…" }
```

`status` is hawk's: `passed`, `failed`, `rejected` (params or guards refused it),
`stopped-before-write` or `cancelled`. A failure reads e.g.
`browser automation "download-report" failed: locator #from not found at steps[2] (hawk run …)`.

- **Long runs** suspend like a [Wait](#waiting-durable-timers-and-signals): the webhook answers
  `waiting`, the run is checked by the timer worker until it ends, and the workflow continues.
  They survive restarts on both sides (hawk marks a run its restart interrupted as failed).
- **WorkflowCall**: a called workflow cannot suspend, so give a BrowserAutomation inside one an
  `inlineFor` as long as its `timeout`, and keep that under the caller's request timeout (300 s).
- **Hawk down**: up to 5 consecutive failed checks are retried before the node fails and the run
  is cancelled.

## Node templates

A node template is a reusable node kept once per tenant and referenced by any number of
workflows, so it has a single source of truth: publish a change and every workflow using it
runs the new version.

```json
PUT /node-templates/create-ticket
{
  "name": "Create ticket",
  "node_type": "serviceCall",
  "config": {
    "method": "POST", "serviceSlug": "core", "path": "/api/v1/tickets",
    "rawBody": { "id": "{{ params.ticketId }}", "priority": "{{ params.priority }}", "source": "{{ global.SOURCE }}" }
  },
  "params": { "ticketId": { "required": true, "label": "Ticket id" }, "priority": { "default": "low" } }
}
```

- `node_type` is one of `HttpRequest`, `ServiceCall`, `WorkflowCall`, `SetVariable`, `If`,
  `Switch`. `config` is what the node's `data` would be.
- `params` declares what a workflow can set: `{ "<name>": { "required"?, "default"?, "label"?, "description"? } }`.
  The config reads them as `{{ params.<name> }}`; using an undeclared one is rejected.
- Versions are immutable. Each publish with changed content adds the next version and marks
  it latest; publishing identical content does nothing (`created: false`).

Use one in a workflow with a `template` node:

```json
{ "id": "mkTicket", "type": "template",
  "data": { "template": "create-ticket", "version": 2, "params": { "ticketId": "{{ Webhook.body.id }}" } } }
```

- Without `version` the node follows the latest version; with it, it is pinned.
- Param values may be `{{ }}` expressions; they are evaluated against the workflow's context
  (`Webhook`, `nodes`, `local`, `item`, …) before the template's config reads them. Missing
  params take their `default`.
- Outputs, edges and branching work as for the underlying node: `{{ nodes.mkTicket.body }}`,
  `If`/`Switch` handles, and `parentId` inside a Loop are unchanged.

References are expanded when an execution starts, and the versions used are recorded in its
context under `templates` (`{ "mkTicket": { "template": "create-ticket", "version": 2 } }`).
A step-mode run keeps those versions even if a newer one is published while it is paused.

Safety checks:

- Saving a workflow (`POST`/`PUT /workflows`) fails with 400 if a template or pinned version
  does not exist, a required param is missing, or an undeclared param is set.
- Publishing a version that adds a required param fails with 409 while nodes following the
  latest version do not set it (pin them, set it, or use `?force=true`).
- Deleting a template, or a version still in use, fails with 409.
  `GET /node-templates/:slug/usages` lists who uses it.

Runs of workflows without template nodes are unaffected; runs with them do one extra
query to load all their templates.

## Saving executions (persistence)

Each workflow chooses how much of a run is saved to `workflow_executions` / `workflow_steps`, with `persistence` in its definition (`data.persistence`, or top-level `persistence`):

| Mode | Successful run | Failed run |
|------|----------------|------------|
| `errors_only` (default) | Nothing saved | Execution (status `failed`, context at the failure) and every step up to the failure: completed outputs, skipped branches and the failed step's error |
| `full` | Execution and all steps, written as the run goes | Same, as the run goes |
| `none` | Nothing saved | Nothing saved |

```json
{ "data": { "persistence": "full", "nodes": [...], "edges": [...] } }
```

A webhook call can override it with `?persist=full` (e.g. to inspect a run in the executions view). The webhook response always includes `execution_id` and `result`, but when nothing was saved that id is not in `GET /executions`. Step mode (`?step=true`) is always saved in full. A `WorkflowCall` child is saved according to the child workflow's own mode.

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
- `Trigger` – how the run was started: `{ "type": "webhook" }`, or `{ "type": "workflow_call", "parent_execution_id": ... }` for a `WorkflowCall` child. Branch on it with e.g. `{{ Trigger.type == 'webhook' }}`. The same value is stored on the execution as `trigger`.
- `nodes.<node_id>` – each completed node's output.
- `current` – the previous node's output.
- `global` – the tenant's stored globals (see the `/globals` API), snapshotted at execution start. Reference as `{{ global.API_BASE }}`.
- `local` – workflow-scoped variables. Reference as `{{ local.counter }}`.
- `item` / `index` – inside a [Loop](#loops) body: the current element and its position.
- `params` – inside a [node template](#node-templates)'s config: the params the workflow node set.

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
- `ENGINE_ROLES` – What this process does: `api`, `worker`, or both (default: `api,worker`). See [queued runs and workers](#queued-runs-and-workers)
- `DATABASE_MAX_CONNECTIONS` – Postgres connection pool size (default: 5)
- `RUN_WORKER_CONCURRENCY` – Maximum queued runs a worker process executes at once (default: 4)
- `RUN_QUEUE_IDLE_POLL_MS` – How often an idle worker checks an empty queue (default: 500)
- `SCHEDULER_POLL_INTERVAL_MS` – How often the scheduler looks for due [schedules](#schedules) (default: 1000)
- `SCHEDULER_BATCH` – Schedules handled per scheduler transaction (default: 100)
- `SCHEDULE_MISFIRE_GRACE_SECS` – How late a slot can fire before it counts as missed (default: 60)
- `SCHEDULE_RUN_LEASE_SECS` – How long a scheduled run counts as still going for the overlap rule, unless it is waiting or paused (default: 3600)
- `RUST_LOG` – Log level (default: `workflow_engine=info,tower_http=info`)
- `WAIT_POLL_INTERVAL_MS` – How often due [waits](#waiting-durable-timers-and-signals) are polled (default: 1000)
- `WAIT_RESUME_CONCURRENCY` – Maximum resumed runs executing at once (default: 4)
- `HAWK_URL` – hawkd's base URL for [browser automations](#browser-automations) (default: `http://127.0.0.1:7878`; a `hawk` service registry entry overrides it)
