# Agent seed

This folder is the **entry point an AI agent reads before building in this repository.** Drop it at the root of a repo (so `AGENTS.md` and `.agent/` sit alongside your code) and point your agent at it.

## What it's for

The enterprise runs a **control plane** for AI and agent development: a governed catalog, a gateway every model call routes through, policy, evals, audit, and cost attribution. This seed is how a repo opts into that: it orients any AI agent so that work comes out **built to company standards, governed, and cost-attributed** — instead of as ungoverned shadow AI.

## How to use it

Tell your AI agent:

> Read `AGENTS.md` in this repo, then go build `<your idea>`.

The agent will:

1. **Examine this repo** to see what's here and who owns it.
2. **Fetch the enterprise standards** via the `list_standards` / `get_standards` tools (the `.agent/` files are the offline copy).
3. **Register the project** with `register_project` — a mandatory gate that mints a stable `proj-YYYY-NNNN` id, so every resource is owned and every cost is attributed.
4. **Build to the standards**, discovering and requesting shared capabilities (models, tools, storage, secrets) through the MCP tools and routing every model call through the gateway — never wiring a provider directly.

You should be able to go from an empty repo to a working, governed component without filing a ticket.

## What's in here

| Path | Purpose |
|---|---|
| `AGENTS.md` | The entry point. The agent's map: the four-move loop, the MCP tools, the reading order, and when to stop and ask a human. |
| `.agent/STANDARDS.md` | Enterprise coding/CI/commit/dependency standards the agent's output must meet. Offline copy of the live standards. |
| `.agent/SECURITY.md` | Data-classification handling, the no-secrets and no-shadow-AI rules, least privilege, and what not to do. |
| `.agent/WORKFLOW.md` | How the agent works: branch, register, request resources via the control plane, and pass the eval/merge gate. |

Start with [`AGENTS.md`](AGENTS.md).
