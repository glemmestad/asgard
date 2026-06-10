---
sidebar_position: 2.2
title: Connect an agent (MCP)
---

# Connect an agent to Asgard

This is the **user** side: you have an Asgard instance running (someone deployed
it — see the [operator guide](./deploy.md)) and you want to point your coding
agent at it and start building. Asgard's MCP server is the **control plane** —
your agent discovers services, registers projects, provisions resources, and
fetches the credentials Asgard mints, all by invoking MCP tools.

You need two things: the **base URL** of the Asgard instance and a credential.
There are two kinds, and the difference matters:

- **User token** (`asg_pat_…`) — a long-lived credential tied to *you*. One token
  lets your agent register new projects and act on **every project you own or
  manage**. This is what you connect your agent with. Create it in the dashboard
  (Getting started → "Create a PAT", or the account/token affordance).
- **Project key** (`asg_…`) — scoped to a *single* registered project. Use it for
  a deployed app or CI that lives in one project. Minted per project.

> **Control plane vs. service usage.** The Asgard MCP is the control plane:
> register/manage projects, request/manage services, read cost, and fetch the
> credentials Asgard minted. *Using* a provisioned service — calling an LLM,
> reading a bucket, hitting a DB — is **service usage**: it uses that service's
> own credential, out-of-band from Asgard. A user token never calls a service.
> In particular, **inference does not go through MCP** (see
> [Calling models](#calling-models-the-inference-path) below).

## Connect your client

Asgard exposes MCP over **Streamable HTTP at `https://<host>/mcp`**, authenticated
with your **user token** as a bearer credential. Asgard's `/mcp` uses bearer-PAT
auth (not OAuth), so the token is supplied by the client at setup rather than
prompted on first connect. How it's supplied differs per client — see each below.

The easiest place to get a ready-to-paste, token-filled command is the **Getting
Started** page in the running UI: mint a PAT and it generates the exact snippet for
each client.

### Claude Code

`claude mcp add` **bakes the header value at add-time** — the shell expands the
token then and stores it in `~/.claude.json`. So put the real token in the command;
an unset `$ASGARD_PAT` would silently store an empty bearer and every call would
fail with `401`:

```sh
claude mcp add --transport http asgard https://<host>/mcp \
  --header "Authorization: Bearer asg_pat_your_user_token"
```

To keep the token out of `~/.claude.json`, export it (`export
ASGARD_PAT=asg_pat_…`) and use `$ASGARD_PAT` in the header instead — but only if
it's set in the shell you run the command in, since it's expanded immediately.

Then `claude mcp list` should show `asgard` connected, and the Asgard tools
(`list_services`, `register_project`, `request_resource`, `seed_plan`, …) are
available in the session.

### Codex

Add to `~/.codex/config.toml` — `bearer_token_env_var` sources the PAT from your
environment at call time, so the token never lands in the file:

```toml
[mcp_servers.asgard]
url = "https://<host>/mcp"
bearer_token_env_var = "ASGARD_PAT"
```

### Cursor / generic Streamable-HTTP clients

Add to your client's MCP config (e.g. Cursor's `~/.cursor/mcp.json` or a
project `.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "asgard": {
      "url": "https://<host>/mcp",
      "headers": { "Authorization": "Bearer ${ASGARD_PAT}" }
    }
  }
}
```

### MCP Inspector (to kick the tyres)

```sh
npx @modelcontextprotocol/inspector
```

Point it at `https://<host>/mcp`, transport **Streamable HTTP**, and add an
`Authorization: Bearer <your user token>` header. `initialize` should
negotiate and `tools/list` should show the catalog.

### Local / stdio

For a local instance, run the server over stdio instead of HTTP — no token needed
(local trust); set the default project via env:

```sh
ASGARD_PROJECT=proj-2026-0001 asgard mcp
```

Wire it as a stdio MCP server in your client (command `asgard`, args `mcp`).

## Get a credential

- **User token** — sign in to the dashboard and create one (Getting started →
  "Create a PAT"). It's shown once; store it like a password. Revoke it any time.
  This is the agent credential: it can register projects and acts as you across
  the projects you own or manage. On a fresh deploy with no users yet, the
  operator mints the first one with `asgard admin bootstrap` (see
  [Deploy → The first credential](deploy.md#the-first-credential)).
- **Project key** — minted per **registered project** (dashboard, or your agent
  calling `gateway_credential`). Use it for an app/CI scoped to one project, or as
  the project's **LLM key** for the gateway (see below).

Both are gated: a missing or invalid token gets `401`. An unregistered project
can't mint a project key.

## First moves once connected

The flow the [agent seed](./onboarding-loop.md) encodes, all as MCP tool calls:

1. **`list_standards` / `get_standards`** — the engineering/security/workflow
   conventions your output must meet. These are **read-only over MCP** — standards
   are normative, edited only by an admin in the dashboard (and every edit is
   versioned). `guidance_list` / `recipe_list` round these out with advisory
   playbooks and runbooks; `guidance_put` / `recipe_put` let an agent contribute,
   but a submission lands as a **draft** until an admin approves it.
   **`mcp_catalog_list` / `mcp_catalog_get`** browse the **MCP catalog** — MCP
   servers the org has shared, each with a structured install spec and an owner
   (contact). On a **user token**, `mcp_catalog_publish` shares one you built
   (owned by you, listed *user-submitted* until an admin promotes it to
   *company-approved*); `mcp_catalog_set_state` disables or archives one you own.
   This catalog is intentionally separate from the provisioning catalog above — it
   is opt-in sharing, not derived from what your projects provisioned.
2. **`seed_plan`** — give it your repo's languages and a one-line description of
   the work; it returns the minimal set of guidance files to drop in (core +
   language add-ons + domain overlays + templates). **`seed_get`** fetches each
   file's body and the path to write it to.
3. **`register_project`** — the gate. Mints `proj-YYYY-NNNN`; nothing chargeable
   happens without it. On a **user token** the owner is stamped from your identity
   (you're immediately authorized to provision the project you just created), and
   `manager`/`group` are required or optional per the operator's policy.
4. **`list_services` / `request_resource`** — discover what you can provision
   (storage, secrets, an Auth0 app, an inference gateway, …) and request it;
   self-service types provision immediately, review-tier types await approval.
   **`list_resources`** shows what the project already has (id, type, state,
   outputs). To let one resource reach another — say an `ecs-service` that must
   read an `s3-bucket` or `dynamodb-table` — call **`request_grant`** with the two
   ids and a level (defaults to `write` = read+write). The grant is itself a
   provision request: same-project only, filed and audited like any other, with
   the binding owned by the target's manifest, so a new target kind needs no core
   change. Your own project's grants are self-service.
5. **`gateway_credential`** — mint the project's LLM virtual key, then call models
   **out-of-band** (see below). Handing you a credential is control plane; using
   it is not.
6. **`get_secret`** — fetch a provisioned secret value (audited, never logged). On
   a user token pass `project_id`; on a project key omit it.

On a **user token**, project-scoped tools require a `project_id` and authorize it
against what you own or manage — you can't touch a project you have no authority
over. On a **project key**, the project is locked to the key and a mismatched
`project_id` is denied.

## Calling models (the inference path)

`gateway_chat` is **not** an MCP tool — inference is service usage, not control
plane. To call a model:

1. Request the inference/gateway service for your project (`request_resource`), or
   mint the project's LLM key with **`gateway_credential`**.
2. Call the gateway's **OpenAI-compatible** endpoint directly with that key,
   out-of-band from MCP:

```sh
curl -sS https://<host>/api/gateway/chat \
  -H "Authorization: Bearer asg_your_project_llm_key" \
  -H 'content-type: application/json' \
  -d '{"model":"model:default/gpt-5","messages":[{"role":"user","content":"hi"}],"data_class":"internal"}'
```

Every call still flows through the governed gateway (budget, policy, guardrails,
audit, kill switch) — that governance lives in the gateway, not in MCP. Never wire
a provider SDK directly.

## Troubleshooting

- **`401` on every call** — the bearer token must be a valid **user token**
  (`asg_pat_…`) or **project key** (`asg_…`), not a human session token. Re-mint
  one in the dashboard.
- **`404` on `/mcp`** — wrong path; it's exactly `/mcp` on the Asgard host.
- **"project_id is required for a user token"** — a user token isn't scoped to one
  project, so project-scoped tools need you to name which one.
- **"not authorized for project …"** — you named a project you don't own or
  manage. Register it (you become the owner) or ask its owner/manager.
- **"cross-project access denied"** — on a project key, you passed a `project_id`
  that isn't the key's project. Drop the argument; the key's project is used.
- **Looking for `gateway_chat`?** It's gone from MCP by design — use the gateway
  endpoint with the project LLM key (see [Calling models](#calling-models-the-inference-path)).
- **Tools missing** — confirm `initialize` succeeded and you sent
  `notifications/initialized`; most clients handle the handshake for you.
