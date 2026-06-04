# Writing good evals

An eval set is a regression test for a system whose behavior you can't fully specify. It exists to answer one question reliably: did this change make the system *worse*, or just *different*? Without one, you're shipping on vibes — a passing demo, a screenshot, a "looks good to me." With a good one, you can promote a model, rewrite a prompt, or swap an agent config and know within minutes whether you broke something.

A bad eval set is worse than none, because it manufactures false confidence: it passes the things that are quietly wrong and fails the things that are fine. Most of the craft of evals is in avoiding that.

## What an eval is, and what it isn't

**An eval is** a collection of test cases. Each case has an input and a way to grade the output, and the whole set runs programmatically to produce pass/fail or a numeric score. You can wire it into CI and re-run it on every change.

**An eval is not** a list of prompts the team likes, a screenshot of a good run, or a notebook nobody ever reopens. The test is simple: if you can't run it the same way twice and get a comparable verdict, you don't have an eval — you have a memory of a good day.

On the control plane, evals are what gate tier promotion: a project can't move up a classification tier until its eval verdict clears the bar. The gate reads the verdict; it does not write your cases. Whether the cases are any good is entirely on you.

## What makes a case useful

Three properties separate a useful case from a decorative one.

1. **It would catch a regression you'd actually care about.** The most valuable cases come from real failures you've seen in production, edge cases the system claims to handle, and adversarial inputs designed to break it. Happy-path cases — the ones where everything is easy and the answer is obvious — are nearly worthless, because they pass by accident and tell you nothing when they pass.
2. **The expected output is well-defined.** "Looks good" is not an expected output. "Returns valid JSON with a `summary` key whose value mentions at least one of the source's three claims" is. If you can't write down what correct looks like, you can't grade it, and the case will drift toward whatever the model happens to produce.
3. **The grading is unambiguous.** Two reviewers handed the same output and the same rubric should arrive at the same grade. If they wouldn't, your rubric is too loose, and the eval's verdict is noise dressed up as a number.

## The three flavors of scorer

There are broadly three ways to grade an output. Most real eval suites combine several.

| Flavor | Use when |
| --- | --- |
| **Heuristic** — exact match, regex, embedding similarity | The output is deterministic or has a clear format with some flexibility. Use embedding similarity sparingly: it is *generous* and lets a great deal of subtly-wrong output through. |
| **Code** — your own grading function | The correctness criterion is domain-specific and you can express it in code: schema validity, numerical tolerance, a structural invariant, a parser that either succeeds or doesn't. |
| **LLM-as-judge** — a separate model scores against a rubric | The thing you're grading is a quality judgment about natural language: helpfulness, groundedness, tone, completeness. The most common flavor for content, and the one most often done badly. |

Picking the wrong flavor is the number-one reason evals fail to catch regressions. The classic mistake is using embedding similarity to grade code or structured data, where it happily passes output that's broken in ways exact-match or a code scorer would have caught instantly. Match the scorer to the shape of the correctness criterion.

## Writing a good LLM-as-judge rubric

When you use an LLM-as-judge, **the rubric is the eval.** A vague rubric produces a vague judge, and a vague judge produces scores that look precise and mean nothing. A good rubric has four properties:

- **It scores on multiple named axes, not one.** "Groundedness, completeness, brevity, format" each scored and then weighted is far more informative than a single "quality: 7." The axes also tell you *what* regressed, not just that something did.
- **Each axis has concrete, anchored descriptions per score level.** Not "score 1-10 on accuracy" but "score 4: factual claims are mostly right but include one or two hedged statements with no supporting evidence." The judge needs to know what each number means.
- **It includes worked examples.** Show the judge what a 9 looks like and what a 4 looks like. Models calibrate to examples far better than to abstract descriptions.
- **It asks for reasoning before the score.** Force the judge to write its justification first, then emit the number. Judges that produce the score first and rationalize after are measurably more biased.

Here is a rubric for grading a technical summary, as an illustration:

```yaml
rubric:
  axes:
    - name: groundedness
      weight: 0.4
      criteria: |
        10: every factual claim is supported by the source document.
         7: mostly supported; one or two minor extrapolations.
         4: significant unsupported claims.
         1: largely fabricated.
    - name: completeness
      weight: 0.3
      criteria: |
        10: every key point from the source is captured.
         7: most captured; one or two secondary points missing.
         4: a central point is omitted.
         1: misses the main thesis.
    - name: brevity
      weight: 0.2
      criteria: |
        10: tight; every sentence carries information.
         7: mostly tight; some repetition.
         4: substantial padding.
         1: rambling.
    - name: format
      weight: 0.1
      criteria: |
        10: matches the requested format exactly.
         5: roughly follows the format.
         1: ignores the format.
  output_format: "Reason about each axis in turn, then emit the scores as JSON."
```

Route the judge model through the gateway like any other call, and pick the judge deliberately: a judge that's weaker than the system it's grading will rubber-stamp output it can't actually evaluate.

## Building the dataset

For most projects, **20-50 well-chosen cases beat 1,000 randomly collected ones.** A small, sharp dataset that targets the failure modes you care about will catch more real regressions than a large, diffuse one full of happy-path noise. Draw your cases from these sources, in descending order of value:

1. **Real production failures.** When the system breaks in the wild, capture the input that broke it and add it as a case. This is the single most valuable kind of case, because it's a regression you've already paid for once and never want to pay for again.
2. **Edge cases from a domain expert.** Ask whoever knows the domain best: "what's the strangest input you've seen this thing get?" Write down their answers.
3. **Adversarial inputs.** Prompt-injection attempts, deliberately ambiguous queries, malicious payloads. If your eval never exercises these, you simply don't know whether the system handles them.
4. **Happy path.** Last, not first. Include a handful to confirm baseline functionality, but if your dataset is mostly happy-path cases, it's mostly decorative.

Store the dataset as a versioned asset alongside the system it tests, so you can re-run a historical version against a new model or prompt and get a true apples-to-apples comparison.

## How often to re-run

For evals that gate something real — production promotion, a tier change — re-run on every change to the thing being gated: the model, the prompt, the agent config. Wire it into CI so it's not a step anyone can forget. For background quality monitoring, a weekly or monthly scheduled run is enough to catch upstream drift, like a provider quietly updating a model version under a stable id.

## When NOT to invest in an eval

Evals are work, and there are cases where that work is premature or wasted:

- **You can't yet state a release criterion.** If you can't finish the sentence "before I ship this, I need to know that ___ is true," you're not ready to write the eval. Build the thing first, develop intuition for how it fails, *then* encode that intuition as cases.
- **The system runs once.** A genuine one-shot job doesn't need a re-runnable regression test. One careful manual review does the job.
- **A human is already mandatorily in the loop.** If every output gets reviewed by a person before it matters, the human *is* the eval. A second programmatic eval is duplicate effort unless it's catching something the human reliably misses.

## See also

- `choosing-a-model` — the A/B test that picks a model is just an eval run on two candidates.
- `rag-patterns` — RAG quality is measured by groundedness and answer correctness, not retrieval recall alone.
- `autonomous-research-loops` — a research loop is only as honest as the eval it grades itself against.
- `agent-orchestration` — agent reliability is, to a first approximation, a function of eval quality.
