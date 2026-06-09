# Recipe: add real-time collaboration to your app

You built an app and now you want multiple people editing the same thing at once —
live cursors, presence, no "your changes conflict with theirs" dialogs. This is
the runbook to take you from that wish to a working, governed deployment on
the control plane. Follow it top to bottom; every step is an MCP call you make
yourself. The control plane is a hub, not a magic "collab service" — you bring the server,
the control plane provisions and governs the infrastructure and hands you a URL.

## What "working" looks like

When you finish: two browser tabs open the same document, you type in one, the
other updates within ~100 ms, and a refresh keeps the content because state is
persisted. Login works (over HTTPS), and every resource you created is attributed
to your project on the cost dashboard.

## Is this the right recipe?

**Use it when** the canonical state is a shared, concurrently-edited document — a
collaborative editor, a design canvas, a shared notebook, anything with cursors
and presence. The standard pattern is a CRDT (Yjs is the common choice) synced
over WebSockets.

**Don't use it for** plain request/response apps (use the `ecs-service` primitive
directly), a simple chat room (you don't need CRDT machinery), or multi-region
active-active writes (a different, harder problem).

## What you're actually building

A small **collaboration server** (your code, your image) that:
- speaks the CRDT sync protocol over WebSocket to browser clients,
- persists document state to **S3** (binary CRDT updates + periodic snapshots),
- stores document metadata in **Postgres**,
- signs short-lived WebSocket access tokens with an **HMAC key**,
- verifies user logins against an **Auth0** SPA app.

The control plane provisions the S3 bucket, the database, the secret, the Auth0 app, and the
load-balanced HTTPS service — and wires their outputs into your server's
environment. **You do not write any Terraform or touch IAM.** You write the
server (or fork an existing CRDT server like Hocuspocus) and a thin frontend
binding.

The server image must honor this env contract (the recipe wires every variable):

| Env var | Carries |
| --- | --- |
| `DOCS_BUCKET` | S3 bucket name for CRDT state + snapshots |
| `DATABASE_URL_SECRET_ARN` | Secrets Manager ARN of the DB connection JSON |
| `TOKEN_KEY_SECRET_ARN` | Secrets Manager ARN of the HMAC key for WS tokens |
| `AUTH0_CLIENT_ID`, `AUTH0_DOMAIN` | SPA app credentials for login |
| `ALLOWED_ORIGINS` | CORS allow-list (empty = same-origin) |

Listen on port **8080** and expose **`/healthz`** (returns 200 when ready) — the
load balancer's health check depends on it.

## The sequence

Everything below is `request_resource` over MCP against your registered project.
Cost-bearing steps (the database, the service) gate to your manager for approval;
the rest self-serve. Record each step's outputs — later steps consume them.

**0. Register your project** (the gate — nothing provisions without it).
`register_project` → note your `proj-…` id and mint a key.

**1. Image repository.** `request_resource ecr-repository { "name": "collab", "spec": { "name": "collab", "immutable": true } }`
→ record `uri`. Build your server image and push an **immutable** tag — never
`:latest`:
```
docker build -t <uri>:sha-$(git rev-parse --short HEAD) .
docker push <uri>:sha-$(git rev-parse --short HEAD)
```
No AWS credentials on your build box? Broker the registry login instead of
`aws ecr get-login-password` — the control plane mints it with its own role:
```
request_resource ecr-credential { "name": "push", "spec": { "name": "push" } }
echo "$(get_secret push-password)" | docker login -u AWS --password-stdin <registry from outputs.registry>
```
Use that exact tag as `image` in step 6.

**2. Metadata database.** `request_resource rds-postgres { "name": "collab-db", "engine_version": "16" }`
→ record `secret_arn` (a real Secrets Manager secret holding the connection
JSON). Approval-gated.

**3. Document storage.** `request_resource s3-bucket { "name": "collab-docs" }` →
record `arn` and `bucket`.

**4. Token-signing key.** `request_resource secretsmanager-secret { "name": "collab-token-key", "byte_length": 64 }`
→ record `secret_arn`. This is the HMAC key your server uses to sign WS tokens.

**5. Login app.** `request_resource auth0-application { "name": "collab-spa", "app_type": "spa" }`
→ record `client_id`. Leave callbacks empty for now — the URL doesn't exist yet
(step 7 fixes this).

**6. The service** (keystone). One `ecs-service` request builds the cluster, task
role, ALB, HTTPS listener, and logs:
```json
request_resource ecs-service {
  "name": "collab",
  "image": "<uri>:sha-…",
  "vpc_id": "<your vpc>", "subnet_ids": ["<a>", "<b>"],
  "certificate_arn": "<ACM cert>",          // HTTPS is mandatory — see gotchas
  "container_port": 8080, "health_path": "/healthz",
  "cpu": "512", "memory": "1024",
  "env": {
    "DOCS_BUCKET": "<from step 3>",
    "DATABASE_URL_SECRET_ARN": "<from step 2>",
    "TOKEN_KEY_SECRET_ARN": "<from step 4>",
    "AUTH0_CLIENT_ID": "<from step 5>",
    "AUTH0_DOMAIN": "<your tenant>"
  },
  "grants": {
    "s3_write": ["<bucket arn from step 3>"],
    "secrets_read": ["<step 2 arn>", "<step 4 arn>"],
    "kms_decrypt": ["<the KMS key wrapping those secrets>"]
  }
}
```
Record `url`. Approval-gated.

**7. Close the Auth0 loop.** Now that you have `url`, re-apply the SPA app with its
callbacks so login works:
`request_resource auth0-application { "name": "collab-spa", "app_type": "spa", "callbacks": ["<url>"], "web_origins": ["<url>"], "allowed_logout_urls": ["<url>"] }`

**Ship updates (CD).** On every change, build + push a new `:sha` (step 1), then
roll the running service to it in one call — no need to re-send the whole spec:
```json
deploy_image { "resource_id": "<ecs-service id>", "image": "<uri>:sha-…" }
```
It swaps only the image (env, grants, and the cert are preserved), re-applies in
place, and ECS rolls the new task-definition revision with automatic rollback.
Self-service — no approval per deploy.

## Verify it's working

1. `curl -fsS <url>/healthz` returns 200 over **https**.
2. The ECS target is healthy (`aws elbv2 describe-target-health …`).
3. Point your frontend's CRDT provider at `wss://<host>` with a token from your
   server's token endpoint. Open the same doc in two tabs — edits converge live.
4. Confirm the task role only has what you granted:
   `aws iam get-role-policy --role-name <proj>-collab-task …`.

## Gotchas (the ones that cost an afternoon)

- **HTTPS is non-negotiable for the SPA.** `auth0-spa-js` (and every modern auth
  SDK) refuses any non-`localhost` HTTP origin and silently never initializes —
  the login button does nothing. The `certificate_arn` in step 6 is what makes
  login work at all.
- **`kms_decrypt` or your secret grant is inert.** Reading a Secrets Manager
  secret fails at runtime without decrypt on the wrapping key, even though the
  policy looks right. Grant both together (step 6).
- **Never `:latest`.** Content-addressed tags only; the repo is immutable.
- **Moving real data is a separate, human-gated step.** This recipe stands up an
  *empty* stack. Migrating an existing app's documents (DB snapshot + S3 sync)
  happens in a maintenance window with a human — never as part of the build.

## Why you bring your own image

The catalog is infrastructure primitives, not application code. A shared
"collab server" image would make the platform own someone else's runtime — its
Dockerfile, CVEs, version bumps — forever, for one recipe. Keeping the server in
your project is what lets the platform stay a thin, auditable hub. Fork a CRDT server,
honor the env contract above, push it, and the rest is this runbook.

## Cost (illustrative, POC)

The 24/7 Fargate task dominates (~$15–25/mo at 0.5 vCPU / 1 GiB); the ALB is
~$16/mo; S3, Postgres-micro, and Secrets Manager are a few dollars combined.
Everything is tagged to your project and shows on the Cost dashboard. Higher
tiers (more replicas, bigger tasks) scale roughly linearly.

## See also

- Guidance: *Wire Auth0 into a single-page app*, *Handling Secrets*, *Picking a Classification*.
- Primitives: `ecr-repository`, `rds-postgres`, `s3-bucket`, `secretsmanager-secret`, `auth0-application`, `ecs-service`.
