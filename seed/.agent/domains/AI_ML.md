# AI / ML domain overlay

Pulled in when the work involves models, prompts, training, inference, or evals.
Copy only if the repo actually does ML/agentic work.

## Route every model call through Frontkeep's gateway
- Never wire a provider SDK (OpenAI, Anthropic, …) directly. Mint the project's virtual key (`gateway_credential`) and call the gateway endpoint (`POST /api/gateway/chat`) so budget, policy, guardrails, audit, and the kill switch apply.
- Pick the model by data classification, not habit. Confidential data only goes to models cleared for it (check `list_services` / model metadata).

## Reproducibility
- Pin model versions, prompts, and decoding parameters. A result you can't reproduce is a result you can't trust.
- Version datasets and record their provenance and license. Track which data trained or evaluated which artifact.

## Evals are the test suite
- An agent/prompt/model change is not done without an eval that measures the behavior you changed. Treat the eval like a unit test: it gates the merge.
- Report eval results honestly, including regressions and failure modes. Don't cherry-pick.

## Safety
- Assume prompts can be adversarial (prompt injection, tool-use abuse). Constrain tools, validate tool outputs, and never let model output directly drive a privileged action without a guardrail.
- Don't log raw prompts/completions that may contain secrets or confidential data.
