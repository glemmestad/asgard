---
sidebar_position: 6.2
title: Migrate an app onto Frontkeep
---

# Agent runbook: stand a collaborative app on Frontkeep

This is a machine-oriented, idempotent runbook for an agent told *"move our app
onto Frontkeep."* The worked example is a real-time collaborative editor: a Node
service behind a load balancer, a Postgres metadata store, an S3 bucket of
collaborative-document blobs, an HMAC key for signing collab tokens, and an
Auth0 SPA + M2M pair for login. It is **standalone** — it used the predecessor
platform only to *provision* infrastructure, so there is no runtime API to
replace. This runbook reprovisions that footprint through Frontkeep's primitives and
repoints the app at the new resources. Substitute your own app's footprint where
it differs.

Execute the steps in order. After each, run the verification and **do not proceed
until it passes**. Everything here is provisioned through Frontkeep's MCP tools
(`request_resource`, `get_secret`, `deprovision_resource`) against a registered
project — the same governed path any agent uses.

:::warning Data move is human-gated
Steps 1–7 stand up **empty** infrastructure and a placeholder image. The live
database snapshot and S3 copy (Step 9) move real user data and **must be run with
a human**, during a maintenance window, after the new stack is proven. Do not
touch the live app's data unattended.
:::

## Why this is now possible

The predecessor platform got this app running only after a string of manual
workarounds. Frontkeep's primitives close each one — this runbook is the proof:

| Predecessor gap | Frontkeep's close |
|---|---|
| Declared `grants:` silently dropped → runtime `AccessDenied` | `ecs-service` builds the task-role inline policy *from* `grants`; a declared grant is an effective grant |
| Task role missing `kms:Decrypt` for the secret-wrapping key | `grants.kms_decrypt` is a first-class field; a secret grant without it is inert, so it ships together |
| Secret ref keyed `connection_secret_arn`, consumers looked for `secret_arn` | `rds-postgres` emits a real Secrets Manager secret as **`secret_arn`**, full stop |
| ALB HTTP-only → `auth0-spa-js` refuses the non-secure origin, login impossible | `ecs-service` `certificate_arn` adds a 443 listener with 80→443 redirect |
| No `url` output → reconstruct ALB DNS by hand | `ecs-service` emits **`url`** |
| ECR repo fully immutable, `:latest` re-push fails | push content-addressed `:sha-<gitsha>` tags (below); never rely on `:latest` |
| No way to update Auth0 callbacks once the ALB URL exists | two-phase: create the SPA, deploy to learn `url`, then re-apply the SPA with `url` in its callbacks (Step 8) |

## Inputs to obtain first

Stop and ask a human for any you cannot derive.

- A **registered, active** Frontkeep project for the app and a project key (Step 0).
- `VPC_ID`, `SUBNET_IDS` (≥2 AZs) — the existing network the app runs in. Frontkeep
  never creates a VPC.
- `CERT_ARN` — an ACM certificate for the app's hostname (required for the Auth0
  SPA; login cannot work over plain HTTP).
- `AUTH0_TENANT` — the Auth0 tenant the SPA + M2M apps live in, and M2M
  Management-API creds set on the Frontkeep process (see the
  [deploy runbook](./deploy-agent.md) Step 8).
- The app's container image, built and pushed to the ECR repo from Step 2.

## Step 0 — Register the project (the gate)

Registration gates everything downstream — no key, no provisioning.

```bash
PID=$(curl -fsS -X POST "$FRONTKEEP/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Collab Editor","owner_email":"owner@corp.example","manager_email":"mgr@corp.example","group":"applications","classification":"poc","budget_usd":200}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["project_id"])')
KEY=$(curl -fsS -X POST "$FRONTKEEP/api/projects/$PID/keys" -H 'content-type: application/json' \
  -d '{"name":"migrate-agent"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["key"])')
```

**Verify:** `PID` matches `^proj-[0-9]{4}-[0-9]{4}$`; `KEY` is non-empty. Use
`KEY` as the bearer token on `/mcp` for every `request_resource` below; resources
are tagged `project=$PID` so their cost rolls up to the project automatically.

> The runbook shows the values as `request_resource` arguments. Each is one MCP
> `tools/call` to `request_resource` with `{"service":"<id>","name":"<n>","spec":{…}}`.
> Cost-bearing services (`rds-postgres`, `ecs-service`) are **not** auto-approved;
> they enter `pending` and a manager approves via the dashboard or
> `POST /api/requests/{id}/approve`.

## Step 1 — Object storage (Yjs blobs)

```json
request_resource s3-bucket { "name": "storage" }
```
**Verify:** the resource reaches `fulfilled`; record `outputs.arn` as `S3_ARN`
and `outputs.bucket` as `S3_BUCKET`.

## Step 2 — Image repository

```json
request_resource ecr-repository { "name": "app", "spec": { "name": "app", "immutable": true } }
```
**Verify:** record `outputs.uri` as `ECR_URI`. Then build and push the app
**by content**, never `:latest`:
```bash
docker build -t "$ECR_URI:sha-$(git rev-parse --short HEAD)" .
aws ecr get-login-password | docker login --username AWS --password-stdin "${ECR_URI%%/*}"
docker push "$ECR_URI:sha-$(git rev-parse --short HEAD)"
```
Record the exact pushed ref as `IMAGE`. (Content tags sidestep the predecessor's
immutable-`:latest` dead end entirely; `immutable: true` enforces it at the repo.)

> **No AWS credentials on the runner?** Broker a short-lived login instead of
> `aws ecr get-login-password` — the control plane mints it with its own role:
> ```bash
> request_resource ecr-credential { "name": "push", "spec": { "name": "push" } }
> # registry from the record's outputs.registry; password from the secret store:
> echo "$(get_secret push-password)" | docker login -u AWS --password-stdin "$REGISTRY"
> ```
> See the [ecr-credential service](../../services/ecr-credential/README.md) for the
> operator IAM prerequisite.

## Step 3 — Metadata database

```json
request_resource rds-postgres { "name": "metadata", "engine_version": "16", "subnet_group_name": "<db-subnet-group>", "vpc_security_group_ids": ["<db-sg>"] }
```
This is approval-gated. **Verify:** record `outputs.secret_arn` as `DB_SECRET_ARN`
(a real Secrets Manager secret holding `{host,port,dbname,username,password,url}`).
`connection_url` and `master_password` are routed to the secret store as
`secret_ref`s — fetch with `get_secret` only when needed; they never appear in the
record.

## Step 4 — Collab-token HMAC key

```json
request_resource secretsmanager-secret { "name": "collab-token-key", "byte_length": 64 }
```
**Verify:** record `outputs.secret_arn` as `HMAC_SECRET_ARN`. The 128-hex-char
value lands in Secrets Manager so the task role can read it at runtime.

## Step 5 — Auth0 SPA + M2M

```json
request_resource auth0-application { "name": "spa", "app_type": "spa" }
request_resource auth0-application { "name": "api", "app_type": "non_interactive" }
```
**Verify:** record each `outputs.client_id`; the `client_secret`s are stored as
`secret_ref`s. Leave callbacks unset for now — the ALB URL does not exist yet
(closed in Step 8).

> **Enterprise SSO + a dedicated audience.** If the app authenticates against a
> resource-server API and logs in through an existing tenant connection, the
> module exposes two optional inputs (no-ops when unset, so OSS deploys are
> unchanged):
>
> - `enabled_connections: ["my-sso-connection"]` enables an existing tenant
>   connection on the client (the connection must already exist in the tenant).
> - `resource_server_template: "https://api-{project}.example.com/"` creates a
>   project-dedicated API; `{project}` is substituted with the project id and the
>   resulting identifier is emitted as `outputs.audience` for the app's
>   `AUTH0_AUDIENCE`.
>
> An operator that wants these on *every* app sets them as `config.defaults` in
> an `FRONTKEEP_SERVICES_DIR` overlay (env-sourced via `${VAR}` / `${VAR:csv}`)
> rather than per request — the tenant-specific values stay in the deployment's
> config, never in the catalog.

## Step 6 — Determine the KMS key

The secrets above are wrapped by a KMS key (the account default `aws/secretsmanager`
or a CMK). Record its ARN as `KMS_ARN`. The task role must decrypt it or every
secret read fails silently — this is the gap that bit the predecessor.

```bash
aws secretsmanager describe-secret --secret-id "$DB_SECRET_ARN" --query KmsKeyId --output text
```
(If it returns `None`, the AWS-managed `aws/secretsmanager` key is in use; grant
`kms:Decrypt` on its ARN, resolvable via `aws kms describe-key --key-id alias/aws/secretsmanager`.)

## Step 7 — The service (keystone)

One request stands up the cluster, task definition, task role (from `grants`),
ALB, target group, HTTPS listener, and logs.

```json
request_resource ecs-service {
  "name": "app",
  "image": "<IMAGE>",
  "vpc_id": "<VPC_ID>",
  "subnet_ids": ["<subnet-a>", "<subnet-b>"],
  "cpu": "512", "memory": "1024",
  "container_port": 3000,
  "health_path": "/healthz",
  "certificate_arn": "<CERT_ARN>",
  "env": {
    "NODE_ENV": "production",
    "S3_BUCKET": "<S3_BUCKET>",
    "DATABASE_URL_SECRET_ARN": "<DB_SECRET_ARN>",
    "COLLAB_TOKEN_KEY_SECRET_ARN": "<HMAC_SECRET_ARN>",
    "AUTH_ENABLED": "true",
    "AUTH0_DOMAIN": "<AUTH0_TENANT>",
    "AUTH0_CLIENT_ID": "<spa client_id from Step 5>"
  },
  "grants": {
    "s3_write": ["<S3_ARN>"],
    "secrets_read": ["<DB_SECRET_ARN>", "<HMAC_SECRET_ARN>"],
    "kms_decrypt": ["<KMS_ARN>"]
  }
}
```
The app reads its secrets by ARN at runtime (it has the AWS SDK and a task role),
matching its existing pattern — so the secrets go in as plain `env` ARNs plus
`grants.secrets_read`, not as injected `secrets:`. (To inject a secret value
directly as an env var instead, use the `secrets` field: `{"DB":"<arn>"}` — the
execution role is then auto-granted read.)

This is approval-gated. **Verify:**
- the request reaches `fulfilled`; record `outputs.url` as `URL` and
  `outputs.task_role_arn`.
- `aws iam get-role-policy --role-name <project>-app-task --policy-name <project>-app-task`
  shows the S3, secrets, and KMS statements — i.e. `grants` were honored.
- the ECS service reaches a healthy target:
  `aws elbv2 describe-target-health --target-group-arn <tg>` → `healthy`.
- `curl -fsS "$URL/healthz"` returns 200 over **https**.

## Step 8 — Repoint Auth0 callbacks

Now that `URL` exists, re-apply the SPA app with its real callbacks (the gap the
predecessor could only fix by hand-editing the Auth0 dashboard):

```json
request_resource auth0-application {
  "name": "spa", "app_type": "spa",
  "callbacks": ["<URL>"], "allowed_logout_urls": ["<URL>"], "web_origins": ["<URL>"]
}
```
**Verify:** load `URL` in a browser; `auth0-spa-js` initializes (no
"must run on a secure origin" error, because Step 7 gave it https) and a login
round-trip returns to the app authenticated.

> If the `auth0-application` module in your deployment does not yet expose
> `callbacks`/`web_origins` inputs, add them to `modules/auth0/application` (they
> map directly to `auth0_client` arguments) — this is the one module input the
> migration adds.

## Step 9 — Data move `[HUMAN-GATED]`

Run with a human, in a maintenance window, after Steps 1–8 are green:

1. Quiesce writes on the live app (scale the old service to 0 or enable read-only).
2. **Database:** `aws rds create-db-snapshot` on the source, then restore into the
   new instance — or `pg_dump | pg_restore` against the new `connection_url`
   (`get_secret`). Verify row counts match.
3. **S3:** `aws s3 sync s3://<old-bucket> s3://<S3_BUCKET>` (Yjs `.bin`, snapshots,
   versions). Verify object counts and a checksum sample.
4. Flip DNS / the Auth0 production callbacks to `URL`; smoke a real collab session
   (two clients, live cursor + document convergence).
5. Decommission the old stack only after a soak period.

## Step 10 — Ship a new image (CD)

On every merge, build + push a new `:sha` (Step 2 — broker the login if the runner
has no AWS creds), then roll the running service to it in one call:
```json
deploy_image { "resource_id": "<ecs-service id>", "image": "<ECR_URI>:sha-<new>" }
```
`deploy_image` swaps **only** the image in the service's spec — `env`, `secrets`,
`grants`, and `certificate_arn` are preserved — and re-applies in place: ECS
registers a new task-definition revision and rolls with circuit-breaker rollback.
It stays self-service (no per-deploy approval) and returns the `provisioning`
record; poll `get_resource` until `provisioned`. This is the whole CD loop —
runner builds + pushes, Frontkeep cycles ECS, no AWS credentials on the runner.

## Done criteria

- Every resource above is `fulfilled` and tagged `project=$PID` (cost rolls up).
- `curl https://$URL/healthz` → 200; ECS target `healthy`.
- Task-role policy contains the `grants` (S3 + secrets + KMS) — verified, not assumed.
- Browser login round-trips through Auth0 over https.
- Report back: `PID`, `URL`, the resource ids, and that the data move (Step 9)
  remains pending a human window.
```
