// Frontkeep default policies (RFC-0002 Part A). Decision-relevant facts are carried
// on the request `context` so the engine needs no preloaded entity store for the
// common path; richer entity-attribute policies can be added later. Cedar is
// default-deny, and `forbid` overrides any `permit`.

// Kill switch: a killed project blocks everything (overrides every permit).
forbid (principal, action, resource)
when { context has project_killed && context.project_killed == true };

// Budget exhaustion blocks model invocation.
forbid (principal, action == Action::"invoke", resource)
when { context has budget_exceeded && context.budget_exceeded == true };

// A model may only be invoked for a data class on its allowlist.
permit (principal, action == Action::"invoke", resource)
when {
  context has data_class &&
  context has model_data_classes &&
  context.model_data_classes.contains(context.data_class)
};

// Deploying anything that touches "restricted" data requires approval from the
// security group (carried as an obligation via the @approval annotation).
@approval("group:default/security")
permit (principal, action == Action::"deploy", resource)
when { context has data_class && context.data_class == "restricted" };

// Deploying non-restricted workloads is allowed without approval.
permit (principal, action == Action::"deploy", resource)
when { !(context has data_class) || context.data_class != "restricted" };

// Provisioning a "review"-tier resource (high blast radius / materially
// expensive, e.g. a database or a dedicated account) requires platform approval,
// even when the data class alone would auto-approve. The resource type's tier is
// carried as context.provision_tier by the provisioning layer.
@approval("group:default/platform")
permit (principal, action == Action::"deploy", resource)
when { context has provision_tier && context.provision_tier == "review" };

// Access grants ride the "deploy" action: granting a consumer access to a target
// in your OWN project auto-approves for non-restricted data (and inherits the
// restricted-data security approval above). Your own resources are self-service —
// authority over both resources is enforced before the request is filed, and
// cross-project sharing is out of scope. The access level rides as context.level
// (present only on grants). To gate grants specifically (enterprise tightening),
// uncomment and adapt — the annotated permit adds an approval obligation on top of
// the auto-permit, so it composes without editing the rules above:
//
// @approval("group:default/platform")
// permit (principal, action == Action::"deploy", resource)
// when { context has level && context.level != "read" };

// Classification promotion (WS2). Rust computes the evidence facts
// (target_classification / evidence_complete / has_exception / risk_accepted)
// and carries them on the request context; routing lives here so an operator
// re-routes by editing policy, not by recompiling. The @approval groups are
// placeholders the org maps to its real platform/security groups.
//
// Light Operational: machine-driven — auto-permit only when evidence is complete
// and no exception signal fired.
permit (principal, action == Action::"promote", resource)
when {
  context has target_classification &&
  context.target_classification == "light-operational" &&
  context has evidence_complete && context.evidence_complete == true &&
  context has has_exception && context.has_exception == false
};

// Wide Operational (and any Light target with an exception) routes to platform
// for a structured cross-functional review — never auto-approved.
@approval("group:default/platform")
permit (principal, action == Action::"promote", resource)
when {
  context has target_classification &&
  (context.target_classification == "wide-operational" ||
   (context has has_exception && context.has_exception == true))
};

// Critical Path requires formal security-group approval and recorded risk
// acceptance.
@approval("group:default/security")
permit (principal, action == Action::"promote", resource)
when {
  context has target_classification &&
  context.target_classification == "critical-path"
};

// A critical-path promotion cannot complete without recorded risk acceptance
// (forbid overrides permit), satisfying the doc's explicit-risk-acceptance rule.
forbid (principal, action == Action::"promote", resource)
when {
  context has target_classification &&
  context.target_classification == "critical-path" &&
  (!(context has risk_accepted) || context.risk_accepted != true)
};

// A review-extension beyond the automatic allowance is a human decision
// (platform review).
@approval("group:default/platform")
permit (principal, action == Action::"extend", resource);

// A budget increase above the classification's self-service ceiling is a human
// decision (platform review). The registry files this request only when the
// requested amount exceeds the ceiling, so it always routes to a human.
@approval("group:default/platform")
permit (principal, action == Action::"set_budget", resource);

// Reads are always allowed.
permit (principal, action == Action::"read", resource);

// Decommissioning is allowed for the resource owner.
permit (principal, action == Action::"decommission", resource)
when { context has is_owner && context.is_owner == true };

// Approving a request is allowed for designated approvers.
permit (principal, action == Action::"approve", resource)
when { context has is_approver && context.is_approver == true };
