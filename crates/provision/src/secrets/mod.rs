//! Secret exchange: references in the control plane, values in the store.
//!
//! The invariant: a secret value never enters git, a manifest, a resource
//! record, a log, or an audit entry — only a [`SecretRef`] does. Provisioning
//! that yields credentials writes the *value* to a [`SecretStore`] and records
//! only `{store, path, version}`. Stores are pluggable (mirrors connectors and
//! cost sources): `builtin` (envelope-encrypted in Frontkeep's DB) is the
//! zero-config default; managed stores (AWS/GCP/Azure) are operator-selected
//! seams.

mod builtin;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::ProvisionError;

pub use builtin::BuiltinSecretStore;

/// The only thing that ever leaves the store. `path` is stable across rotations
/// so a recorded ref keeps resolving; `version` is the version at write time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretRef {
    pub store: String,
    pub path: String,
    pub version: i64,
}

impl SecretRef {
    /// Marker key under which a ref is recorded in resource outputs.
    pub const KEY: &'static str = "secret_ref";
}

/// Non-sensitive metadata about a stored secret (never the value).
#[derive(Debug, Clone, Serialize)]
pub struct SecretInfo {
    pub project_id: String,
    pub name: String,
    pub version: i64,
    pub created_at: String,
    pub rotated_at: Option<String>,
    pub rotation_interval_days: Option<i64>,
}

/// A pluggable place where secret values live. Authorization and audit are the
/// caller's concern (enforced at the MCP/API boundary); the store only keeps
/// values out of the control plane.
#[async_trait]
pub trait SecretStore: Send + Sync {
    fn name(&self) -> &str;
    /// Write a value for `project_id/name`, returning a ref. Re-writing an
    /// existing name creates a new version with a stable path.
    async fn put(
        &self,
        project_id: &str,
        name: &str,
        value: &str,
        rotation_interval_days: Option<i64>,
    ) -> Result<SecretRef, ProvisionError>;
    /// Fetch the current value for a ref. The store resolves the latest version
    /// for the path so a ref recorded before a rotation still works.
    async fn get(&self, r: &SecretRef) -> Result<String, ProvisionError>;
    /// Rotate to a fresh value (new version, stable path).
    async fn rotate(&self, r: &SecretRef) -> Result<SecretRef, ProvisionError>;
    async fn delete(&self, r: &SecretRef) -> Result<(), ProvisionError>;
    async fn list(&self, project_id: &str) -> Result<Vec<SecretInfo>, ProvisionError>;
    /// Secrets whose `rotation_interval_days` has elapsed since `rotated_at`
    /// (or `created_at`), as of `now_rfc3339`. Drives the rotation sweep.
    async fn due_for_rotation(&self, _now_rfc3339: &str) -> Result<Vec<SecretRef>, ProvisionError> {
        Ok(vec![])
    }
}

/// A managed-store seam awaiting operator creds. Real shape, returns
/// "unconfigured" until wired (AWS Secrets Manager / GCP Secret Manager / Azure
/// Key Vault / Vault).
pub struct UnconfiguredStore {
    label: String,
}

impl UnconfiguredStore {
    pub fn new(label: impl Into<String>) -> Self {
        UnconfiguredStore {
            label: label.into(),
        }
    }
    fn err(&self) -> ProvisionError {
        ProvisionError::Backend(format!("secret store '{}' is not configured", self.label))
    }
}

#[async_trait]
impl SecretStore for UnconfiguredStore {
    fn name(&self) -> &str {
        &self.label
    }
    async fn put(
        &self,
        _p: &str,
        _n: &str,
        _v: &str,
        _r: Option<i64>,
    ) -> Result<SecretRef, ProvisionError> {
        Err(self.err())
    }
    async fn get(&self, _r: &SecretRef) -> Result<String, ProvisionError> {
        Err(self.err())
    }
    async fn rotate(&self, _r: &SecretRef) -> Result<SecretRef, ProvisionError> {
        Err(self.err())
    }
    async fn delete(&self, _r: &SecretRef) -> Result<(), ProvisionError> {
        Err(self.err())
    }
    async fn list(&self, _p: &str) -> Result<Vec<SecretInfo>, ProvisionError> {
        Err(self.err())
    }
}

/// Generate fresh random secret material (hex-encoded 32 bytes).
pub fn random_secret() -> String {
    use aes_gcm::aead::rand_core::RngCore;
    let mut buf = [0u8; 32];
    aes_gcm::aead::OsRng.fill_bytes(&mut buf);
    builtin::to_hex(&buf)
}
