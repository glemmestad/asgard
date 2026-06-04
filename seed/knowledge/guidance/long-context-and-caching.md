# Long context and caching

Modern models accept enormous context windows — hundreds of thousands, sometimes a million-plus tokens. You have a corpus. It fits. Should you just put the whole thing in the prompt?

Sometimes yes, sometimes no, and the difference is worth real money. This guide is about choosing between stuffing context, retrieving a subset (RAG), and a hierarchical middle ground — and about using prompt caching so that the "stuff it in context" option is as cheap as it can be.

## The three regimes

| Approach | When | Cost shape |
| --- | --- | --- |
| **Put it all in context** | Corpus fits in ~150-180k tokens and you query it many times | Higher per-call; near-zero setup; cheap reuse with caching |
| **RAG** (retrieve a subset, then pass) | Corpus is too big, or you query rarely | Lower per-call; higher setup (build the index); recurring index cost |
| **Hierarchical** (summary index, then drill down) | Corpus is huge *and* you need precision | Highest setup; multiple model calls per query |

The default, when the corpus is small enough, should be **put it all in context.** RAG was mandatory back when context windows were a few thousand tokens. That constraint is gone, and a lot of RAG infrastructure exists today only out of habit. Don't build a retrieval pipeline for a corpus that fits in a prompt.

## When stuffing it in context works

- Your corpus is under roughly 150-180k tokens, leaving comfortable room for the prompt and the response.
- You'll query the same context repeatedly. Prompt caching makes every call after the first dramatically cheaper.
- The information is dense and interconnected, so the model benefits from reasoning across all of it at once rather than over a retrieved slice.
- Your latency budget can absorb a slow first call. The first call over a large context is slow (tens of seconds is normal); cached calls are fast.

For a team-documentation corpus — dozens to a couple hundred documents — this is very often the right answer, and it lets you skip the entire RAG stack.

## When RAG wins

- The corpus is genuinely large: millions of tokens, thousands of documents.
- The signal you need is localized — one or two relevant documents per query, not the whole corpus.
- The corpus changes constantly, so re-stuffing the full context on every query would cost more than maintaining an index.
- You need citations and provenance. RAG gives you "this chunk supports this claim"; stuffing the whole corpus gives you nothing to point at.

See `rag-patterns` for the implementation.

## When hierarchical wins

For very large corpora where neither pure long-context nor pure RAG is good enough, retrieve in two stages:

1. **Build a summary index.** Generate a one-paragraph summary per document and embed the summaries.
2. **Retrieve at the summary level.** A query first finds the top-k *documents* via summary search.
3. **Drill down.** Re-retrieve at the chunk level within those documents, or just pass the matching documents whole into context.

This gives better precision than naive chunk-level vector search and costs far less than putting the entire corpus in context on every query.

## Prompt caching economics

Prompt caching is the lever that makes long context affordable. The idea: when you reuse the same large prefix across many calls, the provider caches that prefix after the first call, and subsequent calls that share it pay a small fraction of the input cost. The savings are large — repeat calls over a cached corpus commonly cost a fraction of the uncached price.

To benefit, structure the prompt so the **stable, cacheable content comes first** and the variable content comes last:

```python
messages = [
    {"role": "system", "content": SYSTEM_PROMPT + LARGE_CORPUS},  # stable → cacheable
    {"role": "user",   "content": current_query},                 # varies per call
]
```

Two rules that people break constantly:

- **Never put variable content inside the cacheable region.** A timestamp, a request id, a per-user token anywhere in the prefix busts the cache, and you silently pay full price on every call while believing you're cached.
- **Caches expire after a short idle period.** Sporadic, low-frequency query patterns don't benefit — by the time the next query arrives, the cache is gone. Caching pays off for bursts and steady traffic, not for one query an hour.

When you route through the gateway, spend is attributed per project, so you can confirm on the Cost tab that your cache hit rate is actually saving you money rather than just believing it is.

## Compressing a corpus that almost fits

If the corpus is just over the line, compress before you reach for RAG:

- **Drop boilerplate.** Headers, footers, navigation, repeated legal text — usually 10-30% of the tokens, none of the signal.
- **Summarize low-value sections.** Replace long peripheral sections with bullet summaries; keep verbatim only the parts queries actually hit.
- **Extract structured data.** Convert tables to compact JSON; it's denser and the model parses it more reliably than ASCII tables.

Each of these costs a one-time model call to produce. If you'll reuse the compressed corpus many times, that cost amortizes to nothing. If you'll use it once, skip the compression and just RAG.

## Failure modes specific to long context

- **Lost in the middle.** This is the big one. Models advertise huge context windows, but recall degrades for facts buried in the *middle* of a long context — information at the very start and very end is recalled far more reliably. Don't assume that because a fact is "in context" the model will use it. *Test for it:* insert a known fact at varied positions and query for it. *Mitigate it:* put the most important material at the beginning and end of the context, not the middle.
- **Latency.** The first call over 100k+ tokens is slow even on the fastest providers. If user-facing latency matters, RAG is often faster end to end despite the extra retrieval hop, because it sends the model far less to chew on.
- **Cost growth.** A large context costs real money per call, multiplied by your query rate. Cache it or RAG it; don't pay full freight on every call.
- **Cache invalidation.** As above — idle expiry quietly turns your "cached" workload back into a full-price one. Monitor the actual cost, don't assume.

## The decision in one paragraph

For a small corpus (under ~150k tokens) you query often: stuff it in context and cache the prefix. For a large corpus: RAG. For a very large corpus that also needs precision: hierarchical retrieval. Default to the simplest option that fits your size and query rate, and only add infrastructure when the simpler option's failure mode actually shows up in your evals — not because the fancier option sounds more serious.

## See also

- `rag-patterns` — the retrieval regime in detail.
- `choosing-a-model` — models differ in both context size and per-token cost at long context.
- `cost-optimization` — caching and the RAG-vs-context choice are among the biggest cost levers.
