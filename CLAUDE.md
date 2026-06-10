# CLAUDE.md — Frontkeep

Orientation for an agent working in this repo. Read this first; it's evergreen
(architecture + conventions), not session state.

## What Frontkeep is

An open-source, **single static Rust binary** that is a governance control plane
for AI/agent development: a manifest-driven **service catalog** + model
**gateway** + **policy** (Cedar) + **cost** attribution + **registry** + audit,
exposed over CLI / MCP / REST+GraphQL / an embedded UI.

**Agents first, humans second.** Every capability must be an **MCP tool**, not
just a UI/REST route. The product is a **hub, not a workflow engine** — serve
guidance/data and let the agent act; resist building bespoke orchestration
("world-building" is the main failure mode).

The real target is a **governed onboarding loop**: point a new effort's AI agent
at a seed repo → it reads company standards → **mandatory project registration
(the gate)** → unlocks **provisioning of services through the orchestrator** →
all model + resource cost **segregated by project / owner / manager / group**.

## Build & verify (the done bar — actually run these)

```bash
export PATH="$HOME/.cargo/bin:$PATH"      # always; profile has [profile.dev] debug=0 (keep)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash scripts/e2e.sh                        # SQLite
DATABASE_URL="postgres://postgres:postgres@127.0.0.1:5433/asgard" bash scripts/e2e.sh   # Postgres
bash scripts/cleanroom-check.sh            # MUST pass before every commit
```

Local Postgres for e2e:
```bash
docker run -d --name asgard-ci-pg -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=frontkeep -p 5433:5432 postgres:16
# reset between runs:
docker exec asgard-ci-pg psql -U postgres -c "DROP DATABASE IF EXISTS frontkeep WITH (FORCE);" -c "CREATE DATABASE asgard;"
```

Run it locally and browse the UI (no login on loopback):
```bash
FRONTKEEP_DEV_INSECURE=1 FRONTKEEP_DATABASE_URL="sqlite:///tmp/asgard.db" \
  ./target/debug/frontkeep serve --bind 127.0.0.1:8080    # http://localhost:8080
```

## Crate map (`crates/`)

- **storage** — `Db` over SQLite+Postgres via sqlx `Any`. `Db::q(sql)` rewrites
  `?`→`$1` for PG (wrap every SQL in `self.db.q(...)`). Migrations are embedded,
  ordered `(version, include_str!)` in `src/migrations.rs`; the splitter splits on
  `;` so keep semicolons out of migration comments. `ALTER ADD COLUMN` with a
  constant default is the portable change.
- **catalog** — entity model, `SchemaRegistry` (embeds `schemas/*.schema.json`),
  ingestion/reconcile, and the **agent-seed** (`src/seed.rs` + `seed/.agent/**`,
  embedded via `include_str!`; selectable via `seed_plan`).
- **policy** — Cedar `PolicyEngine`; `policies/default.cedar`.
- **gateway** — `complete()` hot path: resolve key → policy → guardrails →
  provider → spend + usage + audit. Providers: Mock (default, offline) +
  OpenAI/Anthropic (live when their `*_MASTER_KEY` env is set).
- **registry** — project registration (the gate), `proj-YYYY-NNNN` minting,
  cost rollups over `usage_events` (dims denormalized per row). Also the
  **promotion gate**: pure `promotion::evaluate` → `request_promotion` →
  `finalize_promotion` routing, and the async **`review_jobs`** queue
  (`review_jobs.rs` + `drain_reviews`/`run_review_job`) the code reviewer runs in.
- **reviewer** — the machine-review panel feeding the promotion gate
  **escalate-only** (may add exception signals → `flagged`/human; never relaxes a
  gate). Manifest+`kind` dispatch like provision: `llm-judge` (inline, judges the
  evidence record) + `webhook` (external) + **`code-review`** (async — reads the
  repo over the GH/GL API via `repo.rs`, *no clone*, judges it against the
  standards store per tier, `run_tool_loop`). The whole panel is **off unless a
  real LLM is reachable** (`FRONTKEEP_REVIEW_ALLOW_MOCK=1` forces a deterministic mock
  for dev/e2e). See `docs/docs/review-gate.md`.
- **provision** — manifest service catalog (`manifest.rs`) + connectors
  (`connectors/terraform.rs` is the universal path) + cost rollup/dashboard
  (`cost/`). Cost dims are denormalized onto `cost_rollup` rows.
- **identity** — local users (Argon2) + sessions; OIDC via `/userinfo`; **RBAC**
  (`Role` = admin/finance/member, `Capability`, one matrix). SAML/SCIM stubbed.
- **workflow**, **eval**, **runtime** — request→approve→fulfill; eval gate;
  Runtime trait (gVisor/container/local).
- **api** — axum 0.8 + async-graphql. `require_session` gates the human surface;
  `FRONTKEEP_DEV_INSECURE=1` (loopback only) bypasses it and makes `/api/auth/me`
  return a synthetic admin so the UI needs no login.
- **mcp** — stdio + remote (`/mcp`, rmcp) JSON-RPC tools, the **control plane**.
  Bearer-gated by either a **user PAT** (`asg_pat_…` → acts across every project
  the user owns/manages; can register) or a **project key** (`asg_…` → one
  project). `McpAuth` enum carries the principal; `resolve_project` authorizes per
  variant. Inference is *not* here (service usage): mint the project LLM key and
  call `/api/gateway/chat` out-of-band.
- **asgard** — the binary; `build_core` / `serve`; embeds `web/dist` (rust-embed)
  and `modules/` for container deploys.

## Authoring patterns

- **New provisionable service = drop a manifest + a TF module. No recompile.**
  `services/<id>/service.yaml` (id, category, `classification_min/max`,
  `required_fields`, `auto_approvable`, `secret_outputs`, `cost`,
  `provisioner.connector: terraform` + `config.module`) → `modules/aws/<id>/*.tf`.
  The connector writes every spec field + the immutable project `tags` as tfvars;
  sensitive TF outputs listed in `secret_outputs` route to the secret store.
  Cost-bearing / IAM-shaping services set `auto_approvable: false`. Non-AWS is the
  same path: `modules/databricks/*` + `modules/auth0/*` prove the connector is
  provider-agnostic (the TF subprocess inherits provider creds from Frontkeep's env).
  Two optional manifest knobs: `long_running: true` (latency hint — returns the
  `provisioning` record immediately, apply runs in the background; never a
  correctness lever) and `retry: {max_attempts, base_secs, cap_secs}` (per-service
  auto-retry override; `max_attempts: 0` disables it; fleet default is
  `provision_max_retries`). Every connector run (apply/destroy, success+failure) is
  captured to `provision_runs` per resource — encrypted at rest, `ViewAudit`-gated
  (REST `…/resources/{rid}/runs`, MCP `resource_runs`, UI Logs button). Full
  contract + commented skeleton: `docs/docs/authoring-a-service.md`.
- **New inference backend = a plug-in manifest, never core code.** `kind:
  openai-compatible` + `base_url_env` + (if the path isn't `/v1/chat/completions`)
  a `chat_path` with an optional `{model}` placeholder. Databricks Model Serving is
  exactly this: `chat_path: /serving-endpoints/{model}/invocations` — no
  Databricks-specific provider in the gateway. Cost sources (e.g.
  `databricks-billing` over `system.billing.usage`) are registered plugins keyed by
  `cost.source.type`, driven by the daily rollup loop + `POST /api/cost/rollup`.
- **RBAC model:** roles are org-wide (admin/finance/member). Authority over a
  *specific* project (see its cost, it shows in your list) is **automatic from
  the owner/manager relationship**, not a role — cost + projects reads are scoped
  to `owner == me OR manager == me` unless the caller has `ViewAllCost`
  (admin/finance). See `scope_for` in `crates/api/src/lib.rs`.
- **Cost has two paths:** model spend (`usage_events`, via `registry::cost`) and
  infra (`cost_rollup`, via `provision::cost`). Both denormalize owner/manager, so
  scoping is a plain predicate. `POST /api/cost/rollup` recomputes (CE itself lags ~24h).

## Conventions / guardrails

- **Clean-room (hard):** this is public OSS — never commit the employer or
  predecessor-platform names, internal hostnames, the dev cloud account number, or
  personal usernames. The full denylist + the scanner are in `scripts/cleanroom-*`;
  run `scripts/cleanroom-check.sh` before every commit. Docs use placeholders;
  real values stay at runtime only.
- **Commits:** conventional; first commit on a branch may be multi-line, every
  one after single-line; never mention AI tooling. Branch `glemmestad/v0` → PR to
  `main`. Feature-branch only (don't commit to `main` directly).
- **Code style:** minimum code that solves it; surgical (every line traces to the
  task); **no narrative comments** — comments explain a non-obvious *why* only.
- **UI** is one self-contained file `web/dist/index.html` (rust-embed, no build,
  no deps, vanilla JS). Reuse helpers `el/esc/api/money/PAL/spark/pill`. Dark
  token system. Verify with `node --check` on the script block.
  - **Destructive actions** (delete / decommission / revoke / disable / archive)
    must go through the shared `confirmAction({ title, message, confirmLabel })`
    modal — never the native `confirm()`/`alert()`. One confirmation UX across the
    app (project kill/decommission, token revoke, user disable, catalog delete all
    use it); consistency is the point.
- Our own engineering standards live in `seed/.agent/{STANDARDS,DONE,SECURITY}.md`
  — and we follow them. "Done" = gates green (and run) + behavior verified + no
  orphaned code + clean-room clean + a change summary.

## Operator/agent docs

`docs/docs/`: `deploy.md` / `deploy-agent.md` (deploy Frontkeep, incl. self-deploy on
ECS), `connect-agent.md` (point an MCP client at `/mcp`), `migrate-app.md`
(stand a real app on Frontkeep's primitives), `inference-backends.md`.

## State / planning

Session handoff and current status live in `.context/CONTINUE.md` (read it for
"what's done right now").

**`plans/ROADMAP.md` is the prioritized, classified backlog** — every item rated
importance + effort (Low/Med/High). Keep it current:
- When you defer something, hit a limitation, or surface a worthwhile idea mid-task,
  **add a line to ROADMAP.md** (right table, with importance + effort) rather than
  letting it evaporate in chat.
- When you finish a roadmap item, **cross it off** (strike the row or remove it) in
  the same change that completes the work.
- `plans/BACKLOG.md` is the older raw "don't forget" pile; ROADMAP.md is the
  prioritized view that drives what to build next.
