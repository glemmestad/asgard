<h1 align="center">Frontkeep</h1>

<p align="center"><b>The Agent Control Plane.</b><br/>
Where your agents ship at AI speed — with your policies and budgets built in.</p>

<p align="center">Open-source. Single static Rust binary. Agent-native.<br/>
One front door to every AI model, infrastructure service, and engineering standard your org runs on.</p>

---

## Why

Most platforms answer *"what services exist and who owns them."* For an agent that isn't enough — the agent needs to know **is this safe, is it working, what is it costing, and what did it just do.** And the company needs to let agents move fast without going off the rails.

Frontkeep is the front door agents use to do real work in your company. They register projects (the mandatory gate), provision real services through your standards, call models with budgets and policy guardrails attached, and every spend traces back to a project, owner, manager, and group. AI is the loudest tenant — Frontkeep is the building it lives in, alongside infrastructure, data services, and everything else an agent needs.

It's a **hub, not a workflow engine**: Frontkeep serves the standards, mints the credentials, owns the audit log. The agent does the work.

## What you get

- **Service catalog (manifest-driven).** Everything an agent can provision is one YAML per service (`services/<id>/service.yaml`) declaring how it's provisioned (`provisioner.connector`) and how its cost is attributed (`cost.source.type`). Adding a service is dropping a manifest + a Terraform module — **no recompile**. The `terraform` connector is the universal path (any cloud); `exec`/`http`/`litellm` cover the rest.
- **Registry — the gate.** Nothing provisions or spends until a project is registered and active. Registration mints a stable `proj-YYYY-NNNN` id and supplies the attribution dimensions stamped onto every usage event. Lifecycle: classification tiers, evidence-gated promotion/demotion, review-date sweeps.
- **Gateway.** Every model call routes through it: per-project virtual keys, budgets, model allowlists per data-classification, PII/secret/prompt-injection guardrails, full audit. **OpenAI + Anthropic built in** (Mock by default, offline); **any OpenAI-compatible backend — LiteLLM, Databricks Model Serving, vLLM — is an enableable module**, not core code. LiteLLM also supports **per-project virtual keys** as a governed, budgeted, cost-attributed resource.
- **Policy.** One Cedar engine queried by gateway, catalog, workflow, and runtime: *can this principal do this, against this data class, with this model — and does it need approval, from whom?*
- **Cost.** Model spend and infrastructure spend both denormalize owner/manager/group, so attribution is a plain query. Daily rollups, month-to-date deltas, EOM forecast, an org cost tree, and a governed cost Q&A.
- **Knowledge platform.** Normative **standards**, advisory **guidance**, and **recipes** (composable runbooks) — versioned with an edit trail and per-version diff, full-text search, moderation (draft → approve), and category facets. Served to humans in the UI and to agents over MCP.
- **MCP control plane.** Agents discover services, register projects, fetch credentials, read standards/guidance/recipes, and request resources — all as MCP tools. The remote server lives at `/mcp`; auth is a user PAT (acts across every project you own/manage) or a per-project key.
- **Surfaces.** CLI, MCP, embedded Web UI, REST + GraphQL.

## Design principles

- **One static binary.** Rust, embedded UI. `docker run` and you're productive in an afternoon.
- **SQLite by default, Postgres opt-in.** Identical behavior on both — the SQLite path needs zero external services; switch `--database-url` to Postgres to scale out.
- **Agents first, humans second.** Every capability is an MCP tool, not just a UI/REST route.
- **Hub, not world-builder.** Serve guidance and data, mint credentials, let the agent act. Resist bespoke orchestration.
- **Open core, honest seams.** The governance spine is OSS. Enterprise features (SAML/SCIM, multi-tenant, SIEM streaming) sit behind clean trait seams.

## Quickstart

```sh
# SQLite, no external services, no login on loopback — browse the UI at http://localhost:8080
cargo run -p frontkeep -- serve --database-url sqlite://frontkeep.db
# (set FRONTKEEP_DEV_INSECURE=1 to skip auth on loopback while exploring)
```

Then connect an agent to the **MCP front door** — the Getting Started tab in the UI mints a Personal Access Token and shows the exact snippet for Claude Code, Codex, and Cursor. Or drive it from the CLI — the same binary, [installed](docs/docs/install.md) and authenticated with that same PAT (`frontkeep login`, or `FRONTKEEP_PAT`/`FRONTKEEP_URL`):

```sh
frontkeep project register --name "My Service" --owner you@corp.example \
  --manager you@corp.example --group platform --classification poc   # the gate; mints proj-YYYY-NNNN
frontkeep project credential proj-2026-0001                             # mint the project's gateway key
frontkeep catalog services                                              # what's provisionable
frontkeep cost report --by group                                        # spend rolled up by dimension
frontkeep validate services/s3-bucket/service.yaml                      # offline manifest check
```

> **Upgrading from Asgard?** The rename is non-breaking: legacy `ASGARD_*` env
> vars are still read, `asg_…`/`asg_pat_…` tokens still authenticate, and an
> existing `asgard.yaml` is still auto-loaded. The one thing that changed default
> is the bare SQLite filename (`asgard.db` → `frontkeep.db`); point at the old one
> with `--database-url sqlite://asgard.db` if you relied on it. Full checklist:
> [Upgrade — Asgard → Frontkeep](docs/docs/upgrade.md).

See [`docs/docs/`](docs/docs/) for [installing the CLI](docs/docs/install.md), [using the CLI](docs/docs/cli.md), the [onboarding loop](docs/docs/onboarding-loop.md), [connecting an agent](docs/docs/connect-agent.md), [deploying Frontkeep](docs/docs/deploy.md), [inference backends](docs/docs/inference-backends.md), and the [architecture](docs/docs/architecture.md).

## Status

Pre-1.0. The OSS governance core is the focus; architecture decisions are recorded in [`RFC-0001`](RFC-0001-entity-model.md) and [`RFC-0002`](RFC-0002-policy-and-sandbox.md), and the prioritized backlog lives in [`plans/ROADMAP.md`](plans/ROADMAP.md).

## License

Apache 2.0. Contributions under the [CLA](CLA.md). See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
