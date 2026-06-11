## [0.25.1](https://github.com/glemmestad/frontkeep/compare/v0.25.0...v0.25.1) (2026-06-11)


### Bug Fixes

* **provision:** bound terraform connector host disk to stop volume fill ([#50](https://github.com/glemmestad/frontkeep/issues/50)) ([cc1f671](https://github.com/glemmestad/frontkeep/commit/cc1f67189f1d1b7bd3c3e38e6f260a4b2b6ad4fc))

# [0.25.0](https://github.com/glemmestad/frontkeep/compare/v0.24.0...v0.25.0) (2026-06-11)


### Features

* **site:** redesign marketing site in HashiCorp/Terraform/Databricks aesthetic — no gradients, no glow, no reveals ([#49](https://github.com/glemmestad/frontkeep/issues/49)) ([26ba3d7](https://github.com/glemmestad/frontkeep/commit/26ba3d722b1ea48f02099735c8782c6f276871cc))

# [0.24.0](https://github.com/glemmestad/frontkeep/compare/v0.23.0...v0.24.0) (2026-06-11)


### Features

* **site:** land the agent-protagonist positioning across tagline, Why, CTA, and meta ([#48](https://github.com/glemmestad/frontkeep/issues/48)) ([db75326](https://github.com/glemmestad/frontkeep/commit/db75326a67d87cb872cac90a65622240c122fad0))

# [0.23.0](https://github.com/glemmestad/frontkeep/compare/v0.22.0...v0.23.0) (2026-06-11)


### Features

* **site:** rebrand marketing site to Frontkeep — The Agent Control Plane ([#47](https://github.com/glemmestad/frontkeep/issues/47)) ([773d323](https://github.com/glemmestad/frontkeep/commit/773d323a8252041f5cd5a7848be84f0f3faaf726))

# [0.22.0](https://github.com/glemmestad/frontkeep/compare/v0.21.0...v0.22.0) (2026-06-10)


### Features

* rebrand Frontkeep → Frontkeep — The Agent Control Plane ([#46](https://github.com/glemmestad/frontkeep/issues/46)) ([ca331e3](https://github.com/glemmestad/frontkeep/commit/ca331e3cf8b68046e36c03ade51f0695e196941e))

# [0.21.0](https://github.com/glemmestad/frontkeep/compare/v0.20.0...v0.21.0) (2026-06-10)


### Features

* re-trigger release for bootstrap excellence hardening ([#44](https://github.com/glemmestad/frontkeep/issues/44)) ([#45](https://github.com/glemmestad/frontkeep/issues/45)) ([207e13a](https://github.com/glemmestad/frontkeep/commit/207e13a9e598966499038664ca73251471989802))

# [0.20.0](https://github.com/glemmestad/frontkeep/compare/v0.19.0...v0.20.0) (2026-06-10)


### Features

* close MCP-driven provisioning gaps (approvals, network, ownership, ECS health) ([bc6b1f1](https://github.com/glemmestad/frontkeep/commit/bc6b1f1591a92e3314109b2f7127df399d7e60c6))

# [0.19.0](https://github.com/glemmestad/frontkeep/compare/v0.18.2...v0.19.0) (2026-06-09)


### Features

* **cli:** typed subcommands for the 8 remaining MCP tools (MCP/CLI parity) ([#41](https://github.com/glemmestad/frontkeep/issues/41)) ([ca57de4](https://github.com/glemmestad/frontkeep/commit/ca57de435da57e82d8c591d7a3413e90a626bcfc))

## [0.18.2](https://github.com/glemmestad/frontkeep/compare/v0.18.1...v0.18.2) (2026-06-09)


### Bug Fixes

* **provision:** bound terraform subprocess and unstick stale provisioning rows ([#40](https://github.com/glemmestad/frontkeep/issues/40)) ([f900056](https://github.com/glemmestad/frontkeep/commit/f9000564d64ab555d85c430b17435740413d17a0))

## [0.18.1](https://github.com/glemmestad/frontkeep/compare/v0.18.0...v0.18.1) (2026-06-09)


### Bug Fixes

* **cli:** enable rustls on rmcp streamable-http client so CLI reaches /mcp over HTTPS ([#39](https://github.com/glemmestad/frontkeep/issues/39)) ([4382fc2](https://github.com/glemmestad/frontkeep/commit/4382fc29053bc1ba742bd6ba8e76c32ede4c80a7))

# [0.18.0](https://github.com/glemmestad/frontkeep/compare/v0.17.0...v0.18.0) (2026-06-09)


### Features

* **cli,docs:** deploy-image CLI command (MCP/CLI lockstep) + recipe deploy guidance ([#38](https://github.com/glemmestad/frontkeep/issues/38)) ([58b1109](https://github.com/glemmestad/frontkeep/commit/58b110969fa3cfc73027bcd8822a824a2482b244))

# [0.17.0](https://github.com/glemmestad/frontkeep/compare/v0.16.0...v0.17.0) (2026-06-09)


### Features

* **provision:** broker ECR push creds + deploy_image cycle (no AWS creds on runner) ([#37](https://github.com/glemmestad/frontkeep/issues/37)) ([6c6918d](https://github.com/glemmestad/frontkeep/commit/6c6918d0c196b4261bddf7913112d6681502be98))

# [0.16.0](https://github.com/glemmestad/frontkeep/compare/v0.15.0...v0.16.0) (2026-06-08)


### Features

* **provision:** make re-request_resource a true in-place upsert ([#36](https://github.com/glemmestad/frontkeep/issues/36)) ([eb6d2d7](https://github.com/glemmestad/frontkeep/commit/eb6d2d782b9e63859aa10b9874b85695ba3ca7a3))

# [0.15.0](https://github.com/glemmestad/frontkeep/compare/v0.14.0...v0.15.0) (2026-06-07)


### Features

* **provision:** run-log capture + auto-retry with per-service policy ([#35](https://github.com/glemmestad/frontkeep/issues/35)) ([d1ae473](https://github.com/glemmestad/frontkeep/commit/d1ae4739ef9f25537ef8562abdd7a8372f017a79))

# [0.14.0](https://github.com/glemmestad/frontkeep/compare/v0.13.1...v0.14.0) (2026-06-07)


### Features

* **skills:** clean install — server install.sh + raw endpoint + MCP install (no base64) ([#34](https://github.com/glemmestad/frontkeep/issues/34)) ([79a8587](https://github.com/glemmestad/frontkeep/commit/79a858751ce499738b7013b88ac8db61597f644e))

## [0.13.1](https://github.com/glemmestad/frontkeep/compare/v0.13.0...v0.13.1) (2026-06-07)


### Bug Fixes

* **install:** fetch the checksum by its real asset name ([#32](https://github.com/glemmestad/frontkeep/issues/32)) ([0359608](https://github.com/glemmestad/frontkeep/commit/03596086a2d6e36553d30f8b9b2f771690ec7912))

# [0.13.0](https://github.com/glemmestad/frontkeep/compare/v0.12.0...v0.13.0) (2026-06-06)


### Features

* **cli:** PAT-authed CLI with MCP parity + native install ([#31](https://github.com/glemmestad/frontkeep/issues/31)) ([c5a85f3](https://github.com/glemmestad/frontkeep/commit/c5a85f3427559791e4f8319224ca84e159725b32))

# [0.12.0](https://github.com/glemmestad/frontkeep/compare/v0.11.1...v0.12.0) (2026-06-06)


### Features

* **skills:** Skills Catalog — host, translate, and govern agent skills ([#30](https://github.com/glemmestad/frontkeep/issues/30)) ([cccdfec](https://github.com/glemmestad/frontkeep/commit/cccdfec6b7464e10736514ac97caadcd545b63a3))

## [0.11.1](https://github.com/glemmestad/frontkeep/compare/v0.11.0...v0.11.1) (2026-06-05)


### Bug Fixes

* **mcp:** de-footgun first-run MCP setup (inline PAT + actionable auth errors) ([#29](https://github.com/glemmestad/frontkeep/issues/29)) ([ea9f7a6](https://github.com/glemmestad/frontkeep/commit/ea9f7a6707b73dc119f870f6757d8ce273c023e3))

# [0.11.0](https://github.com/glemmestad/frontkeep/compare/v0.10.0...v0.11.0) (2026-06-05)


### Features

* machine-judged promotion gate (async repo-reading code reviewer) ([#28](https://github.com/glemmestad/frontkeep/issues/28)) ([432cb71](https://github.com/glemmestad/frontkeep/commit/432cb7178526870318dde1442f9fe0493f7447e2))

# [0.10.0](https://github.com/glemmestad/frontkeep/compare/v0.9.1...v0.10.0) (2026-06-05)


### Features

* **provision:** async, crash-safe provisioning and deprovisioning ([#27](https://github.com/glemmestad/frontkeep/issues/27)) ([6c07ee6](https://github.com/glemmestad/frontkeep/commit/6c07ee64d39add6fff089a246ed55a77320ef7fe))

## [0.9.1](https://github.com/glemmestad/frontkeep/compare/v0.9.0...v0.9.1) (2026-06-05)


### Bug Fixes

* **provision:** thread resolved region into ecs-service awslogs driver ([#26](https://github.com/glemmestad/frontkeep/issues/26)) ([b800d7f](https://github.com/glemmestad/frontkeep/commit/b800d7f3eb4c707cef0cdf5fb62db4ac0d102c84))

# [0.9.0](https://github.com/glemmestad/frontkeep/compare/v0.8.4...v0.9.0) (2026-06-05)


### Features

* **provision:** optional SSO connections + dedicated audience for auth0-application ([#25](https://github.com/glemmestad/frontkeep/issues/25)) ([4c5ad19](https://github.com/glemmestad/frontkeep/commit/4c5ad1920facd934974ab5baa64e98b3cae6a392))

## [0.8.4](https://github.com/glemmestad/frontkeep/compare/v0.8.3...v0.8.4) (2026-06-05)


### Bug Fixes

* getting-started plain-English + Service Catalog polish ([#24](https://github.com/glemmestad/frontkeep/issues/24)) ([306526c](https://github.com/glemmestad/frontkeep/commit/306526ccc052bed47431f749bfb32439caadafe5))

## [0.8.3](https://github.com/glemmestad/frontkeep/compare/v0.8.2...v0.8.3) (2026-06-05)


### Bug Fixes

* getting-started — real one-call repo bootstrap + LLM-key example ([#23](https://github.com/glemmestad/frontkeep/issues/23)) ([6480120](https://github.com/glemmestad/frontkeep/commit/6480120f0be4d8ae46a6586ed4a3794ec6731688))

## [0.8.2](https://github.com/glemmestad/frontkeep/compare/v0.8.1...v0.8.2) (2026-06-05)


### Bug Fixes

* **provision:** AWS-wide default region + account, RDS-specific subnet/SG ([#22](https://github.com/glemmestad/frontkeep/issues/22)) ([5d1040b](https://github.com/glemmestad/frontkeep/commit/5d1040b1cd1802af2f4d0d4643d643be42d5f265))

## [0.8.1](https://github.com/glemmestad/frontkeep/compare/v0.8.0...v0.8.1) (2026-06-04)


### Bug Fixes

* **provision:** let an image-only deploy add its own services via ASGARD_SERVICES_DIR ([#21](https://github.com/glemmestad/frontkeep/issues/21)) ([ccfd990](https://github.com/glemmestad/frontkeep/commit/ccfd990fc86a51ed9e2ef2b6385e229de702fd8b))

# [0.8.0](https://github.com/glemmestad/frontkeep/compare/v0.7.0...v0.8.0) (2026-06-04)


### Features

* self-service-first provisioning, per-classification ceilings, mutable projects ([#20](https://github.com/glemmestad/frontkeep/issues/20)) ([33f34c0](https://github.com/glemmestad/frontkeep/commit/33f34c09fb152e42710ac2104aecb3641631ce03))

# [0.7.0](https://github.com/glemmestad/frontkeep/compare/v0.6.0...v0.7.0) (2026-06-04)


### Features

* **provision:** first-class cross-resource access grants ([#18](https://github.com/glemmestad/frontkeep/issues/18)) ([a37fa77](https://github.com/glemmestad/frontkeep/commit/a37fa77af30cbcf8e3baf0d3a1eb593317599bd8))

# [0.6.0](https://github.com/glemmestad/frontkeep/compare/v0.5.2...v0.6.0) (2026-06-04)


### Features

* add an MCP catalog for publishing and sharing MCP servers ([#17](https://github.com/glemmestad/frontkeep/issues/17)) ([23387eb](https://github.com/glemmestad/frontkeep/commit/23387eb71ce5af465ee72d687c8b60898bc0a243))

## [0.5.2](https://github.com/glemmestad/frontkeep/compare/v0.5.1...v0.5.2) (2026-06-04)


### Bug Fixes

* **release:** stamp workspace version directly; drop semantic-release-cargo ([#16](https://github.com/glemmestad/frontkeep/issues/16)) ([e146744](https://github.com/glemmestad/frontkeep/commit/e14674468e71f62fbe791156076e36776b2231b2))

## [0.5.1](https://github.com/glemmestad/frontkeep/compare/v0.5.0...v0.5.1) (2026-06-04)


### Bug Fixes

* **mcp:** render free-form spec fields as object schemas ([#13](https://github.com/glemmestad/frontkeep/issues/13)) ([1e273d8](https://github.com/glemmestad/frontkeep/commit/1e273d8ad819a86f0583b2681abe532ea832ca27))
* **release:** bump crate versions in lockstep with release tag ([#14](https://github.com/glemmestad/frontkeep/issues/14)) ([a6a2416](https://github.com/glemmestad/frontkeep/commit/a6a24168335084a7a0ab0a976f0a292d00102aa7))
* **release:** correct semantic-release-cargo install target format ([#15](https://github.com/glemmestad/frontkeep/issues/15)) ([5140b59](https://github.com/glemmestad/frontkeep/commit/5140b592fc047d32509cad891970aca68c97662e))

# [0.5.0](https://github.com/glemmestad/frontkeep/compare/v0.4.2...v0.5.0) (2026-06-04)


### Features

* **website:** marketing site at frontkeep.build with auto-synced docs ([#12](https://github.com/glemmestad/frontkeep/issues/12)) ([1e8675c](https://github.com/glemmestad/frontkeep/commit/1e8675c146bb1420e24594c2eb3b009a48cdc274))

## [0.4.2](https://github.com/glemmestad/frontkeep/compare/v0.4.1...v0.4.2) (2026-06-04)


### Bug Fixes

* serve docs at /docs and expand the getting-started flow ([#11](https://github.com/glemmestad/frontkeep/issues/11)) ([6254fad](https://github.com/glemmestad/frontkeep/commit/6254fad05df86d27681f872c4c91073c899938c6))

## [0.4.1](https://github.com/glemmestad/frontkeep/compare/v0.4.0...v0.4.1) (2026-06-04)


### Bug Fixes

* resolve manifest module paths against modules_dir and rebrand UI title ([#10](https://github.com/glemmestad/frontkeep/issues/10)) ([25c5c26](https://github.com/glemmestad/frontkeep/commit/25c5c26d73b3754b65c35ed5f7a2e989ee5171b2))

# [0.4.0](https://github.com/glemmestad/frontkeep/compare/v0.3.0...v0.4.0) (2026-06-04)


### Features

* coordinate replicas with DB leases so concurrent writes are safe ([#8](https://github.com/glemmestad/frontkeep/issues/8)) ([a3ffadb](https://github.com/glemmestad/frontkeep/commit/a3ffadb6374468355be99e8061df0d380605eea1))
* **ui:** default to light theme and reuse existing PATs ([#9](https://github.com/glemmestad/frontkeep/issues/9)) ([57af8df](https://github.com/glemmestad/frontkeep/commit/57af8df8aff9736d569c259b40ad5d4c87bf4f1e))

# [0.3.0](https://github.com/glemmestad/frontkeep/compare/v0.2.1...v0.3.0) (2026-06-04)


### Features

* IdP-driven SSO roles + lockable local login ([#7](https://github.com/glemmestad/frontkeep/issues/7)) ([3e18c5d](https://github.com/glemmestad/frontkeep/commit/3e18c5d1462bc61a8752d773a30e71bf3fb9e022))

## [0.2.1](https://github.com/glemmestad/frontkeep/compare/v0.2.0...v0.2.1) (2026-06-04)


### Bug Fixes

* env-armed provisioning defaults target to the first allowed entry ([#6](https://github.com/glemmestad/frontkeep/issues/6)) ([2c7ceed](https://github.com/glemmestad/frontkeep/commit/2c7ceed8e5133858dbf1eb82b04ac92d1aac4b2e))

# [0.2.0](https://github.com/glemmestad/frontkeep/compare/v0.1.0...v0.2.0) (2026-06-04)


### Features

* durable terraform state in DB + container-first provisioning ([#5](https://github.com/glemmestad/frontkeep/issues/5)) ([c9b899b](https://github.com/glemmestad/frontkeep/commit/c9b899b618716b3ab5aded492b25daa8cdfac2bd))

# [0.1.0](https://github.com/glemmestad/frontkeep/compare/v0.0.1...v0.1.0) (2026-06-04)


### Bug Fixes

* adopt semantic-release for versioning and image tags ([#4](https://github.com/glemmestad/frontkeep/issues/4)) ([a2942b5](https://github.com/glemmestad/frontkeep/commit/a2942b506e1797ef2831309616911b1fc4087b5a))


### Features

* configurable system display name (ASGARD_SYSTEM_NAME) ([#2](https://github.com/glemmestad/frontkeep/issues/2)) ([56acc40](https://github.com/glemmestad/frontkeep/commit/56acc40fc771e03ad5c134cbc8e306487d42b579))
