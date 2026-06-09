# Recipe: stand up an authenticated MCP server

You wrote some tools you want an AI agent to call — over the network, by more than
one person, with auth on every request. This runbook takes you from "I have tool
code" to "agents hit `https://…/mcp` with a Bearer token and it works," using
the platform's primitives. You bring the server image; the platform provisions and governs the
repo, the auth app, and the HTTPS service.

## What "working" looks like

An agent points its MCP client at `https://<your-host>/mcp` with an
`Authorization: Bearer <token>`, the `initialize` handshake negotiates, and
`tools/list` returns your tools. Unauthenticated calls are rejected. The whole
thing is one ECS service, attributed to your project.

## Is this the right recipe?

**Use it** for any MCP server that needs to be reachable beyond one laptop:
shared internal tooling, a server exposing your project's data to agents,
anything an agent runtime calls programmatically over HTTP.

**Don't use it** for a stdio MCP server you run locally (no infra needed), or a
tool you only ever call from your own machine.

## What you're actually building

An **MCP server** (your code, your image) that:
- implements the MCP protocol over **streamable HTTP**,
- validates an Auth0-issued **JWT** on each request,
- exposes your tools.

The control plane provisions the image repository, the Auth0 M2M app agents use to get
tokens, and the load-balanced HTTPS service. The server image must honor:

| Env var | Carries |
| --- | --- |
| `AUTH0_CLIENT_ID`, `AUTH0_DOMAIN` | M2M app credentials for verifying Bearer JWTs |
| `AUTH0_AUDIENCE` | the API audience your server checks tokens against |

Listen on port **8080**, serve MCP at **`/mcp`**, and expose **`/healthz`**.

## The sequence

Each step is a `request_resource` call against your registered project. Record
outputs; later steps consume them.

**0. Register your project** (`register_project`) and mint a key.

**1. Image repository.** `request_resource ecr-repository { "name": "<name>", "spec": { "name": "<name>", "immutable": true } }`
→ record `uri`. Build your MCP server image and push an immutable tag:
```
docker build -t <uri>:sha-$(git rev-parse --short HEAD) .
docker push <uri>:sha-$(git rev-parse --short HEAD)
```
No AWS credentials on the build box? `request_resource ecr-credential { "name": "push", "spec": { "name": "push" } }`,
then `echo "$(get_secret push-password)" | docker login -u AWS --password-stdin <outputs.registry>`.

**2. Auth app.** `request_resource auth0-application { "name": "<name>", "app_type": "non_interactive" }`
→ record `client_id`. This M2M app is what agents use to obtain Bearer tokens;
the `client_secret` is stored as a `secret_ref`.

**3. The service** (keystone):
```json
request_resource ecs-service {
  "name": "<name>",
  "image": "<uri>:sha-…",
  "vpc_id": "<your vpc>", "subnet_ids": ["<a>", "<b>"],
  "certificate_arn": "<ACM cert>",
  "container_port": 8080, "health_path": "/healthz",
  "cpu": "256", "memory": "512",
  "env": { "AUTH0_CLIENT_ID": "<from step 2>", "AUTH0_DOMAIN": "<your tenant>", "AUTH0_AUDIENCE": "<your api audience>" }
}
```
Record `url`. MCP is reachable at `<url>/mcp`. Approval-gated.

**Ship updates.** Push a new `:sha` (step 1), then
`deploy_image { "resource_id": "<ecs-service id>", "image": "<uri>:sha-…" }` — it
swaps only the image and rolls the service in place (env/cert preserved), no
approval per deploy.

## Verify it's working

1. `curl -fsS <url>/healthz` → 200 over https.
2. Unauthenticated MCP `initialize` is rejected (401).
3. With a Bearer token from the M2M app, `initialize` negotiates and `tools/list`
   returns your tools. (MCP Inspector pointed at `<url>/mcp` with the header is
   the quickest check.)

## Gotchas

- **HTTPS, always** — `certificate_arn` in step 3. Agent runtimes and token flows
  assume TLS.
- **Immutable tags** — `:sha-…`, never `:latest`.
- **256 vCPU is usually plenty** — MCP servers are I/O-bound; start small and
  scale `cpu`/`memory` only if you measure a need.

## Why you bring your own image

Same principle as every recipe: the platform provisions infrastructure, not your tool
code. You own the server (and its CVEs, its versioning); the platform owns the governed
repo + auth + HTTPS service it runs in. That separation is what keeps the hub
thin and auditable.

## Cost (illustrative, POC)

A 0.25 vCPU Fargate task (~$8–12/mo 24/7) plus the ALB (~$16/mo); Auth0 + ECR are
negligible. Tagged to your project on the Cost dashboard.

## See also

- Guidance: *Handling Secrets*, *Picking a Classification*.
- Primitives: `ecr-repository`, `auth0-application`, `ecs-service`.
- To connect a client once it's up, see the operator/connect docs.
