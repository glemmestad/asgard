# Operating prompts

Reusable prompt patterns for working through Frontkeep. These are the moves you make
on yourself and on the platform — how to plan, self-review, request what you need,
and hand back. Adapt the wording; keep the shape.

## Ask for a plan before non-trivial work
Before editing anything that spans modules, changes a contract, touches a schema,
or has real regression risk, write the plan first:

> Restate the goal in one sentence. List the files/subsystems in scope and what
> changes in each. Name the approach and one alternative you rejected, with why.
> Give an ordered step list. State how you'll verify each step. Call out the
> blast radius and the riskiest assumption. Stop and confirm before editing.

For a small, localized change, an inline plan in the change summary is enough.

## Self-review before claiming done
Run this against your own diff before handing back:

> Review this diff as a skeptical reviewer who didn't write it. For each changed
> hunk: does it trace to the task, or is it scope creep? Is there a test for the
> new behavior? What input breaks it? Did I orphan any code? Did I widen any
> access or touch a security/data-class path? Which checks did I actually run vs.
> assume? List what's unverified.

If the review surfaces a gap, fix it before you say done — don't report it as a caveat you could have closed.

## Request a resource through Frontkeep
When the task needs a model, a secret, storage, or an existing tool, don't wire it
directly — discover and request it through the catalog:

> Check what already exists: `list_services` for provisionable services,
> `mcp_catalog_list` / `skills_catalog_list` for published tools and skills,
> `guidance_list` for playbooks. If a service fits, read its manifest
> (`get_service`) and `request_resource` for the project with the narrowest
> scope the task needs and the correct data classification. If the project
> isn't registered yet, `register_project` first — it's the gate, not a formality.

Never broaden a grant to silence a permission error; request the correct narrow grant. If the resource isn't in the catalog and five projects would need it, surface that as a gap.

## Write the change summary
On completion, fill `.agent/templates/CHANGE_SUMMARY.md`:

> State the behavior change in plain terms (the effect, not a file list). State
> the problem it solves and link the task. List the exact commands you ran and
> what you saw. Give the blast radius, migrations/flags, and rollback. List
> deferred work with the reason it was deferred.

## When to stop and ask
Pause and surface a question — don't silently decide — when the work touches
`restricted` data, would promote the project to a higher tier, needs a model not
allowlisted for the data class, provisions an expensive or production-grade
resource, or would require bypassing the gateway or weakening a guardrail.
