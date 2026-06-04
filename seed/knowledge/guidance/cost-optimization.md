# Cost optimization

Your project's bill is higher than it should be, or you want to keep it from getting there. This is the playbook, ordered by leverage — because the most common way people waste effort on cost is fiddling with small line items while a large one runs unattended.

## Measure before you optimize

The first rule of cost work: **don't optimize what you haven't measured.** Intuition about where the money goes is wrong more often than it's right. On the control plane, spend is attributed per project, so the Cost tab breaks your bill down by service, by resource within each service, and over time. Look there first. The biggest line item is usually one of three things — compute, model tokens, or storage and egress — and you should fully optimize the biggest before touching anything smaller. Saving 80% of a line item that's 5% of your bill is a rounding error you spent an afternoon on.

## The biggest levers, in order

### 1. Model choice

This is almost always the largest single lever for an AI workload, and it's the cheapest to pull. A cheap-tier or mid-tier model is commonly 10-50x cheaper than a flagship and, for most tasks, within a couple percent on quality. Defaulting every call to the flagship "to be safe" is the most expensive habit in AI engineering. Default to a smaller model and escalate only when you've *watched* it fail on a real eval. See `choosing-a-model` for the decision tree and the cheap A/B test that settles it.

### 2. Prompt caching

If you send the same large prefix — a system prompt, a corpus, a long set of instructions — across many calls, cache it. Subsequent calls that share the cached prefix pay a fraction of the input cost. The catch is that the cacheable content must come first and stay byte-for-byte stable; a single variable token in the prefix busts the cache and you pay full price while believing you're saving. See `long-context-and-caching` for how to structure it.

### 3. Batching

Work that doesn't need a real-time response should go through batch processing rather than synchronous calls. Batch inference is typically meaningfully cheaper than the synchronous price for the same model — often roughly half. If a job can tolerate minutes or hours of latency, batching it is free money. The only cost is the discipline to separate "needs an answer now" from "needs an answer eventually."

### 4. RAG vs. context

Stuffing 50k tokens of context into every call, when each call only needs a small slice of it, is paying for tokens the model doesn't use. If the relevant signal is localized, retrieve the subset (RAG) instead of passing the whole corpus every time. Conversely, if the corpus is small and you query it constantly, RAG infrastructure is overhead you don't need — cache it in context instead. The right choice depends on corpus size and query rate; see `long-context-and-caching` and `rag-patterns`.

### 5. Idle compute

Compute you provisioned and aren't using is the silent budget killer. Services left running at full scale overnight, dev machines with no auto-shutdown, databases billing 24/7 for a project that's active two hours a day. Scale services to zero when idle if your platform supports it, set auto-shutdown on anything interactive, and right-size everything — most workloads run fine far below the instance size people reflexively pick. None of this costs anything to set up, and all of it reverses in one command.

### 6. Storage lifecycle

Storage is cheap per gigabyte and expensive in aggregate when nobody ever deletes anything. Set lifecycle policies that move cold data to colder, cheaper tiers and expire genuinely dead data. Don't disable these to "see the old data again" — restore it deliberately when you need it instead of paying to keep everything hot forever.

## Attribution: know whose cost it is

You can't manage a bill you can't decompose. Route every model call through the gateway so spend lands on the right project automatically, tag resources consistently, and provision through the catalog rather than around it. The moment cost shows up under "miscellaneous" or on someone's personal account, it stops being manageable and becomes a mystery that someone inherits later. Attribution isn't bureaucracy — it's the precondition for every other optimization on this list.

## Budgets and caps

Set a monthly budget cap on anything that can run away. A loop calling a flagship model over long context can burn through hundreds of dollars in an afternoon before anyone notices. A budget cap turns "we got a surprise invoice" into "the job stopped and alerted us," which is the difference between a postmortem and a non-event. Set the cap a little above your expected spend, watch the per-project dashboard, and treat hitting the cap as a signal to investigate, not as an inconvenience to raise.

## What not to do

- **Don't bypass the gateway to "save" cost.** A direct provider key is not cheaper — it just removes the budget cap, the audit trail, and the per-project attribution, and moves the spend somewhere nobody is watching. The savings are imaginary; the loss of visibility is real.
- **Don't go around the catalog with personal accounts.** Personal-account resources are untagged, unmanaged, and become someone else's cleanup problem the day you change teams. They're also where surprise bills hide.
- **Don't disable lifecycle and caps to make a number go away.** That doesn't reduce cost, it just stops you from seeing it. The bill keeps running; you've only blinded yourself to it.

## See also

- `choosing-a-model` — the single biggest cost lever, in detail.
- `long-context-and-caching` — caching economics and the RAG-vs-context tradeoff.
- `rag-patterns` — retrieving a subset instead of paying for the whole corpus every call.
- `picking-a-classification` — over-classifying provisions heavier, more expensive infrastructure than the work needs.
