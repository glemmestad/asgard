# Code Review agent (golden path)

Scaffolded by `frontkeep agent new --template code-review`. This is a complete,
governed starting point: an agent manifest, its prompt, an eval suite that gates
your PRs, and the CI wiring.

## What's here

| File | Purpose |
|---|---|
| `agent.yaml` | The `Agent` entity — model, prompt, tools, data class, owning project. |
| `prompt.yaml` | Versioned `Prompt` template with variables and an eval reference. |
| `eval.yaml` | The `Eval` suite + thresholds that gate merges. |
| `cases.json` | The eval test cases the runner scores. |
| `.github/workflows/frontkeep-eval.yml` | Runs the eval gate on every PR. |

## Use it

1. Register the entities in your Frontkeep catalog (commit these files to a repo
   Frontkeep ingests, or `frontkeep catalog apply`).
2. Get a per-project gateway key: `frontkeep gateway login`.
3. Set `FRONTKEEP_URL` and `FRONTKEEP_TOKEN` repo secrets so the eval gate can run.
4. Open a PR — the gate posts a scored verdict and blocks merge on failure.

The agent calls models **only** through the Frontkeep gateway, so budgets, the
data-class×model policy, guardrails, audit, and the kill switch all apply
automatically.
