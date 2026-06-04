# RAG patterns

Retrieval-augmented generation is the default move when a model needs to know things it wasn't trained on: your team's documentation, your project's history, a domain corpus it has never seen. Done well, RAG is the single most useful technique for grounding a model in your reality. Done poorly, it's a confident-sounding way to be wrong — the model produces fluent answers built on whatever happened to get retrieved, and nobody can tell the difference until it matters.

This guide is about doing it well: knowing when to reach for it, where the leverage actually is, and how to make the output defensible.

## When RAG helps

- The answer requires facts from a specific corpus the model wasn't trained on.
- The corpus changes often enough that fine-tuning would be a treadmill.
- You need citations and provenance — the answer has to be traceable to a source.
- The corpus is genuinely too large to fit in a context window (well past a few hundred thousand tokens).

## When RAG doesn't help

- **The corpus fits in context.** Just pass it. With prompt caching, repeated queries over the same context are cheap, and you skip an entire layer of retrieval infrastructure and its failure modes. See `long-context-and-caching`. RAG was the only option when context windows were a few thousand tokens; that era is over.
- **The task is reasoning, not recall.** Retrieval doesn't make a model reason better. If the work is "think hard about these facts" rather than "find these facts," RAG adds nothing.
- **The corpus is small and structured.** A dozen entities with attributes is a database lookup, not an embedding search. Don't reach for vectors when a `WHERE` clause is exact and free.
- **There's no single correct answer.** If the user wants brainstorming, you want generation, not retrieval of "the" answer.

## The pipeline

```
query → embed → retrieve top-k → rerank → assemble context → model call
```

Every stage has its own failure modes, but the leverage is not evenly distributed. The biggest wins almost always come from the middle three stages — retrieve, rerank, assemble — not from the embedding model at the front or the generation model at the back. People spend their energy in exactly the wrong place.

## Chunking matters more than the embedding model

The standard advice is "use the best embedding model." That's the wrong thing to optimize. The biggest single lever in a RAG system is **how you split the corpus into chunks before embedding anything.**

- **Chunks too small** (sentence-level) strip away the context that makes a passage meaningful. Retrieval surfaces fragments that mean nothing on their own.
- **Chunks too large** (whole-document) are heterogeneous — the embedding averages everything together and washes out the specific passage that would have matched the query.
- **The right size** depends on the material. Prose and docs: a few hundred to ~800 tokens with some overlap. Code: function-level chunks. Tables: row-level chunks that carry their column headers along.

A reliable rule: a chunk should be the smallest unit that's meaningful on its own. If a reader could glance at the chunk and know what it's about without seeing the rest of the document, you've sized it right. Chunking is cheap to re-do, so iterate on it before you touch anything else.

## The retrieval step

Top-k retrieval by vector similarity alone is rarely good enough for production. The fix is a **rerank** step: retrieve a wide net of candidates (say 50-100) by cosine similarity, then rerank them with a model that scores each `(query, chunk)` pair directly and far more accurately than a similarity score can. Reranking costs more compute per pair, but you only run it on the candidates that survived the first pass, so total cost stays moderate while quality jumps. A typical setup retrieves 50, reranks down to the top 5-10, and assembles those.

If the retriever keeps missing documents you know are in the corpus, consider hybrid retrieval — combining keyword (BM25-style) search with vector search. Vector search is weak on exact terms, identifiers, and rare tokens; keyword search covers exactly that gap.

## Assembling the context

Once you have your top chunks, you have to put them in front of the model. Options, roughly in order of sophistication:

1. **Concatenate naively.** Glue the chunks together into the prompt. It works, but the model can't tell you which chunk supports which claim.
2. **Number and label.** Prefix each chunk with a source marker — `[Source 1, doc=X, section=Y]`. Now the model can cite, and you can validate.
3. **Compress first.** If the chunks are long and the question is narrow, summarize each chunk with a cheap model before assembling. Saves tokens at the cost of latency and a little fidelity.
4. **Restructure.** Sometimes the right move is to extract specific fields from each chunk and hand the model structured JSON instead of prose.

For most systems, labeled concatenation (option 2) is both the cheapest and the best-performing default. Reach for the fancier options only when a specific failure mode tells you to.

## Citations and validation

Every RAG response that asserts a fact should be traceable to a source chunk. This is non-negotiable if the system is anything more than a demo. The pattern:

1. In the prompt, instruct the model to attach a source marker to every claim — "for each claim, cite the source id in brackets, e.g. `[Source 1]`."
2. In a post-processing step, validate three things: every claim carries a citation; every citation maps to a chunk that was actually retrieved; and the cited chunk genuinely supports the claim.

That post-processing step is the line between a real product and a plausible-looking toy. Without it, your system's claim to correctness is "trust me, I read some stuff." With it, every assertion has a paper trail.

## The biggest failure modes

- **The retriever misses the right document.** If the relevant context never gets retrieved, no model can save the answer. *Symptom:* the system says "I don't have information about that" on questions you know are in the corpus. *Fix:* better chunking, hybrid retrieval, or a stronger embedding model — in that order of likely payoff.
- **The right document is retrieved but the model ignores it.** *Symptom:* the answer contradicts the retrieved context. *Fix:* require citations in the prompt, lower the temperature, or switch to a model that follows context more faithfully (smaller models often struggle here).
- **Conflicting documents get retrieved.** *Symptom:* the model picks one arbitrarily or blends them into something incoherent. *Fix:* instruct the model to surface the conflict explicitly rather than resolve it silently; tighten metadata filtering at retrieval time.
- **The corpus is stale.** *Symptom:* confident answers based on outdated material. *Fix:* sync the index on a schedule, and carry a `last_modified` field in the metadata so the model can hedge on old documents.

## A worked example

A "what does our documentation say about X" service, end to end:

```python
# Route everything through the gateway.
client = gateway_client(...)

# 1. Retrieve a wide net from the project's vector store.
hits = vector_store.search(query=user_query, max_results=50)

# 2. Rerank the candidates and keep the best handful.
reranked = rerank(user_query, hits)[:10]

# 3. Assemble with source labels so the model can cite.
context = "\n\n".join(
    f"[Source {i + 1}, doc={h.metadata['doc_id']}]\n{h.content}"
    for i, h in enumerate(reranked)
)

# 4. Answer, constrained to the sources, with citations required.
answer = client.chat(
    model="<mid-tier>",
    messages=[
        {"role": "system", "content":
            "Answer using only the provided sources. Cite each claim like "
            "[Source N]. If the sources don't contain the answer, say so."},
        {"role": "user", "content": f"Sources:\n\n{context}\n\nQuestion: {user_query}"},
    ],
)

# 5. Validate before you trust the output.
validate_citations(answer, reranked)
```

The `validate_citations` step at the end is illustrative but essential: it's where you confirm every claim is grounded in something that was actually retrieved. Skip it and you've built a demo.

## See also

- `long-context-and-caching` — the alternative to RAG when the corpus fits in context.
- `choosing-a-model` — for picking both the embedding model and the generation model.
- `writing-good-evals` — RAG quality is groundedness plus answer correctness, not retrieval recall alone.
- `cost-optimization` — RAG-vs-context is one of the larger cost levers you control.
