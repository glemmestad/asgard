---
sidebar_position: 3
title: Architecture
---

# Architecture

A Cargo workspace of focused crates. `storage` is the lowest layer; `catalog`
owns the entity model and is depended on by the service crates; surfaces (`api`,
`cli`, `mcp`) sit on top, wired together by the `asgard` binary.

```
frontkeep (binary: serve / mcp / cli)
  └─ api (REST + GraphQL, axum + async-graphql)
       ├─ catalog   (entities, JSON-Schema validation, Git ingestion, reconcile)
       ├─ gateway   (provider routing, virtual keys, budgets, guardrails, audit)
       ├─ workflow  (request → approve → fulfill state machine)
       ├─ eval      (suite runner, scored verdict, merge gate)
       ├─ identity  (local users + OIDC; SAML/SCIM stubbed)
       ├─ policy    (Cedar PolicyEngine trait)
       └─ runtime   (Runtime trait: gVisor / container / local-process)
  └─ storage (one Db over SQLite + Postgres via sqlx Any)
```

## Storage parity

One `Db` abstracts SQLite and Postgres via sqlx's `Any` driver. Schema is
portable SQL with app-side ids (UUID), RFC3339-text timestamps, JSON-as-TEXT and
INTEGER booleans, so both backends behave identically by construction. `?`
placeholders are rewritten to `$1..$n` for Postgres at the call site
(`Db::q`). The e2e suite runs green on **both** stores.

## Policy & sandbox

See [RFC-0002]. Cedar is the in-tree default policy engine; the `PolicyEngine`
trait is the seam an OPA/Rego backend would implement. The `Runtime` trait keeps
gVisor as the documented default with container and local-process fallbacks;
per-invocation wall-time/step/budget caps are enforced by the supervisor, not by
user code.

## Entity model

See [RFC-0001]. Typed entity graph, Backstage-shaped where it helps, reconciled
from Git with pull-based semantics so deletes propagate (avoiding catalog drift).
Frontkeep emits `catalog-info.yaml` for Backstage interop (one-way).

[RFC-0001]: https://github.com/asgard/asgard/blob/main/RFC-0001-entity-model.md
[RFC-0002]: https://github.com/asgard/asgard/blob/main/RFC-0002-policy-and-sandbox.md
