---
sidebar_position: 6.1
title: Deploy (agent runbook)
---

# Agent runbook: deploy Asgard

This is a machine-oriented, idempotent runbook for an agent told *"deploy Asgard
in our environment."* Execute the steps in order. After each step, run the
verification and **do not proceed until it passes**. Conditionals marked
`[ENTERPRISE]` apply only when single sign-on and/or live provisioning are
required; skip them for a POC.

The prose rationale for every choice is in the [operator guide](./deploy.md);
this file is the checklist.

## Inputs to obtain first

Collect these before starting. Stop and ask a human for any you cannot derive.

- `HOST` — the public hostname Asgard will be reached at (behind your TLS ingress).
- `ASGARD_SECRET_KEY` — 64 hex chars (32 bytes), from the org KMS. Generate with
  `openssl rand -hex 32` only for a throwaway trial.
- `DB` — `postgres://USER:PASS@HOST:5432/asgard` for a pilot; `sqlite:///data/asgard.db` for a box trial.
- `ASGARD_ADMIN_PASSWORD` — optional; if omitted, capture the generated one from the boot log.
- `ASGARD_SYSTEM_NAME` — optional; UI display name to rebrand the dashboard to (e.g. `Acme Control Plane`). Cosmetic only; defaults to `Asgard`.
- `[ENTERPRISE]` OIDC web-app: `ASGARD_OIDC_DOMAIN`, `ASGARD_OIDC_CLIENT_ID`, `ASGARD_OIDC_CLIENT_SECRET`, and the callback URL `https://HOST/api/auth/oidc/callback` registered in the IdP.
- `[ENTERPRISE]` Auth0 M2M for provisioning: `AUTH0_DOMAIN`, `AUTH0_CLIENT_ID`, `AUTH0_CLIENT_SECRET` (authorized for the Management API).

## Step 1 — Database

POC (SQLite): nothing to do; the binary creates and migrates it.

Pilot (Postgres):
```bash
docker run -d --name asgard-pg \
  -e POSTGRES_PASSWORD="$PGPASS" -e POSTGRES_DB=asgard \
  -p 5432:5432 -v asgard-pg:/var/lib/postgresql/data postgres:16-alpine
```
**Verify:** `docker exec asgard-pg pg_isready -U postgres` prints `accepting connections`.

## Step 2 — Config file

Write `asgard.yaml` with at least the group allowlist:
```yaml
groups:
  - { key: platform, display_name: Platform, cost_center: CC-100 }
```
**Verify:** `test -f asgard.yaml`. (Governance-model tuning — promotion evidence
requirements, review windows, the `maintainer_min` metric threshold — is optional
with policy-doc defaults; see the operator guide's `asgard.yaml` reference.)

## Step 3 — Boot (single replica)

```bash
ASGARD_DATABASE_URL="$DB" \
ASGARD_SECRET_KEY="$ASGARD_SECRET_KEY" \
ASGARD_ADMIN_PASSWORD="${ASGARD_ADMIN_PASSWORD:-}" \
asgard serve --bind 0.0.0.0:8080 --config ./asgard.yaml
```
Run exactly **one** replica (the background rollup/rotation loops assume a single writer).

**Verify:** `curl -fsS http://localhost:8080/healthz` returns `ok` (liveness) and
`curl -fsS http://localhost:8080/readyz` returns `ready` (DB reachable). If
`ASGARD_ADMIN_PASSWORD` was empty, grep the log for `password:` and record the
generated admin credentials.

## Step 4 — Confirm enforcement

**Verify (POC):**
```bash
test "$(curl -s -o /dev/null -w '%{http_code}' http://localhost:8080/api/projects)" = 401
```
A human/admin route must return `401` without a session. If it returns `200`, the
dev escape hatch is on — ensure `ASGARD_DEV_INSECURE` is unset for a deployment.

## Step 5 — Onboard a project and mint a key

```bash
PID=$(curl -fsS -X POST http://localhost:8080/api/projects \
  -H 'content-type: application/json' \
  -d '{"name":"Pilot","owner_email":"owner@corp.example","manager_email":"mgr@corp.example","group":"platform","budget_usd":100}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["project_id"])')
KEY=$(curl -fsS -X POST http://localhost:8080/api/projects/$PID/keys \
  -H 'content-type: application/json' -d '{"name":"agent"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["key"])')
```
(With enforcement on, prepend a session: log in at `/api/auth/login` and pass the
token as `Authorization: Bearer`, or run these from the dashboard.)

**Verify:** `PID` matches `^proj-[0-9]{4}-[0-9]{4}$` and `KEY` is non-empty.

## Step 6 — Verify the MCP endpoint

```bash
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"runbook","version":"0"}}}'
ACC='accept: application/json, text/event-stream'
# Unauthenticated must be rejected:
test "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://localhost:8080/mcp -H 'content-type: application/json' -H "$ACC" -d "$INIT")" = 401
# With the key it must negotiate:
curl -s -X POST http://localhost:8080/mcp -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -H "$ACC" -d "$INIT" | grep -q '"name":"asgard"'
```
**Verify:** the unauthenticated call is `401` and the authenticated call contains
`"name":"asgard"`.

## Step 7 — `[ENTERPRISE]` Enable OIDC

Restart the process with the OIDC env set:
```bash
ASGARD_OIDC_DOMAIN=... ASGARD_OIDC_CLIENT_ID=... ASGARD_OIDC_CLIENT_SECRET=... \
ASGARD_OIDC_REDIRECT_URI=https://$HOST/api/auth/oidc/callback \
asgard serve --bind 0.0.0.0:8080 --config ./asgard.yaml
```
**Verify:** `curl -s https://$HOST/api/auth/config` returns `"oidc":true`, and a
browser round-trip `GET /api/auth/oidc/login` → IdP → callback lands on `/` with a
session (`GET /api/auth/me` then returns the user). The local admin still works as
break-glass.

## Step 8 — `[ENTERPRISE]` Arm provisioning

Container-first (no config file) — set on the Asgard process (the image bundles
`terraform` on `PATH` and the modules at `/modules`):
```bash
ASGARD_TF_MODULES_DIR=/modules
ASGARD_TF_WORK_DIR=/data/asgard-tf      # scratch only; can be ephemeral
ASGARD_TF_ALLOWED=auth0:your-tenant
AUTH0_DOMAIN=... AUTH0_CLIENT_ID=... AUTH0_CLIENT_SECRET=...   # M2M provider creds
```
Or via `asgard.yaml` when you want the other provisioning knobs in one place:
```yaml
provisioning:
  terraform: { modules_dir: /modules, work_dir: /data/asgard-tf }
  allowed:
    - { cloud: auth0, account: your-tenant }
```
Either way the M2M creds go **on the Asgard process** (the Terraform child inherits
them). **Terraform state is persisted (encrypted) in Asgard's database** and
hydrated back per run, so `ASGARD_TF_WORK_DIR` is just scratch and may be ephemeral
— back up the DB and you've backed up provisioning state too.
**Verify:** request `auth0-application` over `/mcp` (`request_resource`) for a
registered project; the request reaches `fulfilled` and the created app's
`client_secret` is stored as a `secret_ref` (fetch it via the `get_secret` tool;
confirm it never appears in the resource record or audit log).

## Done criteria

- `healthz` is `ok`; one replica running.
- Unauthenticated human/admin route → `401`; admin can sign in.
- `/mcp` rejects no-key (`401`) and negotiates with a project key; `tools/list`
  shows the catalog including `seed_plan`.
- `[ENTERPRISE]` SSO round-trips; armed provisioning creates a real resource with
  its secret stored as a `secret_ref`.
- Report back: the host URL, the admin credential location, the onboarded project
  id, and which path (POC vs enterprise) was deployed.

## Appendix — `[DOGFOOD]` Self-deploy Asgard on ECS

Asgard provisions itself: a **local** Asgard (Steps 1–8) stands up a **production**
Asgard on ECS through its own `ecs-service` primitive — the same governed path any
app uses. This is the smallest possible proof that the control plane can deploy a
real load-balanced service. Inputs: `VPC_ID`, `SUBNET_IDS` (≥2 AZs), `CERT_ARN`
(ACM cert for the Asgard hostname), and a Postgres for the production instance.

1. **Onboard Asgard as a project** on the local instance and mint a key (Step 5).
   Use group `platform`, classification `light-operational`.

2. **Image → ECR.** Request the repo, then build and push by content:
   ```json
   request_resource ecr-repository { "name": "asgard" }
   ```
   ```bash
   docker build -t "$ECR_URI:sha-$(git rev-parse --short HEAD)" .
   aws ecr get-login-password | docker login --username AWS --password-stdin "${ECR_URI%%/*}"
   docker push "$ECR_URI:sha-$(git rev-parse --short HEAD)"   # IMAGE
   ```
   (The official image already bundles `terraform` + `/modules`, so the ECS Asgard
   can provision with no mounts.)

3. **Database + secret key.** Provision (or point at) Postgres and create the
   32-byte signing key in Secrets Manager so the task can read it:
   ```json
   request_resource rds-postgres { "name": "asgard-db", "engine_version": "16" }
   request_resource secretsmanager-secret { "name": "asgard-key", "byte_length": 32 }
   ```
   Record `DB_SECRET_ARN`, `KEY_SECRET_ARN`, and the wrapping `KMS_ARN` (Step 6 of
   the [app migration runbook](./migrate-app.md)).

4. **Deploy the service.** One `ecs-service` request runs Asgard behind HTTPS:
   ```json
   request_resource ecs-service {
     "name": "asgard",
     "image": "<IMAGE>",
     "vpc_id": "<VPC_ID>", "subnet_ids": ["<a>", "<b>"],
     "cpu": "512", "memory": "1024",
     "container_port": 8080,
     "health_path": "/readyz",
     "certificate_arn": "<CERT_ARN>",
     "idle_timeout": 900,
     "desired_count": 1,
     "env": { "ASGARD_DATABASE_URL_SECRET_ARN": "<DB_SECRET_ARN>", "ASGARD_SECRET_KEY_SECRET_ARN": "<KEY_SECRET_ARN>" },
     "grants": { "secrets_read": ["<DB_SECRET_ARN>", "<KEY_SECRET_ARN>"], "kms_decrypt": ["<KMS_ARN>"] }
   }
   ```
   Run exactly one replica (`desired_count: 1`) — the rollup/rotation loops assume a
   single writer.

   **Verify:** the request reaches `fulfilled`; `curl -fsS "$URL/readyz"` returns
   `ready` over https; the ECS target is `healthy`; the task-role policy contains
   the secrets + KMS grants (`aws iam get-role-policy …`).

5. **Cut over.** Point the Asgard hostname's DNS / ALB alias at `outputs.url`, then
   migrate any local state (re-register projects, or restore the Postgres dump).
   Decommission the local instance once the ECS Asgard serves `/readyz`.

**Dogfood done:** the ECS Asgard answers `/readyz`, lists the catalog over `/mcp`,
and was provisioned end-to-end by the local Asgard with no console clicks.
