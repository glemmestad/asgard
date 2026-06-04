# Picking a classification

Every project on the control plane carries a data classification, and that single field does more work than almost anything else you set. It determines which models you're allowed to call, how heavy the infrastructure under you is, how much human approval a change requires, and how much operational rigor you're signing up for. Getting it right is the difference between a project that's appropriately governed and one that's either reckless or strangled by ceremony it doesn't need.

There are two opposite mistakes, and most people are prone to one or the other. This guide is about choosing deliberately and avoiding both.

## The governing rule

**Classify by the most sensitive data the system touches — anywhere, at any point.**

A classification isn't an average and it isn't a vibe about how important the project feels. If the system ingests, stores, processes, logs, or even transiently handles data at a given sensitivity, the whole system inherits that sensitivity. A pipeline that's 95% public data and 5% confidential data is a confidential system, because a leak doesn't care about the 95%. The most sensitive thing that flows through any part of it sets the floor for all of it.

This is why the rule is "most sensitive," not "mostly." You classify for the worst case the system can actually encounter, because that's the case that determines the blast radius when something goes wrong.

## What each tier implies

The exact tier names and gates live in your deployment's classification policy; the *shape* is consistent. As the classification rises, three things tighten together:

- **Model and infrastructure floor.** Higher classes restrict you to models cleared for that data — which at the top end means private or self-hosted models rather than any public API — and to infrastructure with stronger isolation, encryption, and network controls. The gateway enforces the model side of this directly: a model not cleared for your class is denied, and that denial is the policy working, not a bug.
- **Approval and review.** Lower tiers can be largely self-service or bot-reviewed. Higher tiers require human sign-off — and the highest require multiple, escalating approvers — precisely because that's where real risk concentrates. The human gate is deliberate, not friction to route around.
- **Operational rigor.** Higher tiers come with expectations about availability, on-call, monitoring, and the documentation of risk and blast radius before a change ships. A proof of concept has none of this; a critical-path system has all of it.

Lower tiers are not lower-quality. They're *appropriate-effort*. A small project that lives happily at the lowest tier for a year because nobody needed to promote it is a success, not a thing that fell behind.

## The decision, walked through

Start at the lowest tier and only move up when a real-world responsibility forces you to. Roughly:

1. **Is this just you, or a small handful of people experimenting?** The lowest tier. Stay there until something genuinely changes.
2. **Are real users depending on it for their daily work?** Promote to the operational tier — it now needs an owner and a baseline of reliability.
3. **Are people outside your immediate team depending on it?** Promote again — the blast radius of a failure now extends past people you can tap on the shoulder.
4. **Is it on the critical path to a commitment the business has made?** The top tier, with the full weight of approval and on-call that implies.

And cutting across all of that: **if the data is sensitive, the data wins.** A tiny experiment that happens to process confidential data is classified for that data regardless of how few people use it. Sensitivity sets the floor; scale and dependency raise it from there.

## The cost of over-classifying

Over-classifying feels safe and isn't free. A higher classification provisions heavier, more expensive infrastructure, narrows your model options (often pushing you onto slower or pricier cleared models you don't need), and drags every change through approval gates that exist for risk you don't actually carry. The result is a project that's slower to iterate, more expensive to run, and more annoying to change — all to guard against exposure that was never on the table.

Over-classification also has a corrosive second-order effect: when the ceremony doesn't match the real risk, people start treating *all* the ceremony as theater and learn to route around it. A control that's applied where it isn't needed teaches people to ignore it where it is.

So don't promote for the wrong reasons: not to make the project record sound more important, not to unlock a model you've convinced yourself you need (solve *that* problem on its merits), and not to dodge a budget cap (higher tiers don't remove caps, they just set them higher).

## The cost of under-classifying

Under-classifying is the more dangerous error, because its cost is hidden until it detonates. A system handling confidential data under a proof-of-concept classification is running confidential data through models that were never cleared for it, on infrastructure without the required isolation, with changes shipping unreviewed. Nothing looks wrong — right up until that data ends up somewhere it should never have been, and now it's an incident, possibly a reportable one, and the cleanup costs orders of magnitude more than classifying correctly would have.

Under-classification is seductive because it's faster *today*. It removes a review, unlocks a cheaper or quicker model, skips the heavier infrastructure. You feel the savings immediately and pay the bill later, all at once, at the worst possible time.

## How to promote

Promotion is a deliberate, reviewed act. Edit the project's classification on its record and submit it as a change, with evidence in the description that the target tier's gates are actually met. The higher the target, the more approval it requires — and that human review is by design. Tier promotion is exactly where real risk shows up, so it's exactly where a human, not an automated check, should be standing.

## See also

- `choose-a-model-by-data-class` — how your classification constrains which models the gateway will let you call.
- `handling-secrets` — higher classes raise the bar on how secrets and access are handled.
- `cost-optimization` — over-classifying provisions heavier infrastructure than the work needs, and you pay for it.
