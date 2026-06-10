---
sidebar_position: 2.5
title: Governed onboarding loop
---

# Governed onboarding loop

Frontkeep's headline workflow is a **governed onboarding loop** for AI/agent work, designed agents-first: every capability below is an MCP tool, not just a UI button.

> Point any new effort's agent at the seed → it learns the standards → it registers the project (the gate) → that unlocks gateway keys and resources → all spend is attributed by project / owner / manager / group.

## 1. The seed

`seed/` is the repo you point people at: *"tell your AI agent to read `AGENTS.md`, then go build."* Its `AGENTS.md` encodes the four-move loop and names the MCP tools; `.agent/{STANDARDS,SECURITY,WORKFLOW}.md` are the enterprise standards. Those same files are embedded in the binary and seed the standards store on first boot, so a fresh deployment's served standards match the repo. From there an admin can edit them in the dashboard (every edit is versioned); agents always read the authoritative served copy over MCP.

```text
list_standards / get_standards   →  the conventions your output must meet
register_project                 →  the mandatory gate (mints proj-YYYY-NNNN)
list_services / get_service      →  discover what you can provision
request_resource                 →  provision infra for your project
gateway_credential               →  mint the project's LLM key
cost_report                      →  spend attributed to your project
```

Inference itself is deliberately **not** an MCP tool — it's service usage, not
control plane. Mint the key, then POST to `/api/gateway/chat` with it (see
[Connect an agent](connect-agent.md)).

## 2. Registration is the gate

A project must be **registered and active** before Frontkeep will mint a gateway key or provision a resource. Registration records the owner, manager, group/cost-center, classification and budget, and mints a stable `proj-YYYY-NNNN` id.

```bash
frontkeep project register \
  --name "Fraud Detection" \
  --owner alice@corp.example --manager bob@corp.example \
  --group platform --classification poc --budget-usd 100
```

- `owner` and `manager` are self-entered emails (unverified — no OIDC required) and must differ.
- `group` must be one of the cost-centers the deployer allows (see below). Discover them with `list_groups` / `GET /api/groups`.
- An unregistered or decommissioned project gets a `403` when it tries to mint a key or request a resource.

### Operator-configured cost-centers

The deployer pre-specifies the valid groups in `asgard.yaml`. An empty list means "open mode" (any group accepted, recorded as-is).

```yaml
groups:
  - { key: platform, display_name: Platform Engineering, cost_center: CC-1001 }
  - { key: research,  display_name: Research,             cost_center: CC-2002 }
```

## 3. Cost segregated by dimension

Every gateway call is attributed to its project, and the owner / manager / group / cost-center / classification are denormalized onto each usage event at write time — so cost rolls up by any of them with a single query, and historical spend stays attributed to who owned the project when the cost was incurred.

```bash
frontkeep cost report --by group        # or: project | owner | manager | classification | model | provider
# REST:  GET /api/cost?by=group
# MCP:   cost_report { "by": "group" }
```

## 4. Provisioning through the orchestrator

Resources are requested through Frontkeep so they're owned, governed, and cost-tagged. Cheap, reversible types (object storage, key/value tables, secrets) auto-provision; high blast-radius types (databases, compute) require platform approval before they're fulfilled.

```bash
# MCP: request_resource { project_id, resource_type: "s3-bucket", name: "assets", spec: {...} }
# REST: POST /api/projects/{id}/resources
```

Every provisioned resource is stamped with `project=<id>` (and owner/group/cost-center) so its cost flows into the same rollup as model spend.

:::note Provisioning backend
The universal backend is the **Terraform connector**: a service manifest plus a Terraform module, no core code. A service without live credentials falls back to a dry-run stub that computes the plan, tags, and a cost estimate — enough to drive the whole request → approve → fulfill → cost loop without touching a cloud. Arming live provisioning is an explicit operator decision.
:::

## 5. Adopting an existing system (brownfield)

The same loop works for a system that already exists outside Frontkeep — nothing
is re-provisioned, and Frontkeep never takes over infrastructure it didn't create:

1. **Merge the seed.** Run `bootstrap` against the existing repo. New files are
   written; an existing `AGENTS.md`/`CLAUDE.md` is merged, not replaced.
2. **Register provisional.** `register_project` with `provisional: true` (CLI:
   `--provisional`). The project is **fully live** — gateway keys, resources,
   cost attribution all work — but its lifecycle is `provisional`, flagging it
   for governance triage instead of blocking it.
3. **Link the existing infrastructure.** `link_resource` records each existing
   stack with its cost source (`aws-cost-explorer`, `databricks-billing`, …)
   and a monthly estimate. Frontkeep does not manage it; you tag the real cloud
   resources `project=<id>` yourself, and the source attributes actual spend by
   that tag — into the same rollup as everything else.
4. **Graduate or retire.** The first successful promotion (real evidence,
   machine-checked) flips the project to `active`; a system not worth keeping
   is decommissioned through the normal lifecycle.

To *re-provision* an existing app onto Frontkeep-managed primitives instead, see
[Migrate an app](migrate-app.md) — a different journey.
