# Agent orchestration

You have a task that involves a model. Should you build a scripted pipeline that calls the model at one step? A single agent that loops with tools until it's done? A team of specialized agents handing work between them? The answer determines how much complexity, cost, and operational pain you're signing up for — and the default answer is almost always simpler than people reach for.

## The spectrum

```
Scripted pipeline   →   Single agent   →   Multi-agent

(every step is          (one agent loops      (several specialized
 code; the model         with tools until      agents handing off
 is a function call)     it's done)            work to each other)
```

**Default to the leftmost option that works.** Every step to the right buys you flexibility you may not need and pays for it in complexity, cost, and new failure modes. The discipline is to move right only when the option to your left has demonstrably failed.

## Scripted pipeline — start here

When the steps of your workflow are known and stable, write them as ordinary code and call the model where you need it:

```python
def process_doc(doc_uri):
    text = extract_text(doc_uri)
    summary = summarize(text)          # a gateway call
    entities = extract_entities(text)  # another gateway call
    store(doc_uri, summary, entities)
```

The control flow is your code. Each model call is a function. This is easy to test, easy to debug, easy to cost, and it never surprises you at 2 a.m. Use it whenever you can write down "step 1, step 2, step 3" without ambiguity about the order.

The fact that there's a model in the loop does not make this "an agent," and that's a feature. Most production AI workloads are pipelines with a model at one or two steps, and they should stay that way.

## Single agent — for genuinely dynamic workflows

When the order of operations depends on what gets discovered along the way — in ways you can't enumerate in advance — a fixed pipeline can't express the work. That's when you switch to an agent: a loop that looks at the current state, decides the next action, takes it, and repeats.

```python
while not done:
    step = model.plan(state, available_tools)
    if step.is_done:
        done = True
    else:
        result = tools[step.tool](step.args)
        state = update(state, step, result)
```

Use a single agent when the sequence of steps depends on intermediate results, the available tools are well-defined, and the success criteria are explicit. The key word is *explicit*: an agent without a clear definition of "done" will loop, drift, or quit early.

## Multi-agent — only when truly needed

Multiple agents, each with a specialized role, communicating with each other. The common shapes:

- **Planner / executor.** One agent plans; another executes individual steps; the planner reviews each result.
- **Generator / critic.** One generates, another critiques, the generator revises against the critique.
- **Specialist team.** Researcher, coder, reviewer — with a coordinator routing work between them.

Multi-agent is *significantly* more complex than single-agent, and the costs are real:

- Communication overhead — every agent re-reads the shared state and reasons about it from scratch.
- Coordination failure modes that don't exist with one agent: agents disagreeing, oscillating between positions, deadlocking, or politely agreeing with each other into a corner.
- Roughly 2-5x the token cost of an equivalent single-agent setup, because each agent does its own full reasoning pass.

Reach for multi-agent only when a single agent has *demonstrably* failed and the failures are coordination-shaped — the agent forgets earlier context, conflates roles it's trying to juggle, or can't critique its own work. **Don't reach for multi-agent because it sounds impressive.** Nine times out of ten, wanting multi-agent is a symptom of an under-designed single agent.

## Patterns that actually earn their keep

### Generator plus critic, with different models

The most reliable single improvement over a vanilla agent. Use a strong model as the generator and a *different* model as the critic. The critic reviews each output, the generator revises. The reason it works is that a model is bad at seeing its own blind spots — a second model with different training catches what the first one is structurally unable to notice. Cost is roughly 2x a single-model agent; worth it on hard tasks, overkill on easy ones.

### Plan up front, then execute against the plan

Have the agent write a numbered plan first, commit it to the agent's state, and reference it on every subsequent step. Steps that don't match the plan get flagged. This cuts drift dramatically — most agent wandering comes from the model losing the thread of what it was trying to accomplish.

### Fan-out for independent sub-tasks

When a task decomposes cleanly into N independent pieces — research M documents, analyze K configurations — spawn sub-agents in parallel, let the parent wait and aggregate. The cost and step caps should apply to the whole tree, not per agent, or your budget guard means nothing.

### Rolling-window memory plus summary

Most agents drown themselves in their own context. The pattern that works: keep the last K turns verbatim, plus a running summary of everything older. When the buffer fills, summarize the oldest turns into the running summary and drop them. The agent keeps recent detail and long-range gist without carrying every raw tool output forever.

## Anti-patterns

- **Infinite loops.** The agent calls a tool, dislikes the result, calls it again with slightly different arguments, forever. *Fix:* a hard step cap is non-negotiable. Set it, enforce it at the runtime, and treat hitting it as a failure to investigate.
- **Pre-empting the model.** Hardcoding most of the workflow and using the model as a slot-filler. If you're doing this, you wanted a scripted pipeline; the agent machinery is pure overhead.
- **"AI" for things that aren't AI.** A regex, a SQL query, or a lookup table would do the job faster, cheaper, and deterministically. The single most efficient pattern is the right tool, not "add a model."
- **Memory bloat.** Every tool result gets shoved into context; twenty steps in, the context is enormous and the agent is slow and expensive. *Fix:* store structured memory — the fields that matter — not raw tool dumps.
- **The sycophantic critic.** A critic that always says "looks great" because the generator's framing implies approval is expected. *Fix:* a neutral, explicit rubric and a different model than the generator.

## Picking tools

An agent is only as good as its tools. A few principles:

- **High-leverage and focused.** "Search the docs" is one clean tool. "Search, then summarize, then categorize" is three tools the agent has to learn to compose, and it will compose them wrong. Keep each tool doing one thing.
- **Cheap and fast.** Every tool call costs wait time and budget. A 10-second tool is fine for occasional use; one called every step makes the whole agent feel broken.
- **Structured returns.** A tool that returns JSON is immediately usable. A tool that returns free text forces the next model call to parse it, burning tokens and inviting errors.
- **Safe by default.** Any side-effecting tool — deletes, sends, writes — needs a human-in-the-loop check for anything past a proof of concept. An agent with an unguarded destructive tool is an incident waiting for a slow afternoon.

## Observability

You cannot operate an agent you can't see. Log every step: the plan, the tool called, the arguments, the result, the token cost. Route model calls through the gateway so spend is attributed per project and every completion lands in the audit log. When an agent misbehaves — and it will — the difference between a five-minute fix and a lost afternoon is whether you can replay exactly what it did. Treat the step log as a first-class artifact, not an afterthought.

## See also

- `autonomous-research-loops` — a worked application of orchestration to iterative experiment loops.
- `writing-good-evals` — agent reliability is mostly downstream of eval quality.
- `choosing-a-model` — generator, critic, and summarizer steps each want a different tier.
- `long-context-and-caching` — for managing the context an agent accumulates over a long run.
