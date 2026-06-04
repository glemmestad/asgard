# AGENTS.md

This repository has adopted the **agent seed**. Both humans and AI agents read this file. If you are an AI agent that an engineer has pointed at this repo and told to "examine it and go build," this file is your map. Read it first.

The control plane is the enterprise's hub **for AI and agent development**: a governed catalog of what exists, a gateway every model call routes through, policy, evals, audit, and cost attribution. You do not need to know how the control plane is implemented. You need to know how to *use* it, because building through it is how your work comes out safe, observable, cost-attributed, and built to company standards — instead of as ungoverned shadow AI.

The control plane's **MCP server is your front door.** There is no ticket queue and no human approver on the default path. You discover, register, provision, and call models by invoking MCP tools directly.

---

## The four moves

Do these in order. Stop as soon as you have enough context for the task in front of you.

1. **Examine this repo.** Understand what is being built here and who owns it. Read the existing manifests (`agent.yaml`, `prompt.yaml`, `mcp.yaml`, `eval.yaml`, `dataset.yaml`), the code, and the tests before changing anything.
2. **Learn the standards.** Call `list_standards` to see which standard sets apply, then `get_standards` to fetch them. These are the enterprise's coding, security, and workflow conventions — your output must conform. The `.agent/` files in this repo are an offline copy; the MCP tools are the live source of truth, so prefer them when reachable.
3. **Register the project.** Call `register_project` with the project name, owner, and intended data classification. This is a **hard gate**, not a formality: the control plane will not issue a gateway key, route a model call, or run a sandboxed invocation for an unregistered project, because every chargeable resource must carry a project owner for cost attribution and audit. Registration mints a stable `proj-YYYY-NNNN` id; record it (see "This project's id" below).
4. **Build to the standards.** Write the code. When you need a shared capability (a model, an existing tool/MCP server, storage, a secret), discover it and request it through the control plane rather than wiring a provider directly. Verify your work end to end before claiming it is done.

You should be able to go from an empty repo to a working, governed component **without filing a ticket or waiting on a person.** If you can't, that is a gap — surface it (see the escalation list at the bottom).

---

## The MCP tools

These are the tools the MCP server exposes. Use them instead of inventing your own integrations.

| Tool | What it does | When you call it |
|---|---|---|
| `list_standards` | List the available enterprise standard sets (coding, security, workflow, language add-ons). | First, to see what applies. |
| `get_standards` | Fetch the full text of a standard set. | Right after `list_standards`. |
| `seed_plan` | Given this repo's languages + a description of the work, return the minimal set of seed files to add (core + language add-ons + domain overlays + templates). | When setting a repo up, to pull only the guidance the work needs. |
| `seed_get` | Fetch one seed file's body + the path to write it to. | For each file `seed_plan` returned. |
| `register_project` | Register a project; mints a stable `proj-YYYY-NNNN` id with an owner and data classification. | Before requesting any chargeable resource. **Mandatory gate.** |
| `catalog_search` | Search the entity catalog by kind and/or free-text query. | To discover existing agents, prompts, tools, models, datasets, evals before building your own. |
| `catalog_get` | Fetch a single entity by `kind` / `namespace` / `name`. | To read the full spec of something you found. |
| `request_resource` | Request a shared resource for your project (storage, secret, tool access, etc.) through the governed catalog. | When your task needs shared infrastructure. |
| `gateway_credential` | Issue a per-project virtual key for the model gateway. | Before making model calls. |
| `gateway_chat` | Invoke an allowed model **through the gateway** (budget, policy, guardrails, audit, kill switch all apply). | For every model call. Never call a provider SDK directly. |
| `cost_report` | Report spend attributed to a project. | To check budget before/after work. |

Two rules that the whole platform depends on:

- **Every model call goes through `gateway_chat`.** Calling OpenAI/Anthropic/Bedrock (or any provider) directly bypasses budgets, the data-class × model policy, guardrails, and the audit trail. It is the failure mode the control plane exists to prevent.
- **Every resource belongs to a registered project.** No anonymous spend. If you find yourself about to provision something for an unregistered project, register first.

---

## Discover before you build

The catalog is machine-readable on purpose. Before writing a new agent, prompt, tool, or eval, search for one that already exists:

- `catalog_search` with a `kind` (e.g. `Agent`, `Prompt`, `Tool`, `Model`, `Dataset`, `Eval`, `Project`) and/or a `query`.
- `catalog_get` to read the full spec of a hit.

Reusing a vetted entity is cheaper and safer than re-deriving it. Two entities that do the same thing is the duplication this catalog exists to prevent.

Entities are referenced by **EntityRef**: `kind:namespace/name` (e.g. `agent:default/code-reviewer`, `group:default/platform`, `model:default/gpt-4o`). Namespace defaults to `default`.

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
