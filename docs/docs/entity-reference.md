---
sidebar_position: 4
title: Entity reference
---

# Entity reference

Every manifest shares one envelope and is validated against a JSON Schema in
[`schemas/`](https://github.com/asgard/asgard/tree/main/schemas).

```yaml
apiVersion: asgard.dev/v1
kind: Agent                 # one of the kinds below
metadata:
  name: code-reviewer       # unique within (kind, namespace)
  namespace: default
  title: Code Reviewer
  labels: { team: platform }
  tags: [review, ci]
spec: { ... }               # kind-specific
relations:
  - { type: dependsOn, target: tool:default/github }
```

An **EntityRef** is `kind:namespace/name` (e.g. `group:default/platform`).

## Kinds

| Kind | Source | spec highlights |
|---|---|---|
| `Agent` | `agent.yaml` | `owner`, `model`, `prompt`, `tools[]`, `dataClass`, `project` |
| `Prompt` | `prompt.yaml` | `owner`, `template`, `variables[]`, `evals[]`, `version` |
| `Tool` / `MCPServer` | `mcp.yaml` | `owner`, `transport`, `endpoint`, `scopes`, `visibility`, `allowedConsumers` |
| `Model` | registry | `provider`, `route`, `dataClassAllowlist[]`, `costPer1kIn/Out` |
| `Dataset` | `dataset.yaml` | `owner`, `classification`, `uri`, `purpose` |
| `Eval` | `eval.yaml` | `owner`, `suite`, `thresholds`, `gatingTier`, `target` |
| `Project` | registry | stable `id` (`proj-YYYY-NNNN`), `owner`, `classification`, `budgetUsd`, `lifecycle` |

## Data classes

`public` · `internal` · `confidential` · `restricted`. A model may only be
invoked for a data class on its `dataClassAllowlist`; the gateway enforces this
via the policy engine.

## Lifecycle

`Project` and `Agent` move `active → decommissioned | archived`. Terminal states
retain audit + cost history.

See the golden-path template under
[`templates/code-review/`](https://github.com/asgard/asgard/tree/main/templates/code-review)
for a complete, valid example produced by `frontkeep agent new --template code-review`.
