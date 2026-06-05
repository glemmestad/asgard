---
sidebar_position: 2
title: Getting started
---

# Getting started

:::tip You need a running Asgard instance
Almost certainly your company already gave you one — that's how you're reading
these docs. Use its URL wherever you see `<host>` below. Standing one up
yourself? [Deploy your own](#deploy-your-own) is at the bottom.
:::

This is the whole loop, once: connect your agent, pull your company's standards,
register a project, and provision what it needs — all governed and
cost-attributed.

**How you drive it:** Asgard's capabilities are **MCP tools**, so you don't type
slash commands — you *ask your agent* in plain English and it calls them. Each
step below is what to say.

## 1. Create a token

In the dashboard: **Getting started → Create a PAT**. You get a user token
(`asg_pat_…`), shown once. It's your agent's long-lived credential — it can
register projects and act on every project you own or manage. Copy it; you paste
it into your agent's config next.

## 2. Connect your agent

Add Asgard's MCP server, pasting your token in as the value. For Claude Code:

```sh
claude mcp add --transport http asgard https://<host>/mcp \
  --header "Authorization: Bearer asg_pat_paste_your_token_here"
```

It's saved in your client's config — one-time setup, the token persists, no
environment variable to re-export. `claude mcp list` should then show **asgard**
connected. (Codex, Cursor, the MCP Inspector: [Connect an agent](./connect-agent.md).)

## 3. Open your repo and pull the standards

`cd` into your project — if it's brand new, `git init` first. Then tell your agent:

> **"Pull the Asgard seed into this repo."**

In Claude Code there's a shortcut for this exact step — the slash command
**`/mcp__asgard__bootstrap`** (other clients namespace it differently). Either way
the agent calls the `bootstrap` tool, which returns `AGENTS.md` (the map the next
agent reads first) and the `.agent/` coding and security standards in one shot, and
writes them in. From here your agent builds to your company's conventions, and the
live, versioned standards stay available over MCP.

## 4. Register the project

The gate: nothing provisions or spends until a project is registered. Tell your
agent:

> **"Register this project with Asgard."**

It asks you for whatever it needs — owner, manager, cost-center, data
classification, budget — and mints a `proj-YYYY-NNNN` id. You're the owner, so
you can provision it right away.

## 5. See what you can provision

> **"What can I provision through Asgard?"**

Storage, secrets, databases, compute, an LLM gateway — whatever your operator has
enabled.

## 6. Provision what the project needs

Just ask. Cheap, reversible things (storage, secrets) provision immediately;
cost-bearing ones (databases, compute) route to a manager for approval.

**A bucket for permanent file storage:**

> **"Give this project a private S3 bucket for file storage."**

**An LLM key — for your application's inference:**

> **"Mint this project's LLM key."**

:::caution This key is for app inference, not your dev tools
The project LLM key exists so your **deployed application** can call a model
through the governed gateway — budget, the data-class × model policy, guardrails,
audit, and the kill switch all apply, and the spend lands on this project.

It is **not** a general-purpose key for your coding assistant, your terminal, or
ad-hoc chat. Your coding agent already talks to Asgard's *control plane* over MCP;
it does not use this key. If you just want to experiment with a model yourself,
that's not what this is. Mint it when the product itself needs to query an LLM.
:::

Your app then calls the gateway's OpenAI-compatible endpoint with that key. This
is the one thing that isn't an MCP call — it's the app *using* the service:

```sh
curl -sS https://<host>/api/gateway/chat \
  -H "Authorization: Bearer asg_your_project_llm_key" \
  -H 'content-type: application/json' \
  -d '{"model":"model:default/gpt-5.1","messages":[{"role":"user","content":"hi"}],"data_class":"internal"}'
```

Whether that lands on the built-in OpenAI/Anthropic floor or an enterprise
LiteLLM/Databricks backend is the operator's choice — your code is identical
either way.

<details>
<summary><strong>Optional: run a container (ECS)</strong></summary>

> **"Run this project's container as an ECS task, image `<your-ecr-image>`."**

Same governed path — the task is tagged with your project, so its cost lands in
the same rollup. For a long-running service behind a load balancer, ask for an
ECS service instead.

:::note Provisioning backend
The shipped backend is a **dry-run stub**: it computes the plan, tags, and a cost
estimate and returns deterministic outputs without touching any cloud — enough to
drive the whole request → approve → fulfill → cost loop. A live cloud backend
implements the same connector contract and is selected by configuration; turning
it on is an explicit operator decision.
:::

</details>

## 7. See the cost

> **"Show this project's spend."**

Every model call and resource is attributed to the project (and its owner /
manager / group). To stop everything instantly, tell your agent to **kill the
project** — the next gateway call is rejected and no further spend can land.

## Where to go next

- **[Connect an agent (MCP)](./connect-agent.md)** — Codex/Cursor/Inspector setup,
  the full tool list, user-token vs project-key rules, and troubleshooting.
- **[Governed onboarding loop](./onboarding-loop.md)** — the model behind this flow.
- **[Inference backends](./inference-backends.md)** / **[Databricks](./databricks.md)**
  — operator-side, for putting LiteLLM/Databricks behind the gateway.

---

## Deploy your own

Most readers can skip this — you're already on a company instance. To stand one
up, Asgard is a single binary; the default path needs only a Git token and SQLite:

```sh
# Docker (SQLite, embedded UI)
docker run -p 8080:8080 -e ASGARD_GIT_TOKEN=ghp_xxx ghcr.io/asgard/asgard:latest

# or from source
cargo run -p asgard -- serve --database-url sqlite://asgard.db
```

Open `http://localhost:8080` for the UI, then come back to step 1. For a real
deployment (Postgres, replicas, TLS) see the [operator deploy guide](./deploy.md).

**Inference backend.** A small shop sets `OPENAI_MASTER_KEY` and/or
`ANTHROPIC_MASTER_KEY` on the process and the gateway serves models immediately —
nothing else to deploy. An enterprise points the same gateway at LiteLLM,
Databricks, or any OpenAI-compatible backend; it's invisible to everything above.
See [Inference backends](./inference-backends.md).
