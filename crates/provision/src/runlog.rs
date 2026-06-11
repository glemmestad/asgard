//! Provisioning run-logs: the full output of every connector run (terraform
//! plan+apply log, exec output, HTTP response), captured per resource for operator
//! audit and debugging. Stored in Frontkeep's own DB, append-only, and envelope-
//! encrypted (AES-256-GCM) with the secret-store master key — the output can carry
//! provider secrets in the clear, the same exposure `tf_state` already has, so it
//! gets the same protection and is only ever returned over a `ViewAudit`-gated read.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use serde::Serialize;
use sqlx::Row;

use frontkeep_storage::Db;

use crate::ProvisionError;

/// One captured connector run, as returned to an admin reader (output decrypted).
#[derive(Debug, Clone, Serialize)]
pub struct RunLogEntry {
    pub id: String,
    pub action: String,
    pub ok: bool,
    pub output: String,
    pub started_at: String,
    pub finished_at: String,
}

pub struct RunLogStore {
    db: Db,
    key: [u8; 32],
}

impl RunLogStore {
    pub fn new(db: Db, key: [u8; 32]) -> Self {
        RunLogStore { db, key }
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key))
    }

    /// Encrypt and append one run entry for a resource. Best-effort by convention:
    /// callers log-and-ignore failures so audit capture never breaks a provision.
    #[allow(clippy::too_many_arguments)]
    pub async fn append(
        &self,
        resource_id: &str,
        project_id: &str,
        action: &str,
        ok: bool,
        output: &str,
        started_at: &str,
        finished_at: &str,
    ) -> Result<(), ProvisionError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher()
            .encrypt(&nonce, output.as_bytes())
            .map_err(|e| ProvisionError::Backend(format!("encrypt run log: {e}")))?;
        let now = frontkeep_storage::now();
        sqlx::query(&self.db.q(
            "INSERT INTO provision_runs \
             (id, resource_id, project_id, action, ok, ciphertext, nonce, started_at, finished_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(frontkeep_storage::new_uid())
        .bind(resource_id)
        .bind(project_id)
        .bind(action)
        .bind(ok as i64)
        .bind(to_hex(&ct))
        .bind(to_hex(nonce.as_slice()))
        .bind(started_at)
        .bind(finished_at)
        .bind(&now)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Every run for a resource, oldest first (the attempt timeline), decrypted.
    pub async fn list_for_resource(
        &self,
        resource_id: &str,
    ) -> Result<Vec<RunLogEntry>, ProvisionError> {
        let rows = sqlx::query(&self.db.q(
            "SELECT id, action, ok, ciphertext, nonce, started_at, finished_at \
             FROM provision_runs WHERE resource_id = ? ORDER BY started_at, created_at",
        ))
        .bind(resource_id)
        .fetch_all(self.db.pool())
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let ct = from_hex(&r.get::<String, _>("ciphertext"))?;
            let nonce = from_hex(&r.get::<String, _>("nonce"))?;
            let plain = self
                .cipher()
                .decrypt(Nonce::from_slice(&nonce), ct.as_ref())
                .map_err(|e| ProvisionError::Backend(format!("decrypt run log: {e}")))?;
            out.push(RunLogEntry {
                id: r.get("id"),
                action: r.get("action"),
                ok: r.get::<i64, _>("ok") != 0,
                output: String::from_utf8_lossy(&plain).into_owned(),
                started_at: r.get("started_at"),
                finished_at: r.get("finished_at"),
            });
        }
        Ok(out)
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn from_hex(s: &str) -> Result<Vec<u8>, ProvisionError> {
    if s.len() % 2 != 0 {
        return Err(ProvisionError::Backend("odd-length hex".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| ProvisionError::Backend(format!("bad hex: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> RunLogStore {
        let path = std::env::temp_dir().join(format!(
            "frontkeep-runlog-{}.db",
            frontkeep_storage::new_uid()
        ));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        RunLogStore::new(db, [0x11; 32])
    }

    #[tokio::test]
    async fn append_and_list_roundtrips_in_order() {
        let s = store().await;
        s.append("res-1", "proj-1", "apply", false, "boom", "t1", "t2")
            .await
            .unwrap();
        s.append("res-1", "proj-1", "apply", true, "ok done", "t3", "t4")
            .await
            .unwrap();
        s.append("res-2", "proj-1", "apply", true, "other", "t5", "t6")
            .await
            .unwrap();
        let runs = s.list_for_resource("res-1").await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].output, "boom");
        assert!(!runs[0].ok);
        assert_eq!(runs[1].output, "ok done");
        assert!(runs[1].ok);
    }

    #[tokio::test]
    async fn output_is_encrypted_at_rest() {
        let s = store().await;
        s.append("r", "p", "apply", true, "super-secret-token", "t1", "t2")
            .await
            .unwrap();
        let row = sqlx::query("SELECT ciphertext FROM provision_runs WHERE resource_id = ?")
            .bind("r")
            .fetch_one(s.db.pool())
            .await
            .unwrap();
        let ct: String = row.get("ciphertext");
        assert!(!ct.contains("super-secret"));
    }

    #[tokio::test]
    async fn wrong_key_fails_to_decrypt() {
        let s = store().await;
        s.append("r", "p", "apply", true, "x", "t1", "t2")
            .await
            .unwrap();
        let other = RunLogStore::new(s.db.clone(), [0x22; 32]);
        assert!(other.list_for_resource("r").await.is_err());
    }
}
