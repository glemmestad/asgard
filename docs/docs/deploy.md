---
sidebar_position: 6
title: Deploy (operator guide)
---

# Deploying Frontkeep

Frontkeep ships as **one statically-linked binary** that serves everything on a
single port: the web dashboard (`/`), the REST API (`/api/*`), GraphQL
(`/graphql`), and the **remote MCP server** (`/mcp`, Streamable HTTP) that agents
connect to. There is no separate frontend build, no sidecar, no message broker.

This guide gets you from nothing to a reachable, governed deployment. It branches
into two paths — pick one:

- **POC-local** — built-in local users, no external identity provider, no live
  cloud provisioning. Fully usable, MCP included. The fastest way to a real test
  deployment.
- **Enterprise** — OIDC/Auth0 single sign-on for humans **and** armed Auth0 (or
  AWS) provisioning. Layered on top of the POC path; adopt it once the basics
  work.

The recommended method is deliberate: **deploy the POC path first, knowing it
will hit edges in your environment, find where it stops, then iterate.**

---

## The auth ladder

Frontkeep is **secure by default and never ships wide-open**, but it does not force
an identity provider on you. Three rungs:

| Rung | What | When |
|---|---|---|
| **1 — local users** | Built-in username/password accounts + sessions. On first boot, if no admin exists and `FRONTKEEP_ADMIN_PASSWORD` is unset, Frontkeep **generates an admin password and logs it once**. | Default. Zero external dependencies. |
| **2 — OIDC / SSO** | Authorization-code login against your IdP (Auth0, Okta, Entra, …). Coexists with local users by default (local admin = break-glass); roles can be driven from the IdP and local login can be turned off entirely (see [SSO-driven roles](#sso-driven-roles)). | Enterprise. Set the `FRONTKEEP_OIDC_*` env. |
| **3 — dev escape hatch** | `FRONTKEEP_DEV_INSECURE=1` disables human-session enforcement. **Off by default, only honored on a loopback bind, logs a loud warning.** | Throwaway local hacking only. Never in a deployment. |

Two things are gated independently of the human rung and are **always on**:

- **Agent inference** (`/api/gateway/chat`) is gated by a per-project virtual key.
- **The MCP server** (`/mcp`) is gated by a per-project virtual key on every
  request — even when rung 3 is enabled. A missing or invalid key is `401`.

So a human signs in (rung 1 or 2) to use the dashboard; an agent presents a
project virtual key to use `/mcp`. Different credentials, same enforcement.

### The first credential

On a fresh deploy nobody has a PAT yet. Two ways to mint the first one:

```sh
# DB-direct — no running server needed. Ensures the admin user exists
# (FRONTKEEP_ADMIN_PASSWORD, or a generated password printed once) and prints a PAT.
frontkeep --database-url "$FRONTKEEP_DATABASE_URL" admin bootstrap
```

Or against a running server, with the admin password from the first-boot log:

```sh
TOKEN=$(curl -s -X POST "$BASE/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"admin","password":"<password>"}' | jq -r .token)
curl -s -X POST "$BASE/api/auth/tokens" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"name":"bootstrap"}' | jq -r .token
```

Either way the result is a user PAT (`asg_pat_…`) — point an MCP client at
`/mcp` with it ([Connect an agent](connect-agent.md)) and the governed loop is
open. Both paths are idempotent and audited.

---

## The container image

Official images publish to **GitHub Container Registry** on every released
version:

```
ghcr.io/glemmestad/asgard:<tag>
```

Tags, set by the release pipeline (`.github/workflows/release.yml`):

| Tag | Points at | Use for |
|---|---|---|
| `vX.Y.Z` | An exact released version (semantic-release). | **Pin this in production.** Immutable, reproducible. |
| `latest` | The most recent release. | Trying things out; never pin a deployment to it. |
| `sha-<short>` | The exact commit that built the image. | Tracing an image back to source. |

The image bundles `terraform` on `PATH` and the provisioning modules at `/modules`,
so an armed deployment needs no extra mounts. (Running your own fork/registry?
Substitute your image path — nothing in Frontkeep hard-codes `ghcr.io/glemmestad`.)

### Native binary

The same release also publishes static macOS/Linux binaries — the quickest way to
run `frontkeep serve` (or the [CLI](./cli.md)) without Docker. See
[Install](./install.md). Armed provisioning then needs `terraform` on your `PATH`
(the binary ships only itself); SQLite + the control plane work with no extra
dependencies.

## Prerequisites

- A host that can run the binary (or the container). **That's it — nothing else is
  required to get going.** No reverse proxy, no Redis, no Kubernetes: run the
  binary, reach it over `http://<host>:8080`, and sign in. TLS is an *optional*
  production upgrade (see below), not a prerequisite.
- **Postgres** for anything beyond a single-box trial (SQLite is the default and
  is fine for a first smoke test — no external DB needed to start).
- A 32-byte master key for the secret store (optional for a smoke test; source it
  from your KMS for a pilot).

---

## Optional: TLS via a reverse proxy

**You do not need a proxy to run Frontkeep.** Over plain http it serves the dashboard,
API, and MCP, and sign-in works (the session cookie is only marked `Secure` when a
request actually arrives over TLS, so plain http isn't broken by it). For a pilot
you'll still want TLS — the simplest way is to put any reverse proxy in front and
let it terminate TLS. If you do, set two headers so Frontkeep adapts correctly:

- **`X-Forwarded-Proto: https`** — tells Frontkeep the edge is TLS, so it marks the
  session cookie `Secure`. (Absent → plain http assumed → cookie not `Secure`, and
  login still works.)
- **`X-Forwarded-For`** — login brute-force throttling keys on the client IP from
  this header. Without it, all sources share one throttle bucket (still safe, just
  coarser).

Route `/`, `/api/*`, `/graphql`, and `/mcp` to the Frontkeep upstream. No WebSocket
upgrade is needed (MCP uses Streamable HTTP, i.e. plain POST + SSE responses), but
**don't buffer `/mcp` responses** if you want streaming to flow promptly.

> **Mind the idle timeout in front of `/mcp`.** Streamable HTTP holds a response
> stream open for the duration of a tool call. Any L7 hop with a short idle timeout
> will sever it mid-call — an **AWS ALB defaults to 60s**. Raise it to ~300–900s
> (the bundled `ecs-service` module exposes `idle_timeout`, defaulting to 300).
> nginx: `proxy_read_timeout 900s;`. Caddy handles long streams without tuning.

Caddy makes this automatic:

```caddy
asgard.example.com {
    reverse_proxy asgard:8080
    # Caddy terminates TLS and sets X-Forwarded-Proto / X-Forwarded-For for you.
}
```

nginx:

```nginx
server {
    listen 443 ssl;
    server_name asgard.example.com;
    # ssl_certificate / ssl_certificate_key ...
    location / {
        proxy_pass http://asgard:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;       # must be https
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_buffering off;                              # let /mcp SSE stream
    }
}
```

## Step 1 — Postgres

SQLite (the default) needs nothing. For a real pilot, run Postgres and point
Frontkeep at it.

```bash
docker run -d --name asgard-pg \
  -e POSTGRES_PASSWORD=change-me -e POSTGRES_DB=frontkeep \
  -p 5432:5432 -v asgard-pg:/var/lib/postgresql/data \
  postgres:16-alpine
```

Or with compose, alongside Frontkeep:

```yaml
# docker-compose.yml
services:
  db:
    image: postgres:16-alpine
    environment:
      POSTGRES_PASSWORD: change-me
      POSTGRES_DB: asgard
    volumes: [ "asgard-pg:/var/lib/postgresql/data" ]
  asgard:
    build: .            # or image: your-registry/asgard:tag
    depends_on: [ db ]
    ports: [ "8080:8080" ]
    environment:
      FRONTKEEP_DATABASE_URL: postgres://postgres:change-me@db:5432/asgard
      FRONTKEEP_BIND: 0.0.0.0:8080
      FRONTKEEP_SECRET_KEY: ${FRONTKEEP_SECRET_KEY}        # 64 hex chars from your KMS
      FRONTKEEP_ADMIN_PASSWORD: ${FRONTKEEP_ADMIN_PASSWORD} # optional; else auto-generated + logged
    volumes:
      - ./asgard.yaml:/asgard.yaml:ro
    command: [ "serve", "--config", "/asgard.yaml" ]
volumes: { asgard-pg: {} }
```

Frontkeep runs its own migrations on boot against whatever `FRONTKEEP_DATABASE_URL`
points to; the same schema works on SQLite and Postgres.

> **On ephemeral or replaceable compute, use Postgres — not SQLite.** SQLite is a
> file on the local disk. Where that disk is ephemeral (containers / Fargate /
> Kubernetes that get replaced on every deploy, crash, or scale event), each
> replacement starts from an empty DB and silently loses every project, key, and
> cost record. SQLite is the right call for a genuine single box whose disk
> persists across restarts — a laptop, a homelab, a VM with its own volume — the
> 5-person-shop / single-binary case, **no cloud required**. The moment compute is
> cattle, point `FRONTKEEP_DATABASE_URL` at any Postgres (managed or self-run); that's
> the documented pilot path and what the
> [self-deploy runbook](./deploy-agent.md#appendix--dogfood-self-deploy-asgard-on-ecs)
> uses. The database is the single system of record: back it up and you've backed up
> everything — projects, keys, cost, and the encrypted secret store.

> **Scaling — `desired_count > 1` is safe on Postgres.** The background loops (cost
> rollup, secret rotation, catalog reconcile, review sweep) are leader-leased: each
> tick runs on whichever replica wins a short DB lease, so exactly one replica does
> it. Terraform applies take a per-resource lease plus an optimistic version check on
> the stored state, so two replicas can't race one resource. Failover is bounded by
> the lease TTL (`lease_ttl_secs`, default 600s), and lease correctness assumes
> replica clocks are within a fraction of the TTL of each other (true under NTP). Run
> as many replicas as you like against one **Postgres**; the request path is
> stateless. **SQLite stays single-process** — it's a local file with one writer, so
> keep `desired_count: 1` there.

## Step 2 — The master key

The built-in secret store encrypts secret values with a 32-byte master key.
Source it from your KMS and inject it as **64 hex characters**:

```bash
export FRONTKEEP_SECRET_KEY=$(openssl rand -hex 32)   # or fetch from your KMS
```

It can also be set as `provisioning.secrets.master_key_hex` in `asgard.yaml`, but
the env var wins and is preferred so the key never lands in a config file. If
unset, a built-in dev key is used — **fine for a smoke test, not for a pilot.**

> **The master key is load-bearing and must stay stable.** Secret values are
> encrypted with it; there is no re-encrypt-on-rotate. If the key changes, every
> stored secret becomes undecryptable. Keep it in your KMS (not in the DB), and
> **back up the database** — the DB holds the encrypted secrets, the KMS holds the
> key, and you need both. Rotating the key is a deliberate migration, not a config
> tweak. The same master key also encrypts provisioning's Terraform state (stored
> in the DB, see below), so a key you can't recover means state you can't decrypt —
> one more reason to source it from your KMS and keep it stable.

## Step 3 — `asgard.yaml`

Provisioning, the group/cost-center allowlist, and catalog sources come from a
small config file mounted at a path you pass with `--config`. A minimal POC file:

```yaml
# Cost-centers a project may register against. Empty = open mode (any group).
groups:
  - { key: platform, display_name: Platform, cost_center: CC-100 }
  - { key: research, display_name: Research, cost_center: CC-200 }

# Which registration fields are mandatory. Defaults (shown) keep the strict
# posture; relax them so a solo founder/CEO can self-register without inventing a
# separate manager or a cost-center group.
registration:
  require_manager: true   # false → manager optional, defaults to the owner (self-manage)
  require_group: true     # false → group optional (ungrouped, blank cost-center)
  require_cost_center: false  # reserved; cost-center derives from group today
```

The governance operating model ships with the policy-doc defaults baked in, so
none of the following is required to boot. Add a block only to override a default:

```yaml
# Per-tier evidence required to promote into a tier (keys: light-operational /
# wide-operational / critical-path). Any tier you list replaces that tier's
# shipped default; tiers you omit keep theirs.
classification_requirements:
  wide-operational: [repo_or_source_url, support_contact, runbook_url, monitoring_or_logs_url]

# Lifecycle review-date engine. Defaults shown.
review:
  poc_window_days: 90    # first review deadline for a new POC
  auto_extensions: 1     # automatic +window grants before a human must decide
  sweep_secs: 86400      # how often the background sweep flags overdue reviews

# Portfolio-metric thresholds (the Governance dashboard tab / governance_metrics).
governance:
  maintainer_min: 2      # Wide/Critical systems below this count as understaffed
```

(Everything under `provisioning:` is optional and covered under "Arming
provisioning" below.)

## Step 4 — Boot and verify (POC-local)

```bash
FRONTKEEP_DATABASE_URL=postgres://postgres:change-me@localhost:5432/frontkeep \
FRONTKEEP_SECRET_KEY=$FRONTKEEP_SECRET_KEY \
frontkeep serve --bind 0.0.0.0:8080 --config ./asgard.yaml
```

On first boot with no `FRONTKEEP_ADMIN_PASSWORD`, the log prints a generated admin
username + password **once**. Grab it, then:

1. `curl -fsS http://localhost:8080/healthz` → `ok`.
2. Open `/` in a browser → you get the sign-in screen. Log in with the admin
   credentials. (Set `FRONTKEEP_ADMIN_PASSWORD` to control the password on future
   boots; change it after first login.)
3. Confirm the human surface is enforced: `curl -i http://localhost:8080/api/projects`
   with no session → `401`.

You now have a working, governed control plane. Onboard a project from the
dashboard (or via the agent runbook), mint a virtual key, and point an MCP client
at it (next step).

## Step 5 — Connect an MCP client

The MCP server is at `https://<host>/mcp` (Streamable HTTP). Authenticate with a
**project virtual key** as a bearer token. With the MCP Inspector or any
Streamable-HTTP client:

- URL: `https://<host>/mcp`
- Header: `Authorization: Bearer <project virtual key>`

`initialize` negotiates, `tools/list` shows the catalog (`list_services`,
`register_project`, `request_resource`, `seed_plan`, the `cost_*` tools, …), and
project-scoped tools act on the **key's** project — a different `project_id`
argument is denied. Mint the key from the dashboard (Projects → a registered
project → mint key) or with `POST /api/projects/{id}/keys`.

### Agent-seed over MCP

Agents bootstrap a repo's guidance through the seed tools. `seed_plan` takes the
repo's languages plus a description of the work and returns the **minimal**
relevant set of files (core operating agreement + per-language add-ons + matching
domain overlays + relevant templates), not a one-shot dump; `seed_get` returns
each file's body and the path to write it to. This is how a repo opts into your
standards without a human curating the file list.

---

## Enterprise: OIDC / SSO (rung 2)

Frontkeep uses the OIDC **authorization-code flow** and reads the user's profile from
the IdP's `/userinfo` endpoint (no local JWT/JWKS validation — lower operational
risk). Configure it with env vars; when `FRONTKEEP_OIDC_DOMAIN` is set, the
`Sign in with SSO` button appears on the login screen and `/api/auth/oidc/*`
becomes active.

```bash
FRONTKEEP_OIDC_DOMAIN=your-tenant.us.auth0.com          # endpoints derived from this
FRONTKEEP_OIDC_CLIENT_ID=...
FRONTKEEP_OIDC_CLIENT_SECRET=...
FRONTKEEP_OIDC_REDIRECT_URI=https://<host>/api/auth/oidc/callback
# FRONTKEEP_OIDC_SCOPES defaults to "openid email profile"
```

> **`FRONTKEEP_OIDC_*` and `AUTH0_*` are two unrelated credential sets — don't
> conflate them.** `FRONTKEEP_OIDC_*` is **human login** (the authorization-code flow
> against any OIDC IdP — Auth0, Okta, Entra) and is read by Frontkeep itself.
> `AUTH0_*` is **provisioning** (M2M Management-API creds passed through to the
> Terraform Auth0 provider, see "Arming provisioning" below) and is read by the
> `terraform` child process, not Frontkeep. They happen to overlap only when your IdP
> *is* Auth0 — and even then they are **two separate Auth0 apps** (a Regular Web
> App for login, an M2M app for provisioning). Setting one does nothing for the
> other.

In your IdP, create a **Regular Web Application** for login:

- Allowed callback URL: `https://<host>/api/auth/oidc/callback` (must match
  `FRONTKEEP_OIDC_REDIRECT_URI` exactly).
- Grant: authorization code. Scopes: `openid email profile`.

The local admin still works as a break-glass account alongside SSO. Live callback
URL / audience tuning is expected in-environment iteration — if the callback
fails, the most common cause is a mismatched redirect URI.

### SSO-driven roles

By default a new SSO user lands as **member** and an admin promotes them from the
Users page. Two env knobs let the IdP drive roles instead:

```bash
# Promote-only admin allowlist. These emails are made admin on every login.
# Additive: never demotes, never locks the UI. A reliable break-glass.
FRONTKEEP_ADMIN_EMAILS=alice@corp.com,bob@corp.com

# Authoritative group-claim sync. Setting either of these makes the IdP the SOLE
# source of truth for OIDC roles: every login recomputes the role from the groups
# claim (admin group → admin; else finance group → finance; else member, INCLUDING
# demotion), and the Users page can no longer edit OIDC users' roles.
FRONTKEEP_OIDC_ADMIN_GROUPS=platform-admins
FRONTKEEP_OIDC_FINANCE_GROUPS=finance
# Userinfo claim the group values are read from. Default `groups`. Auth0 custom
# claims are namespaced, so usually something like the line below.
FRONTKEEP_OIDC_GROUPS_CLAIM=https://<host>/groups
```

Behavior:

- **Neither group var set** → group sync is off; OIDC roles stay manually managed
  (today's behavior). `FRONTKEEP_ADMIN_EMAILS` still applies as a promote-only grant.
- **A group var set** → authoritative sync is on. `FRONTKEEP_ADMIN_EMAILS` is *unioned
  in* as admin even in this mode, so a misfiring groups claim can't strip your named
  break-glass admins.

For Auth0, the groups claim is **not** emitted by default — add it in a **Login /
Post-Login Action** and use a namespaced key (Auth0 silently drops non-namespaced
custom claims):

```js
exports.onExecutePostLogin = async (event, api) => {
  const ns = "https://<host>/";
  const groups = (event.authorization && event.authorization.roles) || [];
  api.idToken.setCustomClaim(ns + "groups", groups);
  api.accessToken.setCustomClaim(ns + "groups", groups); // so /userinfo returns it
};
```

Set `FRONTKEEP_OIDC_GROUPS_CLAIM=https://<host>/groups` to match. The value must be in
`/userinfo` (Frontkeep reads profile from there, not the ID token).

### SSO-only: disabling local login

```bash
FRONTKEEP_DISABLE_LOCAL_LOGIN=1
```

Fully disables username/password sign-in — for **everyone**, including the
bootstrap admin. The login screen drops the password form and, when unauthenticated,
**auto-redirects to the IdP** (no "Sign in with SSO" click). `POST /api/auth/login`
returns `403`.

- **Anti-lockout guard:** the flag is *ignored* (local login stays on, with a logged
  error) unless OIDC is configured. Set up an SSO admin (`FRONTKEEP_ADMIN_EMAILS` or an
  admin group) and confirm you can sign in **before** flipping this on.
- **Break-glass once disabled:** unset the env var and restart (or, on a loopback
  bind, `FRONTKEEP_DEV_INSECURE=1`). There is no in-app local fallback by design.

## Enterprise: arming provisioning

Out of the box, provisioning is **unarmed** (the catalog is discoverable and the
dry-run path works, but nothing real is created). There are two ways to arm it —
pick one:

**Env-only (container-first, no config file).** Set these on the Frontkeep process and
the `terraform` connector registers on boot:

```bash
FRONTKEEP_TF_MODULES_DIR=/modules                       # bundled in the official image
FRONTKEEP_TF_WORK_DIR=/data/asgard-tf                   # scratch only; can be ephemeral
# FRONTKEEP_TF_ALLOWED=aws:1234567890                   # OPTIONAL multi-account guardrail

# AWS provisioning context (region + account are AWS-wide; subnet/SG are RDS-only):
AWS_DEFAULT_REGION=us-west-2                          # standard provider env, all AWS modules
FRONTKEEP_AWS_DEFAULT_ACCOUNT=123456789012              # default target + attribution account
FRONTKEEP_RDS_SUBNET_GROUP=my-db-subnets                # RDS placement; omit → default VPC
FRONTKEEP_RDS_SECURITY_GROUP_IDS=sg-123,sg-456          # RDS security groups (csv)

# Auth0 (all optional; omit → bare client, no API, no enforced connection):
AUTH0_RESOURCE_SERVER_TEMPLATE=https://api-{project}.example.com/   # {project} → project id; emitted as `audience`
AUTH0_DEFAULT_CONNECTIONS=my-sso-connection          # existing tenant connections to enable (csv)
```

`FRONTKEEP_TF_MODULES_DIR` is the switch that arms real provisioning — set it and the
`terraform` connector registers; omit it and every terraform-backed service silently
falls back to the dry-run **stub** (a "fulfilled" request that built nothing). The
provider credentials Terraform uses are inherited from Frontkeep's own environment (the
IAM role / instance profile it runs under, plus `AUTH0_*`, etc.).

`FRONTKEEP_TF_ALLOWED` is an **optional** `cloud:account` allowlist — a multi-account
*hardening* guardrail, not a per-resource list and not required to provision. On a
single-account deploy the IAM role Frontkeep runs under is the real boundary; leave this
unset and it provisions into the ambient account. Set it (`aws:<account-id>`,
`auth0:<tenant>`) only to constrain which accounts Frontkeep may target when it can
assume into several; the first entry is the default target.

This is the recommended path for a container deploy — no `asgard.yaml` needed for
the headline feature. (You still set the provider creds below, e.g. `AUTH0_*`.)

#### Bring your own services (operator overlay)

The official image embeds the built-in catalog. To add **your own** provisionable
services without a recompile, point `FRONTKEEP_SERVICES_DIR` at a directory of
`service.yaml` files (one per service, same shape as the built-ins). They overlay
the embedded catalog, adding or overriding by `id`. Each service that uses the
`terraform` connector references a module under `FRONTKEEP_TF_MODULES_DIR`. Setting
`FRONTKEEP_SERVICES_DIR` alone arms the overlay even without Terraform (services on
other connectors still load).

How the files get to that directory is your call — three patterns, lightest first:

1. **Derived image** (keeps the single-versioned-artifact property): build `FROM
   asgard:<tag>`, `COPY` your `service.yaml`s and TF modules in, and set the two
   env vars. The catalog is versioned with the image. Best when the catalog changes
   at deploy cadence.
   ```dockerfile
   FROM asgard:0.7
   COPY my-services/ /srv/services/
   COPY my-modules/  /srv/modules/
   ENV FRONTKEEP_SERVICES_DIR=/srv/services
   ENV FRONTKEEP_TF_MODULES_DIR=/srv/modules
   ```
2. **Shared volume / EFS mount** (mutate the catalog without a redeploy): mount one
   EFS access point (or any volume) into every task and set
   `FRONTKEEP_SERVICES_DIR=/mnt/services`, `FRONTKEEP_TF_MODULES_DIR=/mnt/modules`. Edit
   files on the volume and every node sees them. Cost: you now run EFS, and config
   lives outside the image.
3. **Object-storage sync** (decoupled, no shared filesystem): an ECS sidecar or
   container entrypoint syncs an S3 prefix into a local dir on start
   (`aws s3 sync s3://my-bucket/services /srv/services`), then Frontkeep reads it via
   `FRONTKEEP_SERVICES_DIR`. Lighter than EFS; adds a startup step.

All three converge on the same contract: **files at a path, two env vars pointed at
it.** Frontkeep doesn't care which delivery you choose.

> **Terraform state is durable in the database.** Around every apply/destroy,
> Frontkeep snapshots each resource's state into its own DB (the same SQLite or
> Postgres as everything else), encrypted with the master key. So `work_dir` is
> just scratch and may be ephemeral — back up the database and you've backed up
> your infrastructure state along with everything else. No S3, no remote backend,
> no extra dependency. (Each apply takes a per-resource lease and a version check on
> the stored state, so multiple replicas can't corrupt it; see "Scaling".)

**Config file (full control).** Or arm it from `asgard.yaml` when you want the other
provisioning knobs (auto-approve, services overlay, AWS cost sources) in one place:

1. Add a `terraform` block to `asgard.yaml` pointing at the bundled modules. The
   official container ships `terraform` on `PATH` and the modules at `/modules`,
   so no mount is needed — just point `modules_dir` there:

   ```yaml
   provisioning:
     terraform:
       modules_dir: /modules         # bundled in the image (or your own mounted tree)
       work_dir: /data/asgard-tf     # scratch only; state is kept in the DB
     # Allow only the targets you intend to provision into:
     allowed:
       - { cloud: auth0, account: your-tenant }
   ```

   (Running from source instead of the container? Point `modules_dir` at the
   repo's `modules/` directory and ensure `terraform` is on `PATH`.)

2. **Auth0 provisioning** (the `auth0-application` service) uses the Terraform
   Auth0 provider, which reads **M2M Management API credentials from the
   environment**. The connector spawns `terraform` as a child process that
   inherits Frontkeep's environment, so setting these on the Frontkeep process is
   sufficient:

   ```bash
   AUTH0_DOMAIN=your-tenant.us.auth0.com
   AUTH0_CLIENT_ID=...        # a Machine-to-Machine app authorized for the Management API
   AUTH0_CLIENT_SECRET=...
   ```

   So the enterprise path uses **two Auth0 apps**: a Regular Web App for human
   login (above) and an M2M app for provisioning (here).

3. AWS provisioning is the same Terraform path; keep it unarmed for a first
   deploy and add the AWS target only when you are ready. Cost Explorer reads are
   independent of provisioning and can be enabled on their own.

Provisioned secret values (e.g. an Auth0 client secret) are stored as a
`secret_ref` in the encrypted secret store and surfaced only over the
project-key-gated `get_secret` MCP tool — never in the resource record, the DB in
plaintext, or the audit log.

---

## Environment variable reference

| Variable | Purpose | Default |
|---|---|---|
| `FRONTKEEP_DATABASE_URL` | `sqlite://…` or `postgres://…`. Migrations run on boot. | `sqlite://asgard.db` |
| `FRONTKEEP_BIND` | Listen address. | `0.0.0.0:8080` |
| `FRONTKEEP_SECRET_KEY` | 64 hex chars (32 bytes) for the secret store. From your KMS. **Load-bearing and one-way — changing it orphans every stored secret** (see Step 2). | dev key (insecure) |
| `FRONTKEEP_SYSTEM_NAME` | Display name the dashboard rebrands to (see "Rebranding" below). | `Frontkeep` |
| `FRONTKEEP_ADMIN_USER` | Initial admin username. | `admin` |
| `FRONTKEEP_ADMIN_PASSWORD` | Initial admin password. If unset and no admin exists, one is generated + logged once. | (generated) |
| `FRONTKEEP_OIDC_DOMAIN` | IdP domain; presence enables SSO. Endpoints derived as `/authorize`, `/oauth/token`, `/userinfo`. | (off) |
| `FRONTKEEP_OIDC_CLIENT_ID` / `_SECRET` / `_REDIRECT_URI` | OIDC web-app credentials + callback. | — |
| `FRONTKEEP_OIDC_SCOPES` | Space-separated scopes. | `openid email profile` |
| `FRONTKEEP_ADMIN_EMAILS` | Comma-separated emails promoted to admin on every SSO login. Additive, promote-only (never demotes), never locks the UI. | — |
| `FRONTKEEP_OIDC_ADMIN_GROUPS` / `_FINANCE_GROUPS` | Comma-separated group values → admin / finance. Setting either turns on **authoritative** group-claim sync (IdP owns OIDC roles incl. demotion; UI can't override). | — |
| `FRONTKEEP_OIDC_GROUPS_CLAIM` | Userinfo claim the group values are read from. Auth0 custom claims are namespaced (e.g. `https://<host>/groups`). | `groups` |
| `FRONTKEEP_DISABLE_LOCAL_LOGIN` | `1`/`true` fully disables username/password sign-in (everyone, incl. bootstrap admin); UI drops the form and auto-redirects to SSO. **Ignored unless OIDC is configured** (anti-lockout). | off |
| `FRONTKEEP_DEV_INSECURE` | `1`/`true` disables human-session enforcement. Loopback-only; ignored otherwise. | off |
| `FRONTKEEP_FORCE_HTTPS` | `1`/`true` forces `Secure` on auth cookies regardless of detected scheme — "HTTPS is required." Set this when TLS is mandatory everywhere. | off (adaptive) |
| `AUTH0_DOMAIN` / `AUTH0_CLIENT_ID` / `AUTH0_CLIENT_SECRET` | M2M creds passed through to the Terraform Auth0 provider when provisioning is armed. | — |
| `FRONTKEEP_TF_MODULES_DIR` | Arms the `terraform` connector **without a config file** — point it at the bundled modules (`/modules`). Presence is what registers the connector. | (off) |
| `FRONTKEEP_SERVICES_DIR` | Operator overlay dir of your own `service.yaml` files (added/overridden by `id` on top of the embedded catalog). Lets a deployed image add services without a recompile or an `asgard.yaml`; arms the overlay even without Terraform. See *Bring your own services*. | (off) |
| `FRONTKEEP_TF_WORK_DIR` | Scratch dir for Terraform working dirs. **State itself is kept (encrypted) in the DB**, so this may be ephemeral. | system temp |
| `FRONTKEEP_TF_ALLOWED` | **Optional** `cloud:account` allowlist (e.g. `aws:1234567890,auth0:your-tenant`) — a multi-account *hardening* guardrail, not a per-resource list. The real boundary on a single-account deploy is the IAM role Frontkeep runs under; leave this unset and it provisions into the ambient account. Set it to constrain which cloud accounts Frontkeep may target when it can assume into several. First entry is the default target. | — |
| `AWS_DEFAULT_REGION` / `AWS_REGION` | **Standard AWS env**, read by the Terraform AWS provider for *every* AWS module — the one place to set the region all AWS resources deploy into. Frontkeep adds no region var of its own; a request may still override per-resource via `spec.region`. | (provider default) |
| `FRONTKEEP_AWS_DEFAULT_ACCOUNT` | AWS-wide default account id for attribution + the request gate's default target. Set it and provisioning into that account works without `FRONTKEEP_TF_ALLOWED` (it's added to the allowlist and made the default target). The account Terraform actually deploys into is still whatever Frontkeep's IAM creds resolve to. | — |
| `FRONTKEEP_RDS_SUBNET_GROUP` | RDS-only: the DB subnet group `rds-postgres` deploys into (operator network placement, so agents don't supply it). Unset → the module falls back to the default VPC. | — |
| `FRONTKEEP_RDS_SECURITY_GROUP_IDS` | RDS-only: comma-separated security group ids for `rds-postgres`. Unset → default VPC security group. | — |
| `FRONTKEEP_AUTO_APPROVE_CEILINGS` | Per-classification monthly self-service ceilings, `classification=usd` comma list, e.g. `poc=500,light-operational=2500,wide-operational=10000,critical-path=25000`. A request whose project-total infra stays under its tier's ceiling auto-approves; above it routes to human review. Merged per-tier onto the defaults. | poc=500, light-op=2500, wide-op=10000, critical-path=25000 |
| `FRONTKEEP_GIT_TOKEN` | Token for catalog source repos (GitHub/GitLab), if configured. | — |
| `FRONTKEEP_GUARDRAIL_MODE` | `enforce` (default) or `monitor`. | `enforce` |

Provider keys for inference backends (e.g. `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`)
activate the corresponding inference modules when present; see
[Inference backends](./inference-backends.md).

---

## Rebranding the dashboard

Set **`FRONTKEEP_SYSTEM_NAME`** (e.g. `Acme Control Plane`) to rebrand the deployment. It is
**cosmetic and UI-only** — it changes:

- the browser tab title,
- the header brand text (every `.brand` element), and
- the logo glyph (the first letter of the name),

served via `GET /api/auth/config` so the change is live on next page load. It does
**not** rename anything functional: the MCP server still identifies as `asgard` in
the `initialize` handshake, project ids keep the `proj-YYYY-NNNN` shape, env var
names stay `FRONTKEEP_*`, and log lines / API paths are unchanged. Set it once on the
process; there's nothing else to configure.

---

## Operational notes

- **Probes.** `GET /healthz` is **liveness** (static `ok`, touches nothing).
  `GET /readyz` is **readiness** — it confirms the database is reachable and
  returns `503` if not. Point your orchestrator's readiness probe at `/readyz`,
  liveness at `/healthz`. The container's `HEALTHCHECK` uses `/readyz`.
- **Graceful shutdown.** On `SIGTERM`/Ctrl-C the server stops accepting new
  connections and drains in-flight requests before exiting. Combined with the
  single-replica model, give the process a few seconds to stop.
- **Cookies.** The session cookie is `HttpOnly; SameSite=Lax`, and `Secure`
  **only when the request arrived over TLS** (`X-Forwarded-Proto: https`). So a
  plain-http deployment works out of the box, and once you put TLS in front the
  cookie is automatically `Secure` and never crosses a plaintext hop — no config
  flag to flip. Enterprises that mandate TLS everywhere can set
  **`FRONTKEEP_FORCE_HTTPS=1`** to force `Secure` on unconditionally (so a cookie can
  never be issued non-`Secure`, even if a misconfigured proxy drops the header).
- **CORS.** There is no permissive CORS layer — the dashboard is same-origin and
  API/MCP consumers aren't browsers, so cross-origin browser access is denied by
  default. If you front the API from a different origin, that's a deliberate
  change to make.
- **Login throttling.** Local sign-in is rate-limited per source IP
  (`X-Forwarded-For`): repeated failures lock that source out for a few minutes.
  It's best-effort and in-memory (per replica) — Argon2 already makes each attempt
  expensive. Prefer SSO (rung 2) for the human surface in an enterprise setting.

## Troubleshooting

- **Dashboard returns 401 for everything.** Expected when not signed in. Log in at
  `/`. If you can't, check the boot log for the generated admin password, or set
  `FRONTKEEP_ADMIN_PASSWORD` and restart.
- **`/mcp` returns 401.** The bearer token must be a valid, active **project
  virtual key**, not a human session token. Mint one for a registered project.
- **`/mcp` returns 404.** You're hitting the wrong path or method — it's `POST`
  (and `GET`/`DELETE`) on exactly `/mcp`.
- **OIDC callback fails / `state mismatch`.** The redirect URI registered in the
  IdP must equal `FRONTKEEP_OIDC_REDIRECT_URI` exactly (scheme, host, path). The
  state cookie is short-lived; don't reuse a stale callback URL.
- **Armed Auth0 provisioning fails with auth errors.** Confirm the `AUTH0_*` M2M
  variables are set **on the Frontkeep process** (the Terraform child inherits them)
  and the M2M app is authorized for the Management API scopes the module needs.
- **`FRONTKEEP_DEV_INSECURE=1` had no effect.** It's only honored on a loopback bind
  (`127.0.0.1`/`localhost`/`::1`); on any other bind it logs a warning and stays
  off.
