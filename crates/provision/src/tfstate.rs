//! Durable Terraform state, stored in Frontkeep's own DB (SQLite or Postgres) so it
//! survives ephemeral compute with no external backend (no S3, no EFS). The
//! terraform connector snapshots each resource's `terraform.tfstate` here around
//! every apply/destroy. Values are envelope-encrypted (AES-256-GCM) with the
//! secret-store master key, since state carries provider secrets in the clear.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use sqlx::Row;

use frontkeep_storage::Db;

use crate::ProvisionError;

pub struct TfStateStore {
    db: Db,
    key: [u8; 32],
}

impl TfStateStore {
    pub fn new(db: Db, key: [u8; 32]) -> Self {
        TfStateStore { db, key }
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key))
    }

    /// Decrypt and return the stored state for `id` with its version, or `None` if
    /// there is none. The version feeds the compare-and-swap in [`Self::save_cas`].
    pub async fn load(&self, id: &str) -> Result<Option<(Vec<u8>, i64)>, ProvisionError> {
        let row = sqlx::query(
            &self
                .db
                .q("SELECT ciphertext, nonce, version FROM tf_state WHERE id = ?"),
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let ct = from_hex(&row.get::<String, _>("ciphertext"))?;
        let nonce = from_hex(&row.get::<String, _>("nonce"))?;
        let plain = self
            .cipher()
            .decrypt(Nonce::from_slice(&nonce), ct.as_ref())
            .map_err(|e| ProvisionError::Backend(format!("decrypt tf state: {e}")))?;
        Ok(Some((plain, row.get::<i64, _>("version"))))
    }

    /// Encrypt and persist the state for `id` as a compare-and-swap on `version`.
    /// `expected` is the version [`Self::load`] returned (`None` for a first write).
    /// A mismatch — another instance advanced the state under us — is a `Conflict`
    /// that writes nothing, so a stale snapshot can never clobber newer state.
    pub async fn save_cas(
        &self,
        id: &str,
        state: &[u8],
        expected: Option<i64>,
    ) -> Result<(), ProvisionError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher()
            .encrypt(&nonce, state)
            .map_err(|e| ProvisionError::Backend(format!("encrypt tf state: {e}")))?;
        let ct_hex = to_hex(&ct);
        let nonce_hex = to_hex(nonce.as_slice());
        let now = frontkeep_storage::now();
        let affected = match expected {
            Some(v) => sqlx::query(&self.db.q(
                "UPDATE tf_state SET ciphertext = ?, nonce = ?, updated_at = ?, \
                 version = version + 1 WHERE id = ? AND version = ?",
            ))
            .bind(&ct_hex)
            .bind(&nonce_hex)
            .bind(&now)
            .bind(id)
            .bind(v)
            .execute(self.db.pool())
            .await?
            .rows_affected(),
            None => sqlx::query(&self.db.q(
                "INSERT INTO tf_state (id, ciphertext, nonce, updated_at, version) \
                 VALUES (?, ?, ?, ?, 1) ON CONFLICT(id) DO NOTHING",
            ))
            .bind(id)
            .bind(&ct_hex)
            .bind(&nonce_hex)
            .bind(&now)
            .execute(self.db.pool())
            .await?
            .rows_affected(),
        };
        if affected == 0 {
            return Err(ProvisionError::Conflict(format!(
                "tf state {id} changed under us (expected version {expected:?})"
            )));
        }
        Ok(())
    }

    pub async fn exists(&self, id: &str) -> Result<bool, ProvisionError> {
        let row = sqlx::query(&self.db.q("SELECT 1 AS one FROM tf_state WHERE id = ?"))
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row.is_some())
    }

    pub async fn delete(&self, id: &str) -> Result<(), ProvisionError> {
        sqlx::query(&self.db.q("DELETE FROM tf_state WHERE id = ?"))
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(())
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

    async fn store() -> TfStateStore {
        let path =
            std::env::temp_dir().join(format!("frontkeep-tfs-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        TfStateStore::new(db, [0x11; 32])
    }

    #[tokio::test]
    async fn save_cas_roundtrips_and_rejects_stale_writes() {
        let s = store().await;
        assert!(s.load("proj/ecs/web").await.unwrap().is_none());
        assert!(!s.exists("proj/ecs/web").await.unwrap());

        // First write (no prior version), then a CAS overwrite with the loaded one.
        s.save_cas("proj/ecs/web", b"{\"v\":1}", None)
            .await
            .unwrap();
        assert!(s.exists("proj/ecs/web").await.unwrap());
        let (bytes, version) = s.load("proj/ecs/web").await.unwrap().unwrap();
        assert_eq!(bytes, b"{\"v\":1}");
        assert_eq!(version, 1);

        s.save_cas("proj/ecs/web", b"{\"v\":2}", Some(1))
            .await
            .unwrap();
        let (bytes, version) = s.load("proj/ecs/web").await.unwrap().unwrap();
        assert_eq!(bytes, b"{\"v\":2}");
        assert_eq!(version, 2);

        // A stale expected version conflicts and writes nothing.
        assert!(matches!(
            s.save_cas("proj/ecs/web", b"{\"v\":3}", Some(1)).await,
            Err(ProvisionError::Conflict(_))
        ));
        assert_eq!(
            s.load("proj/ecs/web").await.unwrap().unwrap().0,
            b"{\"v\":2}"
        );

        // A first-write against a row that already exists conflicts too.
        assert!(matches!(
            s.save_cas("proj/ecs/web", b"{\"v\":9}", None).await,
            Err(ProvisionError::Conflict(_))
        ));

        s.delete("proj/ecs/web").await.unwrap();
        assert!(s.load("proj/ecs/web").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ciphertext_is_not_plaintext() {
        let s = store().await;
        s.save_cas("p/t/n", b"super-secret-state", None)
            .await
            .unwrap();
        let row = sqlx::query("SELECT ciphertext FROM tf_state WHERE id = ?")
            .bind("p/t/n")
            .fetch_one(s.db.pool())
            .await
            .unwrap();
        let ct: String = row.get("ciphertext");
        assert!(!ct.contains("super-secret"));
    }

    #[tokio::test]
    async fn wrong_key_fails_to_decrypt() {
        let s = store().await;
        s.save_cas("p/t/n", b"state", None).await.unwrap();
        let other = TfStateStore::new(s.db.clone(), [0x22; 32]);
        assert!(other.load("p/t/n").await.is_err());
    }
}
