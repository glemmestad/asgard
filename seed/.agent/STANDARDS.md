# Engineering Standards

These are the enterprise standards that code, infrastructure, and documentation in this repository should follow. The **live source of truth** is the Frontkeep `get_standards` / `list_standards` MCP tools; this file is the offline copy. When the two differ, the live standards win.

They are directionally correct defaults. A team may tighten them locally; a local tightening wins over a global default, never a loosening of a security rule.

## Code

- **Pick the right language and stay consistent with the repo.** Use the language the repo already uses; don't introduce a second stack without a clear reason.
- Use the standard formatter and linter for the language, with no bespoke style configs. Run them before you consider a change done.
- Type-annotate public interfaces. Treat type errors as build failures for new code.
- Write the **minimum code that solves the problem.** No speculative abstractions, no configurability nobody asked for, no error handling for impossible cases.
- Match local conventions even where you'd personally do it differently. Stay surgical: every changed line should trace to the task.

## Testing

- Unit-test the logic you write. A change without a test for its new behavior is not done.
- Prefer tests that exercise real behavior over mocks for anything security- or data-sensitive (don't mock the policy decision, don't mock the gateway in a guardrail test).
- Reproduce a bug with a failing test first, then make it pass.
- Never weaken, skip, or delete a test to make a build go green.

## CI

- CI is the finish line, not the push. Work is done when CI passes, not when the branch is pushed.
- Every pull request runs: format check, lint, type check, unit tests, and the eval gate where one applies.
- Don't merge red. Don't merge on a manual override unless a human explicitly authorizes it.

## Commits

- **Conventional commits.** `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, optionally scoped (`feat(gateway): ...`).
- First commit on a branch may be multi-line (one summary line, then bullets). Every commit after is single-line.
- Don't mention AI tooling in commit messages or history.
- The "why" goes in the commit message or the PR description, not in narrative code comments.

## Dependencies

- Don't add a dependency without a clear reason and a look at what's already vendored.
- Pin/lock versions. Commit the lockfile.
- Prefer well-maintained, widely-used libraries over novel ones for load-bearing paths.

## Security basics

- See [`SECURITY.md`](SECURITY.md) for the full set; the essentials:
- **No secrets in the repo, ever** — not in code, not in `.env`, not in a manifest or commit message. Fetch secret values at runtime through the approved secret path.
- **All model calls go through the Frontkeep gateway** — the project key (minted with `gateway_credential`) against the gateway endpoint (`POST /api/gateway/chat`). Never call a provider SDK directly.
- Least-privilege access; no wildcard grants without explicit, written justification.
- No public, unauthenticated endpoints above a proof-of-concept tier.

## Documentation

- Short over long. Concrete over abstract. No marketing voice — write what the thing does.
- Code examples in docs must actually work, or be clearly marked illustrative.
- **No narrative comments in code.** Comments explain a non-obvious *why* (a constraint, a workaround, an invariant), never *what* the code is doing or the history of how it got there.

## What "high-rigor" means

When a change touches security, policy, identity, data classification, or anything production-grade, the bar rises:

- The plan and the blast-radius/risk are written down in the PR.
- Validation evidence (test output, eval verdict) is attached.
- A second reviewer beyond the author signs off.

This file is the floor. For high-rigor work, default upward.
