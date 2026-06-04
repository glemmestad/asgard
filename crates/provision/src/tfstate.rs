//! Durable Terraform state, stored in Asgard's own DB (SQLite or Postgres) so it
//! survives ephemeral compute with no external backend (no S3, no EFS). The
//! terraform connector snapshots each resource's `terraform.tfstate` here around
//! every apply/destroy. Values are envelope-encrypted (AES-256-GCM) with the
//! secret-store master key, since state carries provider secrets in the clear.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use sqlx::Row;

use asgard_storage::Db;

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

    /// Decrypt and return the stored state for `id`, or `None` if there is none.
    pub async fn load(&self, id: &str) -> Result<Option<Vec<u8>>, ProvisionError> {
        let row = sqlx::query(
            &self
                .db
                .q("SELECT ciphertext, nonce FROM tf_state WHERE id = ?"),
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
        Ok(Some(plain))
    }

    /// Encrypt and upsert the state for `id`.
    pub async fn save(&self, id: &str, state: &[u8]) -> Result<(), ProvisionError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher()
            .encrypt(&nonce, state)
            .map_err(|e| ProvisionError::Backend(format!("encrypt tf state: {e}")))?;
        sqlx::query(&self.db.q(
            "INSERT INTO tf_state (id, ciphertext, nonce, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET ciphertext = excluded.ciphertext, \
             nonce = excluded.nonce, updated_at = excluded.updated_at",
        ))
        .bind(id)
        .bind(to_hex(&ct))
        .bind(to_hex(nonce.as_slice()))
        .bind(asgard_storage::now())
        .execute(self.db.pool())
        .await?;
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
            std::env::temp_dir().join(format!("asgard-tfs-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        TfStateStore::new(db, [0x11; 32])
    }

    #[tokio::test]
    async fn save_load_roundtrip_and_overwrite() {
        let s = store().await;
        assert!(s.load("proj/ecs/web").await.unwrap().is_none());
        assert!(!s.exists("proj/ecs/web").await.unwrap());

        s.save("proj/ecs/web", b"{\"v\":1}").await.unwrap();
        assert!(s.exists("proj/ecs/web").await.unwrap());
        assert_eq!(s.load("proj/ecs/web").await.unwrap().unwrap(), b"{\"v\":1}");

        // Upsert replaces in place.
        s.save("proj/ecs/web", b"{\"v\":2}").await.unwrap();
        assert_eq!(s.load("proj/ecs/web").await.unwrap().unwrap(), b"{\"v\":2}");

        s.delete("proj/ecs/web").await.unwrap();
        assert!(s.load("proj/ecs/web").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ciphertext_is_not_plaintext() {
        let s = store().await;
        s.save("p/t/n", b"super-secret-state").await.unwrap();
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
        s.save("p/t/n", b"state").await.unwrap();
        let other = TfStateStore::new(s.db.clone(), [0x22; 32]);
        assert!(other.load("p/t/n").await.is_err());
    }
}
