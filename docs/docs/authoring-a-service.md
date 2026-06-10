---
sidebar_position: 5
title: Author a service
---

# Author a service

> **Audience: platform operators** extending the catalog. A new provisionable
> service is a **manifest** (plus, for the universal Terraform path, a module) —
> **no recompile, no core change**. This is the service-module contract the
> [inference backends](./inference-backends.md) and [Databricks](./databricks.md)
> pages build on.

A service definition declares two things at once: **how** a project provisions the
service (`provisioner.connector` + `config`) and **how** its cost is attributed
(`cost.source.type`). Drop a `service.yaml` (and a Terraform module if you use the
`terraform` connector) and the service shows up in the catalog, the UI, the CLI,
and as an MCP `request_resource` target.

## Where manifests live

- **Built-in defaults** are embedded in the binary from `services/<id>/service.yaml`.
- **Operator overlay**: point `provisioning.services_dir` (config) or
  `FRONTKEEP_TF_MODULES_DIR` + a services directory (env) at a directory of
  `service.yaml` files to add or override services at runtime. Overlay entries with
  an existing `id` replace the built-in one.

One service per directory: `services/<id>/service.yaml`, optionally a sibling
`README.md` referenced by `documentation:`.

## The manifest contract

Required: `id`, `name`, `category`, `provisioner`, `cost`. Everything else is
optional with a sensible default. The fully-commented skeleton below is the
canonical reference; the authoritative field list is `schemas/service.schema.json`
in the repo (what the UI and tooling read).

```yaml
# services/<id>/service.yaml — a fully-commented template.
# Copy, strip the comments you don't need, and drop it under services/ (built-in)
# or your overlay dir (runtime). One service per file.

id: my-service                 # unique resource type; the request_resource `type`
name: My Service               # human label in the UI/CLI
category: storage              # grouping: storage | database | compute | tooling | llm | …
status: live                   # live | deprecated (deprecated hides it from new requests)
description: One line on what this provisions and when to pick it.

# --- governance: who may provision it, and whether it self-services -------------
classification_min: poc            # lowest project classification allowed (default: any)
classification_max: critical-path  # highest (default: any)
auto_approvable: true              # true → self-service within cost/classification caps.
                                   # Set false for cost-bearing or IAM-shaping services
                                   # so every request routes to human review.
required_fields: [name]            # spec fields a request must supply (validated up front)

# --- how it's provisioned -------------------------------------------------------
provisioner:
  connector: terraform         # terraform (universal) | exec | http | mcp | litellm | stub
  config:                      # connector-specific, passed through opaquely
    module: aws/my-service     # terraform: module path under the modules dir
    cloud: aws                 # recorded on the resource + used for tag/target checks
    # defaults:                # optional operator-env-sourced tfvars the agent never sets
    #   subnet_group_name: "${FRONTKEEP_MY_SUBNET}"

# --- latency + resilience -------------------------------------------------------
long_running: false            # latency hint ONLY. true → request returns its
                               # `provisioning` record immediately instead of waiting
                               # the inline budget; the apply runs in the background
                               # either way. Set true for slow services (RDS/ALB/ECS).
                               # Never a correctness lever — durability is identical.
retry:                         # per-service override of the auto-retry policy (optional).
  max_attempts: 5             # auto-retries of a failed apply/destroy. 0 disables
                               # auto-retry for this service (still manually retryable) —
                               # use for expensive/destructive ones. Default: fleet
                               # `provision_max_retries` (5).
  base_secs: 60               # first-retry backoff; doubles each attempt. Default 60.
  cap_secs: 3600              # max backoff between retries. Default 3600 (1h).

# --- secrets --------------------------------------------------------------------
secret_outputs: [master_password]  # output keys whose VALUES are secrets: the connector
                                   # routes them to the secret store and records only a
                                   # reference, never the value.

# --- cost attribution -----------------------------------------------------------
cost:
  model: usage                 # usage | flat | free (display/forecast hint)
  estimated_monthly_usd: 5.0   # pre-provision estimate; drives the auto-approve cost gate
  source:
    type: aws-cost-explorer    # none | flat | gateway | aws-cost-explorer | gcp-billing |
                               # azure-cost-management | databricks-billing | litellm | exec

# --- discovery ------------------------------------------------------------------
tags: [storage, aws]
documentation: services/my-service/README.md
```

### Cost-bearing variants (optional)

When one service spans cheap and expensive shapes (instance sizes, GPU vs CPU),
add a `variants` block keyed to a spec field instead of splitting into many
services. Small variants auto-approve on cost; big or sensitive ones gate to a
classification or force human review. See `services/ec2-instance/service.yaml` for
a worked example.

## The Terraform path

`connector: terraform` is the universal path and needs no service-specific code:

1. Write `modules/<cloud>/<id>/*.tf` (resolved against the configured modules dir).
2. The connector writes **every spec field** plus the immutable project `tags` as
   tfvars, runs `terraform apply -auto-approve -no-color`, and persists state
   **encrypted in Frontkeep's own DB** (the work dir is just scratch).
3. Outputs named in `secret_outputs` route to the secret store; the rest land on
   the resource record.

Non-AWS is the same path — `modules/databricks/*`, `modules/auth0/*` prove the
connector is provider-agnostic (the Terraform subprocess inherits provider creds
from Frontkeep's environment).

Other connectors: `exec` (run a command, parse JSON stdout), `http`/`mcp`, and
`stub` (the dry-run fallback used when a declared connector isn't registered, so
the single binary works out of the box).

## Run-logs: every run is captured for audit

Every connector run — `apply` and `destroy`, **success and failure** — is captured
to the per-resource **run-log**: the full Terraform plan+apply output (or exec
output / HTTP response), timestamped and attached to the resource. This is the
operator's window to debug a failure *or verify a success*.

- **Encrypted at rest** (AES-256-GCM with the secret-store master key) — the output
  can contain provider secrets, the same exposure `tf_state` already has, so it gets
  the same protection.
- **Read-gated to `ViewAudit`** (admin + finance). Surfaces:
  - REST `GET /api/projects/{id}/resources/{rid}/runs`
  - MCP `resource_runs` (user token with audit access)
  - UI: a **Logs** button on each resource row in the project detail (shown only to
    audit-capable viewers).

You author nothing for this — it's automatic for the `terraform` and `stub`
connectors. (`exec`/`litellm` capture is on the roadmap.)

## Auto-retry & manual retry

A failed `apply`/`destroy` auto-retries with capped exponential backoff (default 5
attempts, 60s doubling to a 1h cap). Tune per service with the `retry` block above,
or fleet-wide with the `provision_max_retries` config (`0` disables auto-retry
everywhere). Once a row exhausts its attempts it rests as `failed`/`destroy_failed`
and an operator can **Retry** it manually (UI button, REST
`POST …/resources/{rid}/retry`, or MCP `retry_resource`) — which re-arms it in place
and drives it immediately, bypassing the backoff window.

## Validate before you ship

Service manifests are validated by the **catalog loader at startup**: every
`service.yaml` is parsed and its connector checked, and any that fail to load are
logged and skipped — so a bad overlay surfaces in the server log instead of
silently disappearing. Watch the log on boot:

```
terraform connector registered (modules=…)
service overlay <dir> failed: …      # ← a malformed manifest shows up here
```

`schemas/service.schema.json` is the authoritative field reference (what the UI
and tooling read). (`frontkeep validate` today only lints *entity* manifests — agents
and the like, which carry a `kind` — not `service.yaml`; see the roadmap.)

Then provision it end-to-end against a running binary (or the `stub` connector
offline) and confirm the resource reaches `provisioned`, the cost shows up, and —
as an admin — the run-log captured the apply output.
