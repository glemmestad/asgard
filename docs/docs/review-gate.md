---
sidebar_position: 2.6
title: The review gate (machine-judged promotion)
---

# The review gate

Promoting a project up the classification ladder (POC → Light → Wide → Critical)
is gated. The base gate is a **machine evidence check**: are the required fields
for the target tier present, and did any exception signal fire? On top of that,
Frontkeep can run a **reviewer panel** that *judges* the promotion — including a deep
reviewer that **reads the actual repository** and checks it against your coding
standards.

Reviewers are **escalate-only**: they can return a promotion to the submitter
(`flagged`) or push it to a human, but they can never clear an evidence gap or turn
a human-gated promotion into an auto-approve. The safe state is always
*not promoted*.

## Enablement — the gate is off without a real LLM

The review panel is **disabled unless a real LLM is reachable**. "Real" means one
of:

- an **OpenAI** or **Anthropic** key on the built-in gateway
  (`OPENAI_MASTER_KEY` / `ANTHROPIC_MASTER_KEY`), or
- an enabled **`openai-compatible` gateway module** (LiteLLM, Databricks Model
  Serving, etc. — see [Inference backends](./inference-backends.md)).

The offline **mock model does not count.** With no real model wired, Frontkeep logs

```
review gate DISABLED: no LLM access — set an OpenAI/Anthropic key or enable an
LLM gateway module (LiteLLM/etc.) to turn it on; promotion uses the presence check
```

and promotion falls back to the pure evidence/presence check. When a model is
reachable it logs `review gate enabled (model: …)`.

> **Dev/test only:** `FRONTKEEP_REVIEW_ALLOW_MOCK=1` forces the gate on against the
> mock model. It runs a deterministic stub (no real model call) so the async path
> is exercisable offline. Never use it in production.

## The reviewers

Reviewers are manifests under `reviewers/<id>/reviewer.yaml` (mirroring the service
catalog — drop a file, no recompile), dispatched by `kind`. Two ship built-in:

| Reviewer | kind | Runs | What it judges |
| --- | --- | --- | --- |
| `llm-judge` | `llm-judge` | inline | the **evidence record** for coherence (e.g. `ci_status_url: "N/A"` is a placeholder, not evidence) |
| `code-review` | `code-review` | **async** | the **repository itself**, against your coding standards, depth per tier |

You can add an external reviewer (`kind: webhook`) that delegates to CodeRabbit,
Greptile, or an in-house service, or disable a built-in with an operator overlay
(`FRONTKEEP_REVIEWERS_DIR`).

## The async code reviewer

`code-review` is the deep one. It is **asynchronous and crash-safe**:

1. On `request_promotion`, if the evidence verdict is machine-clean and a
   `code-review` reviewer applies to the target tier, the promotion is submitted,
   parked in a new **`reviewing`** state, and a row is enqueued in `review_jobs`.
   The call returns immediately.
2. A background worker leases the job, has the reviewer **read the repository**
   (over the GitHub/GitLab HTTP API — **no clone, no git binary**), and judge it
   against the standards for the target tier.
3. The worker **finalizes** the promotion: a clean verdict restores the pre-review
   state (auto-approve for a clean Light, the human queue for Wide+); findings
   return it to the submitter as `flagged`.

Because the job state lives in the database (not memory), a **server restart or a
crashed run resumes**: a lapsed lease is reclaimed and the review re-runs. A
promotion never hangs in `reviewing` — after the retry budget is exhausted it
**fails closed to `flagged`** ("review unavailable — fix and retry, or escalate").

The reviewer reads code; it **never executes it** (static review only).

### Repository access — the token Frontkeep holds

The reviewer reads repos with a token Frontkeep holds in its environment, selected by
host:

| Host | Token env | Fallback |
| --- | --- | --- |
| `github.com` / GitHub Enterprise | `FRONTKEEP_GITHUB_TOKEN` | `FRONTKEEP_GIT_TOKEN` |
| `gitlab.com` / self-hosted GitLab | `FRONTKEEP_GITLAB_TOKEN` | `FRONTKEEP_GIT_TOKEN` |

The token needs read access to the repositories you expect to review. Self-hosted
hosts are detected from the URL (`github.<host>` → `…/api/v3`; everything else is
treated as GitLab `…/api/v4`). A `repo_or_source_url` that is a local path is read
directly (used for air-gapped review and tests).

### The standards it judges against

The rubric is your **standards store** (`/api/standards`, operator-editable, seeded
from the engineering standards shipped with the binary). The reviewer is handed the
bodies of the standards configured for the target tier — *you* own the bar.

### Per-tier review depth

Depth scales with the target tier. The shipped defaults:

| Target tier | Standards applied | Effort |
| --- | --- | --- |
| `light-operational` | `coding` | focused |
| `wide-operational` | `coding`, `security` | deeper |
| `critical-path` | `coding`, `security`, `workflow` | exhaustive |

(POC is the floor — nothing promotes *to* POC.) Override any tier in `frontkeep.yaml`:

```yaml
review_depth:
  wide-operational:
    standards: [coding, security, data-handling]
    max_rounds: 10
  critical-path:
    skip: false
    standards: [coding, security, workflow]
    max_rounds: 12
```

`max_rounds` bounds how many `list_files`/`read_file` cycles the model gets to
navigate the repo. Absent tiers keep the default.

## Outcomes a submitter (or agent) sees

`request_promotion` returns a workflow request whose `state` is one of:

- **`approved`** — clean Light, auto-approved (then fulfilled).
- **`reviewing`** — parked in the async code review; poll `promotion_status` or
  re-fetch the request until it resolves. The project UI shows a *Code review in
  progress* card with a Refresh.
- **`requested`** — clean Wide/Critical, awaiting the human approver by tier.
- **`flagged`** — the review found fixable problems (`review_findings` on the
  payload; full verdicts at `GET /api/requests/{id}/reviews`). Self-service: fix
  the evidence/repo and call `request_promotion` again (it **supersedes** the prior
  attempt), or `escalate_promotion` to forward it to a human. An admin can
  authorize a flagged promotion directly at any time.

Every verdict is persisted (`promotion_reviews`) and a flag is audited
(`project.promotion_flagged`).

## Tuning the worker

| Knob | Where | Default |
| --- | --- | --- |
| Worker poll cadence | `review.worker_secs` in `frontkeep.yaml` | 15s |
| On-demand drain (ops/e2e) | `POST /api/reviews/run` | — |

The on-demand endpoint runs the same idempotent pass the periodic loop does — handy
to kick a review immediately or in tests.

## Security note — repo contents are untrusted input

The reviewer feeds attacker-controllable repository text to a model. The design
contains the blast radius: the reviewer's authority is **advisory only**
(escalate-only — worst case is friction, never an unsafe auto-promotion), output is
constrained to a JSON verdict, files/rounds/budget are bounded, and every verdict is
stored for audit. Treat repo text as data, not instructions.
