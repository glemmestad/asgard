# Terraform / IaC add-on

Conventions for any infrastructure-as-code, layered on top of
`.agent/STANDARDS.md`. Pulled in when the repo contains Terraform/HCL.

## Build & checks
- The done bar for Terraform: `terraform fmt -check -recursive`, `terraform validate`, and a reviewed `terraform plan`. All green, and the plan reviewed by a human, before you claim done.
- Read the plan before any apply. Never `apply -auto-approve` outside a CI pipeline that produces and stores the plan artifact. Never `-target=` to dodge unrelated drift.
- `plan` showing changes you didn't intend is drift — investigate it, don't paper over it.

## State & secrets
- No secrets in state, variables, or outputs that land in plaintext. Mark sensitive inputs and outputs `sensitive = true`, and source secret *values* at runtime through the approved secret path — never hardcode them.
- Remote state with locking. Local state is for throwaway experiments only. Check in the backend config; never the credentials.
- Treat the state file as sensitive data — it can contain resolved secret values.

## Modules
- Every variable and output has a `description` and a `type`. Resources are named `<type>_<purpose>`, not `aws_s3_bucket.bucket`.
- Module interfaces are contracts: don't rename a variable or drop an output without a version bump. Pin shared modules to a tag, never to a moving branch.
- Take existing infrastructure (VPC, subnets, security groups) as inputs rather than creating new networks per module. Don't spin up a parallel VPC when one was handed to you.

## Cost attribution
- Apply the standard tag set to every provisioned resource: project id, owner, and data classification. Resources missing these fail cost reconciliation and show up as untagged spend.
- Request infrastructure through Frontkeep's catalog when it exists there — the provisioner enforces tagging, classification, and audit for you.

## Idempotency & destroy-safety
- A second `apply` with no config change must produce no changes. If it doesn't, your config is non-deterministic — fix it.
- Guard stateful resources (databases, buckets with data) with `prevent_destroy` or equivalent. A `plan` that proposes to destroy data is a stop-and-ask, not a rubber stamp.
- Prefer `for_each` over `count` for "zero or one"; `count` re-indexing silently destroys and recreates.
