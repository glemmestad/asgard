# Choosing a model

Most projects waste money on inference by reaching for a flagship model where a cheaper one would have done the job perfectly well. A smaller number make the opposite mistake: they grab the cheapest model available because tokens are cheap, and then ship something that quietly fails on the tasks that actually needed reasoning. Both errors come from treating model choice as a single dial — "good" to "cheap" — when it's really a point in a three-dimensional space.

This guide is about how to pick that point deliberately, measure your choice cheaply, and avoid the two default mistakes.

## The three axes

Every model decision trades off three things at once:

1. **Capability.** Can the model do the task at all? This covers reasoning depth, reliability over long multi-step runs, tool-use competence, multimodal handling, and the size of context window it can actually use well (not just the number on the spec sheet).
2. **Cost.** Tokens per dollar. The spread between a flagship and a cheap-tier model is commonly 10-50x on input and output token price, before you even account for the flagship being slower and therefore tying up more of your latency budget.
3. **Data sensitivity.** Where is this model allowed to run, given the data you're sending it? A model not cleared for your data class is simply off the table — no matter how good or cheap it is. On the control plane, the gateway enforces a data-class-by-model allowlist, so this axis isn't advisory; it's a hard gate. See `choose-a-model-by-data-class`.

You are choosing the cheapest model that clears your sensitivity floor *and* meets your capability bar. Not cheaper than that (you'll ship something broken), and not more expensive than that (you'll burn budget for no quality gain).

## The decision tree

Walk it top to bottom and stop at the first match:

1. **Does the request touch sensitive, confidential, or unreleased data?** Your model set is restricted to whatever is cleared for that data class. Pick from inside that set first; cost and capability are secondary considerations *within* the allowed set. For the strictest data, that may mean your organization's private or self-hosted models rather than any public frontier API.
2. **Is the task structured extraction, classification, formatting, routing, or summarizing short text?** A cheap-tier model is almost always enough. These tasks don't reward reasoning depth, and a cheap model is typically within a percent or two of a flagship on them at roughly a tenth of the cost.
3. **Does the task need genuine multi-step reasoning or tool orchestration?** Step up to a mid-tier model. This is where the reasoning gap over cheap-tier models starts to matter, and where a mid-tier model usually lands the sweet spot.
4. **Is the task hard reasoning that the mid-tier model demonstrably fails?** Now reach for a flagship — but only after you've watched the mid-tier model fail, not on a hunch. Mid-tier models surprise people constantly.

The word "demonstrably" in step 4 is load-bearing. The whole point of the tree is that you escalate on evidence, not on anxiety.

## Don't default to the flagship

The single most common cost mistake is "this matters, so I'll use the best model." For the large majority of real tasks, a mid-tier or even cheap-tier model is within a few percent of the flagship on quality while costing an order of magnitude less. Over the life of a deployment, that difference is the difference between a sustainable bill and a budget alert every week.

Before you commit to a flagship, run a quick comparison (below). If the quality gap between the flagship and the next tier down is inside the noise of your eval, take the cheaper one. The flagship is a tool for the tasks that need it, not a status symbol.

## Don't default to the cheap model either

The mirror-image mistake is "tokens are cheap, so I'll run everything on the cheap tier." Some tasks really do need flagship reasoning, and a cheap model on them fails in ways that are expensive to discover later. Watch for these symptoms of under-modeling:

- Long-horizon agent runs that fall apart after five or ten steps — plans that don't hold together, goals that drift.
- Output that reads like a competent first draft when the task needed senior judgment: technically on-topic, substantively shallow.
- Repeated hallucination in a specialized domain, where you have reason to believe a stronger model actually knows the material and the cheap one is guessing.

When you see these, escalate one tier and re-measure. Don't jump straight to the flagship; the next tier up often resolves it.

## Embedding models

Picking an embedding model follows the same three axes, with two wrinkles.

First, the data-sensitivity axis is just as binding here as for generation. If your corpus is confidential, you need an embedding model cleared for that class — which often means a self-hosted or private model rather than a public embeddings API.

Second, and this is the one people forget: **switching embedding models is expensive in a way switching generation models is not.** Your stored vectors are tied to the model that produced them. Changing the embedding model means re-embedding the entire corpus — real compute, real time, and real risk of a half-migrated index serving stale results. Choose deliberately up front, and treat a later switch as a migration project, not a config change.

For most retrieval work, the chunking strategy matters more than the embedding model anyway (see `rag-patterns`), so don't agonize over a one-percent retrieval benchmark difference and then chunk badly.

## Stable model ids vs. pinned versions

Route everything through the gateway and refer to models by their stable, canonical names (think generic tiers — a flagship id, a mid-tier id, a cheap-tier id — rather than a provider's dated version string). The gateway maps each canonical name to a specific upstream version, and advances that mapping when a provider ships a compatible update. Your code keeps calling the same name and doesn't churn every time a vendor releases a point version.

When you genuinely need exact-version reproducibility — for instance, an eval that gates tier promotion on a specific behavior — pin the upstream version explicitly in the request. The audit log records both the canonical id and the upstream version it resolved to, so you can always reconstruct what actually ran.

## Illustrative cost ratios

Absolute prices change constantly and depend on your provider contracts, so don't hardcode a table. What's stable is the *shape* of the spread. As a rough mental model:

| Tier | Relative input cost | Relative output cost | Typical latency |
| --- | --- | --- | --- |
| Cheap-tier | 1x (baseline) | 1x | Fast (sub-second) |
| Mid-tier | ~5-15x | ~5-15x | Moderate (~1-2s) |
| Flagship | ~30-75x | ~30-75x | Slow (several seconds) |

These multipliers are illustrative, not a quote. The point is that the gap between tiers is large and real — large enough that picking the right tier dominates almost every other inference-cost lever you have. Spend is attributed per project on the control plane, so you can watch the actual numbers on the Cost tab rather than guessing.

## How to A/B test a model choice cheaply

You don't need to agonize over model choice in the abstract. Measure it:

1. Assemble 20-50 representative cases. The same dataset you'd build for an eval works perfectly here (see `writing-good-evals`).
2. Run every case through both candidate models via the gateway, with identical prompts and parameters.
3. Score both sets of outputs against the same rubric — an LLM-as-judge scorer, a code scorer, whatever fits the task.
4. Compare not just the average score but the *distribution of failures*. A cheaper model often scores slightly lower on average while failing in ways you can live with. That's a perfectly good outcome — take the cheaper model.
5. If the cheaper model clears your bar, ship it. If it doesn't, try the next tier up and repeat.

This experiment costs you a few dollars in tokens and a half-hour of attention. The model choice it informs runs for the life of the deployment. There is almost no higher-leverage thirty minutes available to you.

## See also

- `choose-a-model-by-data-class` — how the gateway's data-class allowlist constrains your options.
- `writing-good-evals` — the dataset and rubric you'll use for the A/B test.
- `rag-patterns` — for choosing the embedding model inside a retrieval system.
- `cost-optimization` — model choice is the biggest single cost lever; this is the rest of them.
