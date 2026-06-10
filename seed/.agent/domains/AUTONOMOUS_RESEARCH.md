# Autonomous research domain overlay

Pulled in when the work is an autonomous or agentic research loop: experiment
management, parameter sweeps, scientific computing, or reproducible analysis.
The risk is generating a mountain of plausible-looking results that nobody can
trust or reproduce — speed without rigor is just faster wrong answers.

## Reproducibility is the deliverable
- Every result carries its provenance: the code version, the inputs/dataset version, the parameters, and the random seed. A result you can't regenerate from that record is not a result.
- Pin the environment (dependencies, interpreter, hardware where it matters). "Worked on my machine last week" is not reproducibility.
- Separate the experiment definition from its outputs. Re-running the same definition must produce the same artifact, or the difference must be explained.

## Honest method
- State the hypothesis and the success criterion before the run, not after looking at the data. Don't retrofit the question to the answer.
- Report negative and null results. A sweep that found nothing is information; hiding it wastes the next run.
- Distinguish exploratory from confirmatory work. Don't present a number you went hunting for as if it fell out of a pre-registered test.

## Sweeps & cost
- Size a sweep before launching it: estimate the run count, cost, and wall-time, and bound it. An unbounded autonomous loop is how budgets evaporate overnight.
- Make runs idempotent and resumable; checkpoint long jobs. A crash at hour nine shouldn't cost you the whole sweep.
- Route compute, datasets, and any model calls through Frontkeep so they're attributed, budgeted, and auditable. Model calls go through the gateway, not a direct SDK.

## Agentic loops
- Cap the loop: max iterations, budget, and wall-time enforced outside the agent's own logic. Expect the circuit breaker to trip a runaway and handle it as a normal condition.
- Keep a human-readable trace of what the loop decided and why, so a result can be audited after the fact.

## Done bar
A research change is done when every result is reproducible from a recorded
(code, data, params, seed) tuple, the environment is pinned, the hypothesis and
success criterion were stated up front, negative results are reported, sweeps are
bounded/resumable and cost-attributed through Frontkeep, and autonomous loops have
enforced caps and an auditable trace.
