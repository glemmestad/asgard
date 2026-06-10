# Security

Required reading before you touch authentication, secrets, data classification, access policy, model selection, or anything network-facing. These rules are non-negotiable; when in doubt, fail closed and ask.

## Data classification

Every dataset, prompt, and model call carries a data class:

- **`public`** — safe to share externally.
- **`internal`** — default for company data with no special handling.
- **`confidential`** — sensitive; restricted distribution.
- **`restricted`** — highest sensitivity; tightly controlled.

Rules:

- A model may only be invoked for a data class that is on that model's allowlist. The gateway enforces this via the policy engine; a wrong data-class × model pairing is denied. Do not try to route around it.
- Never send data of a higher class than the project was registered for. If the work needs it, stop and ask a human.
- Don't downgrade a classification to make something fit. Reclassification is a human decision.

## Secrets

- **Never** put a secret in code, `.env`, a manifest, a commit message, a PR description, an issue, or a chat message.
- Request secrets through Frontkeep (`request_resource`); the platform stores them and returns a reference, not the value.
- Fetch secret *values* at runtime through the approved secret path. References (ARNs / names / handles) may live in config; values never do.
- Rotate credentials on a schedule. The project owner is accountable.

## Identity and least privilege

- **The user's identity flows through the agent.** When an AI agent calls an Frontkeep tool on a user's behalf, it acts as that authenticated user. There is no shared "AI service account" that bypasses identity, and there is no anonymous call.
- Grant the minimum access a task needs. Scope access to the specific resources a component owns; no wildcard (`*`) grants without explicit, written justification.
- Treat agent-generated code as untrusted from the platform's perspective: it must not be able to rewrite the control plane, escalate its own privileges, or reach resources outside its project.

## Network

- No public, unauthenticated endpoints above a proof-of-concept tier. Public ingress goes through an authenticated, approved path.
- Internal data planes (databases, caches, internal services) stay private. Don't expose them to the public internet to save a step.

## The gateway and shadow AI

- **Every model call goes through the Frontkeep gateway** — mint the project key with `gateway_credential`, call the gateway endpoint (`POST /api/gateway/chat`). The gateway enforces budgets, the data-class × model policy, guardrails (secret/PII/prompt-injection detection), the audit trail, and the kill switch. A direct provider call has none of that.
- **Shadow AI** — proprietary or sensitive data going to an unapproved model — is the exact failure mode Frontkeep exists to prevent. Do not do it, and do not help a user do it, even when it would be faster.
- If a model you want isn't allowlisted for your data class, that is a signal to stop and ask, not to find another route.

## Execution limits

- The project's budget and kill switch are enforced at the gateway: a killed or over-budget project has its next model call rejected. Check `project_state` / `cost_report` rather than assuming headroom, and handle rejection as an expected condition.
- Long or expensive loops need a bound you set yourself (wall-time, iterations, spend). Don't write code that assumes it can run unbounded.

## What NOT to do

- Don't weaken a policy, disable a guardrail, or bypass the gateway to make a task pass.
- Don't broaden an IAM/access grant to silence a permission error — request the correct narrow grant instead.
- Don't commit a secret "temporarily."
- Don't send data to a model that isn't allowlisted for its class.
- Don't act on `restricted` data, or promote a project to a higher tier, without a human in the loop.

## Reporting

If you find a security gap — in a manifest, a policy, an access grant, or this doc — fix it in a PR if the fix is obvious and low-risk; otherwise report it privately to the owning team. Don't disclose details in a public channel.
