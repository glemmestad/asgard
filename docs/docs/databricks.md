---
sidebar_position: 6.3
title: Databricks (in front of)
---

# Databricks: Frontkeep sits in front of it

Frontkeep **orchestrates** Databricks — it does not replace it, and Databricks does
not replace Frontkeep. Databricks runs the lakehouse, Spark, Unity Catalog, and
serves models. Frontkeep is the org-wide, cross-vendor, **agent-first control plane**
that sits in front: it governs *who* gets *what*, attributes the cost, and fronts
the model calls — for Databricks exactly as it does for AWS, Auth0, and OpenAI.

This integration is built entirely from Frontkeep's existing primitives — Databricks
is "just another tenant of the building," wired three ways:

## 1. Inference — in front of Databricks Model Serving

Model calls flow **agent → Frontkeep's gateway → Databricks Model Serving / Foundation
Model APIs**. The project gets an Frontkeep virtual key, never the Databricks token.
Every call is budget-checked, Cedar-policy- and data-class-gated, guardrailed,
kill-switchable, audited, and **cost-attributed per project**.

This sits in front of even Databricks' own **Mosaic AI Gateway** (which is
per-workspace model governance): Frontkeep is the layer above it that spans projects
and vendors and exposes an MCP control plane to agents.

The `databricks` inference module is a **plug-in manifest** (`services/databricks/
service.yaml`), the same shape as LiteLLM — no Databricks-specific code in Frontkeep's
core. It's `kind: openai-compatible` with a `chat_path` override, because Databricks
queries a served model at `{host}/serving-endpoints/{endpoint-name}/invocations`.

Enable (the standard Databricks env vars, shared with Terraform + cost):
```
DATABRICKS_HOST=https://dbc-xxxx.cloud.databricks.com
DATABRICKS_TOKEN=dapi...
```
Then edit the module's `models[]` so each `route` is one of your serving endpoint
names (`databricks serving-endpoints list`). Use it out-of-band, like any service:
```sh
curl -sS https://<asgard-host>/api/gateway/chat \
  -H "Authorization: Bearer <project-llm-key>" \
  -H 'content-type: application/json' \
  -d '{"model":"model:databricks/llama-3-3-70b","messages":[{"role":"user","content":"hi"}],"data_class":"internal"}'
```

## 2. Provisioning — Databricks resources through the gate

Databricks resources are provisioned **through Frontkeep** via the universal
`terraform` connector + the Databricks Terraform provider (`modules/databricks/*`).
Same registration gate, classification floor, approval tier, project tagging, and
audit as AWS. The TF subprocess inherits `DATABRICKS_HOST`/`DATABRICKS_TOKEN` from
Frontkeep's env — no connector code.

| Service | Resource | Tier |
|---|---|---|
| `databricks-sql-warehouse` | `databricks_sql_endpoint` | human approval (cost-bearing) |
| `databricks-job` | `databricks_job` | human approval (recurring compute) |
| `databricks-model-serving` | `databricks_model_serving` | human approval (serving compute) |
| `databricks-uc-volume` | `databricks_schema` + `databricks_volume` | auto (cheap, self-service) |

Each stamps the immutable `project=<id>` tag, so spend attributes back (below).
Provisioning a serving endpoint **closes the loop** with §1: add its name to the
`databricks` inference module and the gateway fronts it.

## 3. Cost — Databricks spend in Frontkeep's single pane

The `databricks-billing` cost source reads `system.billing.usage` (joined to
`system.billing.list_prices`) via the SQL Statement Execution API, filtered to the
`project` custom tag — so Databricks DBU spend lands **per project** in Frontkeep's
cost dashboard next to AWS and model spend. Set:
```
DATABRICKS_WAREHOUSE_ID=<a SQL warehouse id>   # runs the billing query
```
It refreshes on the **daily rollup loop** and the manual **`POST /api/cost/rollup`**
button (the dashboard's refresh) — not real-time. Until system tables + tag
propagation are in place, it reports "not measured" and the manifest estimate
stands in (it never invents a number).

## What Databricks does *not* replace

Unity Catalog governs *data*; Mosaic AI Gateway governs *one workspace's* models.
Neither is a cross-vendor service orchestrator, neither attributes spend across
AWS + Auth0 + OpenAI + Databricks in one place, and neither exposes an agent MCP
control plane that turns "register → provision → use" into governed tool calls.
That's Frontkeep's job — in front.

## Setup checklist

1. A Databricks **PAT** (`dapi…`) for a service principal that can create
   warehouses/jobs/serving endpoints/UC volumes and `SELECT` on `system.billing`.
2. Env on the Frontkeep process (`.env`): `DATABRICKS_HOST`, `DATABRICKS_TOKEN`, and
   `DATABRICKS_WAREHOUSE_ID` (for the cost source) — the standard Databricks vars.
3. Edit `services/databricks/service.yaml` `models[]` to your serving endpoints +
   pricing.
4. For UC volumes, have a catalog projects may create schemas under.

> Clean-room: real host/token/warehouse values live only in `.env` at runtime —
> manifests reference env var **names** and public model names, never your workspace.
