# RFC-0001 — Entity Model

- Status: **Accepted** (drives the `catalog` and `storage` crates)
- Scope: the typed entity graph, its on-disk (YAML-in-Git) form, its stored form, reconciliation semantics, and Backstage/CMDB interop.

## 1. Goals & non-goals

**Goals**

- A typed entity graph that is *Backstage-shaped where it helps* (so federation and existing `catalog-info.yaml` muscle memory transfer) and *CMDB-compatible* (CI-like records + a relationship graph) so Frontkeep can stand alone or later sync to a CMDB.
- **Source of truth is YAML files in Git repos** (federated, dev-led ownership), reconciled into a catalog store.
- **Pull-based reconciliation so deletes propagate** — explicitly avoiding Backstage's well-known catalog-drift failure where removed files leave ghost entities.
- Identical behavior on SQLite and Postgres (storage is an implementation detail behind a trait).

**Non-goals (this RFC)**

- Bidirectional CMDB sync, multi-tenant partitioning, and entity-level fine-grained RBAC — these are enterprise seams (§5 of the brief), stubbed elsewhere.
- A general-purpose query language beyond the search surface defined in the `api` crate.

## 2. The entity envelope

Every entity, regardless of kind, shares one envelope. This is deliberately close to Backstage so `catalog-info.yaml` round-trips, but the `spec` is ours.

```yaml
apiVersion: frontkeep.dev/v1
kind: Agent                         # one of the kinds in §3
metadata:
  name: code-reviewer               # unique within (kind, namespace)
  namespace: default                # default "default"
  title: Code Reviewer              # human label (optional)
  description: Reviews PRs for ...   # optional
  labels: { team: platform }        # selectable key/values
  annotations: {}                   # non-selectable metadata (origin info lives here too)
  tags: [review, ci]
spec:
  # kind-specific; validated against schemas/<kind>.schema.json
  owner: group:default/platform     # EntityRef — owner of record
  ...
relations:                          # optional explicit relations; most are derived from spec
  - type: dependsOn
    target: tool:default/github
```

**EntityRef** is the canonical cross-reference, Backstage-compatible:

```
[<kind>:][<namespace>/]<name>          e.g. agent:default/code-reviewer, group:default/platform
```

Parsing rules: kind and namespace are optional in references and default to a context kind / `default` namespace. The fully-qualified form `kind:namespace/name` is the storage key (`uid` is a separate internal id; see §5).

## 3. Kinds

| Kind | Source of truth | spec highlights |
|---|---|---|
| `Agent` | `agent.yaml` in repo | `owner`, `model` (ref), `prompt` (ref), `tools` ([ref]), `policy` (inline or ref), `dataClass`, `project` (ref) |
| `Prompt` | `prompt.yaml` | `owner`, `template`, `variables` ([{name,type,required}]), `evals` ([ref]), `version` |
| `Tool` / `MCPServer` | `mcp.yaml` | `owner`, `transport` (stdio/http/sse), `endpoint`, `scopes`, `auth`, `tools` ([name]), `visibility`, `allowedConsumers` |
| `Model` | central registry | `provider` (openai/anthropic/bedrock/...), `route`, `dataClassAllowlist` ([class]), `contextWindow`, `costPer1kIn/Out` |
| `Dataset` | `dataset.yaml` | `owner`, `classification`, `uri`, `purpose` (train/eval/rag) |
| `Eval` | `eval.yaml` | `owner`, `suite`, `thresholds` ({min_pass_rate,min_avg_score}), `gatingTier`, `target` (ref) |
| `Project` | central registry | **stable id** (see §6), `owner`, `manager`, `classification`, `budgetUsd`, `lifecycle` |
| `Component`/`API`/`System` | repo | Backstage-compatible passthrough for federation |
| `Group`/`User` | repo / identity provider | `members`, `email`, `parent` |

`Tool` and `MCPServer` are the same kind family; `MCPServer` is a `Tool` whose `transport` is an MCP transport. Modeling them together keeps the agent's `tools: [ref]` list uniform.

## 4. Lifecycle

`Project` and `Agent` carry a `lifecycle` field:

```
active  ──▶ decommissioned        (intentional shutdown; resources torn down)
        └─▶ archived              (read-only retention)
```

`decommissioned` and `archived` are **terminal** and **retain audit + cost history**. Lifecycle is orthogonal to `classification` — a decommissioned project keeps its last classification for historical reporting. Transitions are audited (actor, time, reason) and are themselves policy-gated (see RFC-0002).

## 5. Stored form

The store (SQLite or Postgres, behind the `Store` trait) holds three core tables:

- **`entities`**: `uid` (ULID, internal stable id), `kind`, `namespace`, `name`, `title`, `description`, `spec` (JSON), `metadata` (JSON), `lifecycle`, `origin_repo`, `origin_path`, `origin_commit`, `content_hash`, `seen_at`, `created_at`, `updated_at`. Unique on (`kind`,`namespace`,`name`).
- **`relations`**: `from_uid`, `type`, `to_ref` (string EntityRef), optionally resolved `to_uid`. Relations are re-derived from `spec` on every upsert (a relation is never authoritative on its own).
- **`audit_log`**: `id`, `ts`, `actor`, `action`, `entity_ref`, `trace_id`, `outcome`, `reason`, `data` (JSON). Append-only; queryable/exportable. This is the spine the gateway, workflow, and policy layers all write to.

`spec`/`metadata` are stored as JSON text (SQLite `TEXT`, Postgres `JSONB`) so the schema is portable and queries behave identically. Search uses generated/extracted columns + LIKE/`to_tsvector` only where the backend supports it, falling back to a portable path.

## 6. Stable project id

A `Project` gets a human-readable **stable id** of the form `proj-YYYY-NNNN` (year + zero-padded monotonic counter), minted once at registration and never reused or renamed. It is the join key for cost attribution (`project=<id>` tag), audit, and classification, and it survives the entity through `decommissioned`/`archived`. The internal `uid` (ULID) is separate and used for relational integrity. (The predecessor's `<prefix>-YYYY-NNNN` convention validated this pattern; Frontkeep generalizes the prefix to `proj`.)

## 7. Reconciliation (the anti-drift design)

Ingestion is **pull-based** per source repo:

1. A `SourceProvider` (GitHub, GitLab) enumerates candidate manifest files (`agent.yaml`, `prompt.yaml`, `mcp.yaml`, `eval.yaml`, `dataset.yaml`, and `catalog-info.yaml`) at a ref.
2. Each file is parsed → validated against its JSON Schema → normalized into an entity with `origin_{repo,path,commit}` and a `content_hash`.
3. A reconcile pass diffs the **full set observed for that source** against the **set currently stored for that source**:
   - new/changed (`content_hash` differs) → upsert + audit `entity.upserted`,
   - **present in store but absent from the latest observation → removed** (soft-delete with audit `entity.removed`), which is precisely how deletes propagate.
4. Relations are re-derived on every upsert; dangling `to_ref`s are allowed (recorded, resolved lazily) so ingestion order doesn't matter.

Reconciliation is idempotent and source-scoped: re-running with no changes is a no-op; removing a repo from config removes its entities on the next pass. Target reconciliation latency < 5 min (tracked in BUILD_LOG).

## 8. Backstage & CMDB interop

- **Backstage:** *emit* `catalog-info.yaml` for any entity (one-way). No ingest of Backstage's processing pipeline, no bidirectional sync (§3.10 of the brief). Frontkeep *can* read a repo's existing `catalog-info.yaml` as one more manifest source, but it owns its own `spec`.
- **CMDB:** the `entities` + `relations` tables are CI-records + relationship-graph shaped, so a future connector can map entities→CIs and relations→CI relationships without remodeling.

## 9. Validation

Each kind has a JSON Schema in `schemas/<kind>.schema.json`. Manifests are validated at ingestion and by `frontkeep validate` / `frontkeep agent new`. Schemas are the readable contract operators inspect before deploying (brief §3.5). Validation failures are surfaced as PR comments by the Git integration and never silently dropped.
