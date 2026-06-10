# Frontkeep control plane (this hub)

The governance hub exposes itself over MCP. Once connected, an agent can register
a project (the mandatory gate), discover and provision services, read cost, and
pull the company standards, guidance, and recipes — the whole onboarding loop,
agent-first.

## Prerequisites

- A **Personal Access Token** (`asg_pat_…`). Create one on the Users page; it acts
  across every project you own or manage. Export it as `FRONTKEEP_PAT` and reference
  `$FRONTKEEP_PAT` in the install snippet rather than pasting the literal token.
- The URL is your deployment's `/mcp` endpoint — replace the example host with
  your own.

## What you get

- `register_project`, `request_resource`, `seed_plan`, `cost_report`,
  `guidance_*`, `recipe_*`, `mcp_catalog_*`, and more.
- Inference is **not** here — it's service usage: mint the project LLM key and
  call the gateway out-of-band.

See the **Getting started** tab for the exact per-client connection snippets.
