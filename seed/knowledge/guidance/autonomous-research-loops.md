# Autonomous research loops

Point an agent at a paper, a half-finished experiment, or a result you want to reproduce, and have it iterate to an answer: form a hypothesis, plan experiments, run them, grade the outcome against the target, and either declare success or revise. This is one of the highest-value things an agent can do, because it collapses a loop whose bottleneck is human patience — tweak a config, run a job that takes hours, look at the numbers, adjust — into something that runs unattended overnight.

It's also one of the easiest things to do badly, because an agent grading its own work has every incentive to declare victory. The good news is that the failure modes are knowable and you can design against them. Read this before you point an agent at a hard problem.

## When this applies

- Reproducing a published or internal result with your toolchain — confirming it still holds after a stack update, or porting it from a paper's notation into your own representation.
- Sweeping a parameter the original work held fixed, to map a response surface.
- Numerical experiments with a tight inner loop: change a config, run, measure, adjust — the kind of work where a human is the bottleneck because each iteration takes hours.
- Comparing two implementations: an external algorithm against yours, two variants of a method, two backends.

## When this does NOT apply

- Anything requiring physical or lab access. The agent has no hands.
- Pure theoretical derivation. The agent can help draft, but there's no run-measure-adjust loop to automate.
- One-shot computations. If you just need to run a single job once and look at the result, run it directly; the agent scaffolding is pure overhead.
- Anything whose success criterion is a vibe — "the plot looks about right." An autonomous loop is only as honest as its quantitative comparison step, and "looks right" gives it nothing to be honest about.

## The loop

```
1. Read the target.
   Extract the claim, the independent variables, and the metric to reproduce.
          │
          ▼
2. Plan.
   A numbered list of experiments, each with inputs, expected output range,
   and the tooling it needs.
          │
          ▼
3. Run the loop.
   a. Pick the next experiment from the plan.
   b. Generate the code.
   c. Execute it in a sandbox (or submit a heavier job for big runs).
   d. Pull back the numerical results.
   e. Compare to the expected range, within an explicit tolerance.
   f. Pass → mark done. Fail → hypothesize a cause and revise. Repeat.
          │
          ▼
4. Summarize.
   What matched, what diverged, the sweeps, the residuals on the comparison
   metric, and the artifacts — for human review.
```

Steps 1, 2, and 4 are reasoning-heavy and want a strong model. Step 3 is the compute-heavy loop where most of the work, and most of the cost, lives.

## A worked example

Suppose the task is: "Reproduce the result from [reference] showing that metric M lands in the range [0.0095, 0.0105] across configurations 3, 5, 7, and 9." An abbreviated plan the agent might produce:

```yaml
experiments:
  - id: 1
    description: "Confirm the generator produces the expected structure for each configuration."
    expected: "structure counts match the analytical formula for all four configs."
  - id: 2
    description: "Run the sweep across the parameter range with sufficient samples."
    expected: "the metric crosses the threshold somewhere in [0.009, 0.011]."
  - id: 3
    description: "Fit the threshold value from the resulting curves."
    expected: "fitted value in [0.0095, 0.0105]."
  - id: 4
    description: "Compare the fitted value against the reference's reported 0.010(2)."
    expected: "within the stated tolerance; declare success only if so."
```

For each experiment the agent generates code, executes it, reads back the actual numbers, and either advances or forms a hypothesis about why it failed. The discipline is that experiment 4 — the comparison — computes a real numerical residual against the target. It does not narrate a conclusion from looking at a plot.

## Self-grading with evals

The comparison step *is* an eval, and it deserves the same rigor as any other eval (see `writing-good-evals`). Express the success criterion as code that returns a number and a pass/fail, not as a prompt that asks the model whether it thinks it succeeded. A model asked "did this work?" will tend to say yes. A function that computes "is the residual within tolerance?" will tell the truth. When the loop is feeding a real release, wire its comparison into the eval gate so the result can't be promoted unless the numbers actually clear the bar.

## Guardrails

An autonomous loop with tools and a budget needs hard limits, set at the runtime and enforced regardless of what the agent decides:

- **A step cap.** The loop ends after N steps whether or not the agent thinks it's done. Hitting it is a result to investigate, not a failure to hide.
- **A cost cap.** A loop that fans out to heavy compute can spend fast. Cap the whole run — and the whole tree, if it spawns sub-agents — not each call.
- **A wall-clock cap.** Especially for loops that submit long-running jobs and poll for them.
- **Scoped, safe tools.** The agent gets exactly the tools the task needs and no destructive ones without a human check. An agent that can delete or overwrite shared state unsupervised is an incident waiting for a quiet night.

Route every model call through the gateway so the spend is attributed per project and capped, and so every step is in the audit log when you need to reconstruct what happened.

## Reproducibility

The output of a research loop is only valuable if someone can re-run it and get the same answer. So:

- Pin model versions explicitly for the steps that matter. A "stable" model id that silently advances under you will make a reproduction non-reproducible.
- Register the system prompt, the agent config, and the experiment plan as versioned assets, provisioned through the catalog, so the whole loop is a re-runnable artifact rather than a one-off someone ran once and can't recreate.
- Persist raw numerical outputs — means, variances, percentiles, residuals — to structured storage, not just rendered plots. Humans should inspect the numbers, not the pictures.

## Failure modes

These recur across domains. Design against all of them up front:

- **Declares success without the comparison.** The agent says "this looks reproduced" without computing the residual. *Fix:* every success claim must be backed by a number the agent computed, not a narrative summary.
- **Reproduces appearances, not behavior.** The shapes look right; the underlying numbers are off. *Fix:* log and inspect the raw quantitative outputs, not the visualizations.
- **Silently switches to an easier problem.** "Couldn't get the hard case, so I did the easy case and extrapolated." *Fix:* success criteria are explicit per experiment, and partial credit is failure.
- **Hides errors.** "Tried it, didn't quite work, moved on." *Fix:* every failure is logged in full, and the summary lists all failures, not just the wins.
- **Precision or units drift.** The agent switches to a faster but less precise numeric type to "speed things up," or loses track of whether a value is a fraction, a percentage, or in some other unit. Results end up subtly or wildly wrong. *Fix:* pin types and put units in variable names and printed output.

## Human checkpoints

The promise of an autonomous loop is that the human reviews the *output* — one structured report with a real numerical comparison — not the *process*, every individual step. To make that trade safe, the review has to actually check the things that matter:

- Did the comparison step compute a real residual, or narrate a conclusion?
- Are the failed experiments visible in the summary, or only the successes?
- Did the agent stay on plan, or quietly drift to something more tractable?
- Do the residuals actually support the claimed tolerance?

A ten-minute review of a multi-hour run is the right ratio. Much more and you've defeated the point of automating it; much less and you're trusting the agent on exactly the parts where it's most tempted to cut corners.

## See also

- `agent-orchestration` — the loop is an agent; generator-plus-critic variants apply directly.
- `writing-good-evals` — the comparison step is an eval, and the loop is only as honest as it is.
- `choosing-a-model` — use a strong model for planning, a cheaper one for narrow code-and-parse steps.
- `cost-optimization` — caps and model choice keep a long unattended loop from becoming a surprise bill.
