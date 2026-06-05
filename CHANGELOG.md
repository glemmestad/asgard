# [0.10.0](https://github.com/glemmestad/asgard/compare/v0.9.1...v0.10.0) (2026-06-05)


### Features

* **provision:** async, crash-safe provisioning and deprovisioning ([#27](https://github.com/glemmestad/asgard/issues/27)) ([6c07ee6](https://github.com/glemmestad/asgard/commit/6c07ee64d39add6fff089a246ed55a77320ef7fe))

## [0.9.1](https://github.com/glemmestad/asgard/compare/v0.9.0...v0.9.1) (2026-06-05)


### Bug Fixes

* **provision:** thread resolved region into ecs-service awslogs driver ([#26](https://github.com/glemmestad/asgard/issues/26)) ([b800d7f](https://github.com/glemmestad/asgard/commit/b800d7f3eb4c707cef0cdf5fb62db4ac0d102c84))

# [0.9.0](https://github.com/glemmestad/asgard/compare/v0.8.4...v0.9.0) (2026-06-05)


### Features

* **provision:** optional SSO connections + dedicated audience for auth0-application ([#25](https://github.com/glemmestad/asgard/issues/25)) ([4c5ad19](https://github.com/glemmestad/asgard/commit/4c5ad1920facd934974ab5baa64e98b3cae6a392))

## [0.8.4](https://github.com/glemmestad/asgard/compare/v0.8.3...v0.8.4) (2026-06-05)


### Bug Fixes

* getting-started plain-English + Service Catalog polish ([#24](https://github.com/glemmestad/asgard/issues/24)) ([306526c](https://github.com/glemmestad/asgard/commit/306526ccc052bed47431f749bfb32439caadafe5))

## [0.8.3](https://github.com/glemmestad/asgard/compare/v0.8.2...v0.8.3) (2026-06-05)


### Bug Fixes

* getting-started — real one-call repo bootstrap + LLM-key example ([#23](https://github.com/glemmestad/asgard/issues/23)) ([6480120](https://github.com/glemmestad/asgard/commit/6480120f0be4d8ae46a6586ed4a3794ec6731688))

## [0.8.2](https://github.com/glemmestad/asgard/compare/v0.8.1...v0.8.2) (2026-06-05)


### Bug Fixes

* **provision:** AWS-wide default region + account, RDS-specific subnet/SG ([#22](https://github.com/glemmestad/asgard/issues/22)) ([5d1040b](https://github.com/glemmestad/asgard/commit/5d1040b1cd1802af2f4d0d4643d643be42d5f265))

## [0.8.1](https://github.com/glemmestad/asgard/compare/v0.8.0...v0.8.1) (2026-06-04)


### Bug Fixes

* **provision:** let an image-only deploy add its own services via ASGARD_SERVICES_DIR ([#21](https://github.com/glemmestad/asgard/issues/21)) ([ccfd990](https://github.com/glemmestad/asgard/commit/ccfd990fc86a51ed9e2ef2b6385e229de702fd8b))

# [0.8.0](https://github.com/glemmestad/asgard/compare/v0.7.0...v0.8.0) (2026-06-04)


### Features

* self-service-first provisioning, per-classification ceilings, mutable projects ([#20](https://github.com/glemmestad/asgard/issues/20)) ([33f34c0](https://github.com/glemmestad/asgard/commit/33f34c09fb152e42710ac2104aecb3641631ce03))

# [0.7.0](https://github.com/glemmestad/asgard/compare/v0.6.0...v0.7.0) (2026-06-04)


### Features

* **provision:** first-class cross-resource access grants ([#18](https://github.com/glemmestad/asgard/issues/18)) ([a37fa77](https://github.com/glemmestad/asgard/commit/a37fa77af30cbcf8e3baf0d3a1eb593317599bd8))

# [0.6.0](https://github.com/glemmestad/asgard/compare/v0.5.2...v0.6.0) (2026-06-04)


### Features

* add an MCP catalog for publishing and sharing MCP servers ([#17](https://github.com/glemmestad/asgard/issues/17)) ([23387eb](https://github.com/glemmestad/asgard/commit/23387eb71ce5af465ee72d687c8b60898bc0a243))

## [0.5.2](https://github.com/glemmestad/asgard/compare/v0.5.1...v0.5.2) (2026-06-04)


### Bug Fixes

* **release:** stamp workspace version directly; drop semantic-release-cargo ([#16](https://github.com/glemmestad/asgard/issues/16)) ([e146744](https://github.com/glemmestad/asgard/commit/e14674468e71f62fbe791156076e36776b2231b2))

## [0.5.1](https://github.com/glemmestad/asgard/compare/v0.5.0...v0.5.1) (2026-06-04)


### Bug Fixes

* **mcp:** render free-form spec fields as object schemas ([#13](https://github.com/glemmestad/asgard/issues/13)) ([1e273d8](https://github.com/glemmestad/asgard/commit/1e273d8ad819a86f0583b2681abe532ea832ca27))
* **release:** bump crate versions in lockstep with release tag ([#14](https://github.com/glemmestad/asgard/issues/14)) ([a6a2416](https://github.com/glemmestad/asgard/commit/a6a24168335084a7a0ab0a976f0a292d00102aa7))
* **release:** correct semantic-release-cargo install target format ([#15](https://github.com/glemmestad/asgard/issues/15)) ([5140b59](https://github.com/glemmestad/asgard/commit/5140b592fc047d32509cad891970aca68c97662e))

# [0.5.0](https://github.com/glemmestad/asgard/compare/v0.4.2...v0.5.0) (2026-06-04)


### Features

* **website:** marketing site at asgard.build with auto-synced docs ([#12](https://github.com/glemmestad/asgard/issues/12)) ([1e8675c](https://github.com/glemmestad/asgard/commit/1e8675c146bb1420e24594c2eb3b009a48cdc274))

## [0.4.2](https://github.com/glemmestad/asgard/compare/v0.4.1...v0.4.2) (2026-06-04)


### Bug Fixes

* serve docs at /docs and expand the getting-started flow ([#11](https://github.com/glemmestad/asgard/issues/11)) ([6254fad](https://github.com/glemmestad/asgard/commit/6254fad05df86d27681f872c4c91073c899938c6))

## [0.4.1](https://github.com/glemmestad/asgard/compare/v0.4.0...v0.4.1) (2026-06-04)


### Bug Fixes

* resolve manifest module paths against modules_dir and rebrand UI title ([#10](https://github.com/glemmestad/asgard/issues/10)) ([25c5c26](https://github.com/glemmestad/asgard/commit/25c5c26d73b3754b65c35ed5f7a2e989ee5171b2))

# [0.4.0](https://github.com/glemmestad/asgard/compare/v0.3.0...v0.4.0) (2026-06-04)


### Features

* coordinate replicas with DB leases so concurrent writes are safe ([#8](https://github.com/glemmestad/asgard/issues/8)) ([a3ffadb](https://github.com/glemmestad/asgard/commit/a3ffadb6374468355be99e8061df0d380605eea1))
* **ui:** default to light theme and reuse existing PATs ([#9](https://github.com/glemmestad/asgard/issues/9)) ([57af8df](https://github.com/glemmestad/asgard/commit/57af8df8aff9736d569c259b40ad5d4c87bf4f1e))

# [0.3.0](https://github.com/glemmestad/asgard/compare/v0.2.1...v0.3.0) (2026-06-04)


### Features

* IdP-driven SSO roles + lockable local login ([#7](https://github.com/glemmestad/asgard/issues/7)) ([3e18c5d](https://github.com/glemmestad/asgard/commit/3e18c5d1462bc61a8752d773a30e71bf3fb9e022))

## [0.2.1](https://github.com/glemmestad/asgard/compare/v0.2.0...v0.2.1) (2026-06-04)


### Bug Fixes

* env-armed provisioning defaults target to the first allowed entry ([#6](https://github.com/glemmestad/asgard/issues/6)) ([2c7ceed](https://github.com/glemmestad/asgard/commit/2c7ceed8e5133858dbf1eb82b04ac92d1aac4b2e))

# [0.2.0](https://github.com/glemmestad/asgard/compare/v0.1.0...v0.2.0) (2026-06-04)


### Features

* durable terraform state in DB + container-first provisioning ([#5](https://github.com/glemmestad/asgard/issues/5)) ([c9b899b](https://github.com/glemmestad/asgard/commit/c9b899b618716b3ab5aded492b25daa8cdfac2bd))

# [0.1.0](https://github.com/glemmestad/asgard/compare/v0.0.1...v0.1.0) (2026-06-04)


### Bug Fixes

* adopt semantic-release for versioning and image tags ([#4](https://github.com/glemmestad/asgard/issues/4)) ([a2942b5](https://github.com/glemmestad/asgard/commit/a2942b506e1797ef2831309616911b1fc4087b5a))


### Features

* configurable system display name (ASGARD_SYSTEM_NAME) ([#2](https://github.com/glemmestad/asgard/issues/2)) ([56acc40](https://github.com/glemmestad/asgard/commit/56acc40fc771e03ad5c134cbc8e306487d42b579))
