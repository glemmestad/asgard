# Databricks Model Serving (inference)

Routes governed model calls through the gateway to **Databricks Model
Serving / Foundation Model APIs**. This is a plug-in module — the same shape as
LiteLLM — not core code: it's `kind: openai-compatible` with a request-path
override.

**The control plane sits in front of Databricks.** A project gets a virtual key,
never the Databricks token. Every call is budget-checked, policy- and
data-class-gated, guardrailed, kill-switchable, audited, and cost-attributed per
project — including in front of Databricks' own Mosaic AI Gateway.

## Enable

Set in the control plane's process env (e.g. `.env`):

```
DATABRICKS_HOST=https://dbc-xxxx.cloud.databricks.com
DATABRICKS_TOKEN=dapi...
```

The module activates automatically when both are present. List your serving
endpoints (`databricks serving-endpoints list`) and edit `models[]` so each
`route` is a serving endpoint name and `cost_in`/`cost_out` match your pricing.

## How it routes

Databricks queries a served model at
`{host}/serving-endpoints/{endpoint-name}/invocations` (the endpoint is in the
path, not the body), with an OpenAI-shaped request/response. The manifest's
`chat_path: /serving-endpoints/{model}/invocations` expresses exactly that — the
`{model}` placeholder is filled with the route model. No Databricks-specific code
lives in the control plane.

## Use (out-of-band, like any service)

Inference is **service usage**, not an MCP control-plane call. Call the gateway
with the project's LLM key:

```sh
curl -sS https://<frontkeep-host>/api/gateway/chat \
  -H "Authorization: Bearer <project-llm-key>" \
  -H 'content-type: application/json' \
  -d '{"model":"model:databricks/llama-3-3-70b","messages":[{"role":"user","content":"hi"}],"data_class":"internal"}'
```
