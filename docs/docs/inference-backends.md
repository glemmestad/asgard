---
sidebar_position: 6
title: Inference backends (operator)
---

# Inference backends

> **Audience: platform operators** deploying Asgard. End users and agents never
> read this — they just request an LLM key for their project and get one. Which
> backend serves it is invisible to them.

Inference is a **service module**, like any other (see the service-module contract
in `plans/SERVICE-MODULE.md`). Asgard's control plane is constant — register →
project token → governed inference → cost + audit — and the *backend* behind it is
swappable. AI inference is the loudest tenant, not the whole building.

## Out of the box (the lightweight floor)

Asgard ships two built-in inference modules. Each activates the moment its master
key is present — no other setup:

| Module | Enable by setting | Models |
|---|---|---|
| `openai` | `OPENAI_MASTER_KEY` | `model:default/gpt-5.1`, … |
| `anthropic` | `ANTHROPIC_MASTER_KEY` | `model:anthropic/claude-sonnet` |

That's the whole story for one or two providers. A project mints a token, calls the
gateway, and spend is attributed per token — the master key never leaves the
platform. The project id is forwarded downstream as the OpenAI `user` /
Anthropic `metadata.user_id`, so the provider's own logs/spend carry it too.

You do **not** need LiteLLM for this.

## Scaling out: LiteLLM (optional, operator-deployed)

When you want 100+ providers, fallbacks, or routing, enable the **`litellm`
module** — a standard module that ships with Asgard. You do not author a service
definition; you stand up LiteLLM and point Asgard at it.

### 1. Deploy LiteLLM
LiteLLM is its own process — run it under *your* controls (HA, network, secrets),
not bundled inside Asgard. Two common paths:

- **docker-compose** (local / single host):
  ```yaml
  services:
    litellm:
      image: ghcr.io/berriai/litellm:main-latest
      command: ["--config", "/app/config.yaml"]
      ports: ["4000:4000"]
      volumes: ["./litellm.config.yaml:/app/config.yaml"]
  ```
- **Kubernetes / Helm** — deploy the LiteLLM chart as its own Deployment+Service
  alongside the Asgard release.

Configure LiteLLM's providers/keys per its own docs:
[docs.litellm.ai/docs/proxy/configs](https://docs.litellm.ai/docs/proxy/configs).

### 2. Point Asgard at it
Set on the Asgard process:
```
LITELLM_BASE_URL=http://litellm:4000     # the proxy URL
LITELLM_MASTER_KEY=sk-...                 # the LiteLLM master key
```
The `litellm` module activates on `LITELLM_BASE_URL` (it's OpenAI-compatible).
Asgard now routes inference through LiteLLM while keeping all governance — tokens,
budgets, policy, guardrails, audit, per-project cost — exactly the same.

> **This gated path needs no LiteLLM database.** Asgard only proxies chat
> completions through LiteLLM on the one shared master key, so LiteLLM can run
> **config-only / stateless** (no `DATABASE_URL` on the proxy). Key issuance,
> budgets, and cost attribution all live in Asgard. The per-project path below is
> the one that needs LiteLLM's own DB.

### 3. (Optional) Make it the default backend
Point the catalog's default model ref at the `litellm` module's model, or set the
project default-backend rule. Projects keep requesting "an LLM key"; they never
know the backend changed.

### 4. (Optional) Per-project LiteLLM keys
The `litellm` module above fronts LiteLLM through Asgard's gateway on one shared
master key. To instead give each project its **own** budgeted LiteLLM key — so the
project calls LiteLLM directly and Asgard pulls the key's spend back — the same two
env vars also enable the **`litellm-key`** service:
```
LITELLM_BASE_URL=http://litellm:4000
LITELLM_MASTER_KEY=sk-...
```
A project requests `litellm-key` (with a `max_budget_usd`) through the normal
governed `request_resource` flow → human review (it's a spend-authorizing
credential, and Asgard isn't in the call path) → Asgard mints a project-tagged
virtual key on the proxy, stores the key value in the secret store, and registers a
`litellm` cost source that reads each key's spend back via `/key/info`. Without
those env vars the service's connector falls back to `stub` and no spend is pulled.

> **This direct path requires LiteLLM to have its own database.** Per-project key
> minting (`/key/generate`), budgets, and spend read-back (`/key/info`) are
> DB-backed features of the LiteLLM proxy — LiteLLM must be configured with its own
> `DATABASE_URL` (its own Postgres, separate from Asgard's). The gated `litellm`
> module above does **not** need this; only `litellm-key` does, because here the
> key lives on the proxy, not in Asgard.

## Databricks Model Serving

The shipped **`databricks` module** is exactly the same shape as LiteLLM — a plug-in
manifest, not core code. It's `kind: openai-compatible` with one extra knob,
`chat_path`, because Databricks queries a served model at
`{host}/serving-endpoints/{endpoint-name}/invocations` (the endpoint is in the path,
not OpenAI's `/v1/chat/completions`):

```
DATABRICKS_HOST=https://dbc-xxxx.cloud.databricks.com
DATABRICKS_TOKEN=dapi...
```

The module activates on those two env vars (the standard Databricks ones, shared
with the Terraform provider and cost source); edit its `models[]` so each `route` is a
serving endpoint name. Asgard's gateway then sits **in front of** Databricks Model
Serving (and Databricks' own Mosaic AI Gateway) — same tokens, budgets, policy,
guardrails, audit, per-project cost. See [Databricks](./databricks.md) for the full
picture (provisioning + cost too).

## Other backends

vLLM, Azure OpenAI, or any **OpenAI-compatible** endpoint use the same shape — set
`base_url` + `api_key_env` on a module, plus `chat_path` if its request path isn't
`/v1/chat/completions`. Anthropic-native uses the `anthropic` kind. Adding a
brand-new backend is dropping a `services/<id>/service.yaml` with an `inference`
block; no recompile.

## What the operator never has to do
- Write a LiteLLM service definition (it's a standard module).
- Bundle LiteLLM into Asgard's image (deploy it separately, point at it).
- Expose any of this to end users (they see a capability, not a backend).
