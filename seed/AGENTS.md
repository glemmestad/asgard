# AGENTS.md

This repository has adopted the **agent seed**. Both humans and AI agents read this file. If you are an AI agent that an engineer has pointed at this repo and told to "examine it and go build," this file is your map. Read it first.

The control plane is the enterprise's hub **for AI and agent development**: a governed catalog of what exists, a gateway every model call routes through, policy, evals, audit, and cost attribution. You do not need to know how the control plane is implemented. You need to know how to *use* it, because building through it is how your work comes out safe, observable, cost-attributed, and built to company standards — instead of as ungoverned shadow AI.

The control plane's **MCP server is your front door.** There is no ticket queue and no human approver on the default path. You discover, register, provision, and call models by invoking MCP tools directly.

---

## The four moves

Do these in order. Stop as soon as you have enough context for the task in front of you.

1. **Examine this repo.** Understand what is being built here and who owns it. Read the README, any existing `AGENTS.md`/`CLAUDE.md` guidance, the code, and the tests before changing anything.
2. **Learn the standards.** Call `list_standards` to see which standard sets apply, then `get_standards` to fetch them. These are the enterprise's coding, security, and workflow conventions — your output must conform. The `.agent/` files in this repo are an offline copy; the MCP tools are the live source of truth, so prefer them when reachable.
3. **Register the project.** Call `register_project` with the project name, owner, and intended data classification. This is a **hard gate**, not a formality: the control plane will not issue a gateway key, route a model call, or provision a resource for an unregistered project, because every chargeable resource must carry a project owner for cost attribution and audit. Registration mints a stable `proj-YYYY-NNNN` id; record it (see "This project's id" below).
4. **Build to the standards.** Write the code. When you need a shared capability (a model, an existing tool/MCP server, storage, a secret), discover it and request it through the control plane rather than wiring a provider directly. Verify your work end to end before claiming it is done.

You should be able to go from an empty repo to a working, governed component **without filing a ticket or waiting on a person.** If you can't, that is a gap — surface it (see the escalation list at the bottom).

**Already have an `AGENTS.md` or `CLAUDE.md`?** Merge, don't replace. Keep the repo's existing guidance and add the Frontkeep sections it lacks — the project id, the MCP tools, the gateway rule. The `.agent/` files are additive.

---

## The MCP tools

These are the tools the MCP server exposes. Use them instead of inventing your own integrations.

| Tool | What it does | When you call it |
|---|---|---|
| `list_standards` | List the available enterprise standard sets (coding, security, workflow, language add-ons). | First, to see what applies. |
| `get_standards` | Fetch the full text of a standard set. | Right after `list_standards`. |
| `bootstrap` | Return the seed plan for this repo with every file's body inlined — the one-shot repo setup. | When setting a repo up, instead of the `seed_plan`/`seed_get` loop. |
| `seed_plan` / `seed_get` | Plan the minimal seed set (core + language add-ons + domain overlays + templates), then fetch each file's body. | The fine-grained alternative to `bootstrap`. |
| `registration_requirements` | What registering/promoting requires per classification tier: evidence fields, budget ceilings, registration policy. | Once, before `register_project`. |
| `register_project` | Register a project; mints a stable `proj-YYYY-NNNN` id with an owner and data classification. | Before requesting any chargeable resource. **Mandatory gate.** |
| `list_services` / `get_service` | Discover the catalog of provisionable services (storage, databases, compute, secrets, LLM access, …) and read one manifest. | Before requesting a resource — to see what exists and what it needs. |
| `request_resource` | Request a service from the catalog for your project. Cheap, reversible types auto-approve; risky ones route to a human. | When your task needs infrastructure. |
| `list_resources` / `get_resource` | List the project's provisioned resources; poll one for async status and outputs. | After requesting, to pick up results. |
| `gateway_credential` | Mint the project's virtual key for the model gateway. | Before making model calls. |
| `get_secret` | Fetch a provisioned secret's value at runtime (audited, never logged). | When your code needs a secret a service minted. |
| `cost_report` / `project_state` | Spend attributed to the project; live budget / kill-switch state. | Before and after expensive work. |
| `promotion_status` / `request_promotion` | Read the promotion checklist; request a one-step classification promotion. | When the project outgrows its tier. |

**Calling models is out-of-band, on purpose.** The MCP server is the control plane: it registers, provisions, and mints credentials. To call a model, mint the project key with `gateway_credential`, then POST to the gateway endpoint (`/api/gateway/chat`, OpenAI-compatible) with that key. Budget, the data-class × model policy, guardrails, audit, and the kill switch all apply there.

Two rules that the whole platform depends on:

- **Every model call goes through the gateway** — your project's key against the gateway endpoint. Calling OpenAI/Anthropic/Bedrock (or any provider) directly bypasses budgets, the data-class × model policy, guardrails, and the audit trail. It is the failure mode the control plane exists to prevent.
- **Every resource belongs to a registered project.** No anonymous spend. If you find yourself about to provision something for an unregistered project, register first.

---

## Discover before you build

The catalog is machine-readable on purpose. Before building something new, check what the org already has:

- `list_services` / `get_service` — the provisionable service catalog (storage, databases, compute, LLM access, secrets, …).
- `mcp_catalog_list` — MCP servers the org has published and vetted.
- `skills_catalog_list` — agent skills the org has shared (install with `skills_catalog_install`).
- `guidance_list` / `recipe_list` — governed how-to playbooks and narrated runbooks.
- `catalog_search` / `catalog_get` — the entity catalog, where the org ingests one.

Reusing a vetted service, tool, or skill is cheaper and safer than re-deriving it. Two things that do the same job is the duplication this catalog exists to prevent.

---

## Reading order if you are an agent

1. **This file.**
2. `list_standards` + `get_standards` (or `.agent/STANDARDS.md` offline) — the conventions your output must meet.
3. `.agent/SECURITY.md` — **required** before you touch auth, secrets, data classification, model selection, or anything network-facing.
4. `.agent/WORKFLOW.md` — when you are about to make changes: branching, the registration gate, requesting resources, and the eval/merge gate that defines "done."

---

## Operating principles (compact)

- Understand before editing. Inspect related files and tests first.
- Prefer narrow, reviewable changes. Match local conventions even if you'd do it differently.
- Do not weaken tests, linters, type checks, evals, or security checks to make a task pass.
- Do not introduce a new dependency without a clear reason.
- Reuse what the catalog already has before building a new entity.
- Route every model call through the gateway; keep every resource attached to a registered project.
- State your assumptions, what you validated, and the remaining risk.
- Never claim a check, test, or eval passed unless you actually ran it.

Full versions are in [`.agent/`](.agent/) and in the live standards. This file is the floor, not the ceiling.

---

## When to stop and ask a human

Pause and surface a question (do not silently decide) when:

- The work involves **`restricted`** data, or any data class above what the project was registered for.
- The work would **promote the project to a higher maturity/criticality tier** (e.g. POC → operational → production-critical).
- The work needs a model that is **not on the allowlist** for the data class you are handling, or a capability that is **not in the catalog**.
- You are about to provision a **production-grade or materially expensive** resource.
- A `register_project` or `request_resource` call fails for a reason you cannot resolve from the error.
- You would have to bypass the gateway, weaken a policy, or disable a guardrail to make the task work. Don't. Ask.

---

## This project's id

`proj-YYYY-NNNN`  — replace with the id returned by `register_project` once you have it. Keep it in this file so the next agent that touches the repo knows the project is already registered.
