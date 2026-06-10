//! Provisioning connectors: the pluggable "how" behind a service manifest.
//! Routing is by `provisioner.connector`, not by cloud — so a genuinely new
//! service (any cloud, any kind) is a manifest pointing at `terraform`/`exec`/
//! `http`/`mcp` with zero Frontkeep code. `terraform` is the universal, unrestricted
//! path: the hub team's modules define what gets built (every AWS/GCP/Azure
//! resource included). `stub` is the dry-run default and the fallback when a
//! manifest's connector isn't registered in this deployment.

pub mod exec;
pub mod litellm;
pub mod terraform;

pub use exec::ExecConnector;
pub use litellm::LiteLlmConnector;
pub use terraform::TerraformConnector;
