//! Builtin secret store: values envelope-encrypted (AES-256-GCM) at rest in
//! Frontkeep's DB. Zero-config default so the single binary works out of the box;
//! the master key comes from operator config (KMS/env/file) in production.
//!
//! A name is versioned in place: re-`put` or `rotate` inserts a new row with the
//! next version for `project_id/name`; `get` resolves the latest version for the
//! path, so a ref recorded before a rotation keeps resolving.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use async_trait::async_trait;
use sqlx::Row;

use frontkeep_storage::Db;

use super::{SecretInfo, SecretRef, SecretStore};
use crate::ProvisionError;

pub struct BuiltinSecretStore {
    db: Db,
    key: [u8; 32],
}

impl BuiltinSecretStore {
    pub fn new(db: Db, key: [u8; 32]) -> Self {
        BuiltinSecretStore { db, key }
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key))
    }

    fn path(project_id: &str, name: &str) -> String {
        format!("{project_id}/{name}")
    }

    fn split_path(path: &str) -> (&str, &str) {
        path.split_once('/').unwrap_or((path, ""))
    }

    async fn next_version(&self, project_id: &str, name: &str) -> Result<i64, ProvisionError> {
        let cur: Option<i64> = sqlx::query_scalar(
            &self
                .db
                .q("SELECT MAX(version) FROM secrets WHERE project_id = ? AND name = ?"),
        )
        .bind(project_id)
        .bind(name)
        .fetch_one(self.db.pool())
        .await?;
        Ok(cur.unwrap_or(0) + 1)
    }

    async fn write(
        &self,
        project_id: &str,
        name: &str,
        value: &str,
        rotation_interval_days: Option<i64>,
    ) -> Result<SecretRef, ProvisionError> {
        let version = self.next_version(project_id, name).await?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher()
            .encrypt(&nonce, value.as_bytes())
            .map_err(|e| ProvisionError::Backend(format!("encrypt secret: {e}")))?;
        let meta = serde_json::json!({ "rotation_interval_days": rotation_interval_days });
        sqlx::query(&self.db.q(
            "INSERT INTO secrets (id, project_id, name, version, ciphertext, nonce, meta, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(frontkeep_storage::new_uid())
        .bind(project_id)
        .bind(name)
        .bind(version)
        .bind(to_hex(&ct))
        .bind(to_hex(nonce.as_slice()))
        .bind(meta.to_string())
        .bind(frontkeep_storage::now())
        .execute(self.db.pool())
        .await?;
        Ok(SecretRef {
            store: self.name().to_string(),
            path: Self::path(project_id, name),
            version,
        })
    }

    async fn latest_row(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<
        Option<(
            String,
            String,
            serde_json::Value,
            i64,
            String,
            Option<String>,
        )>,
        ProvisionError,
    > {
        let row = sqlx::query(&self.db.q(
            "SELECT ciphertext, nonce, meta, version, created_at, rotated_at FROM secrets \
             WHERE project_id = ? AND name = ? ORDER BY version DESC LIMIT 1",
        ))
        .bind(project_id)
        .bind(name)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| {
            let meta: String = r.get("meta");
            (
                r.get::<String, _>("ciphertext"),
                r.get::<String, _>("nonce"),
                serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null),
                r.get::<i64, _>("version"),
                r.get::<String, _>("created_at"),
                r.get::<Option<String>, _>("rotated_at"),
            )
        }))
    }
}

#[async_trait]
impl SecretStore for BuiltinSecretStore {
    fn name(&self) -> &str {
        "builtin"
    }

    async fn put(
        &self,
        project_id: &str,
        name: &str,
        value: &str,
        rotation_interval_days: Option<i64>,
    ) -> Result<SecretRef, ProvisionError> {
        self.write(project_id, name, value, rotation_interval_days)
            .await
    }

    async fn get(&self, r: &SecretRef) -> Result<String, ProvisionError> {
        let (project_id, name) = Self::split_path(&r.path);
        let (ct_hex, nonce_hex, _meta, _v, _c, _rot) = self
            .latest_row(project_id, name)
            .await?
            .ok_or_else(|| ProvisionError::RefNotFound(r.path.clone()))?;
        let ct = from_hex(&ct_hex)?;
        let nonce_bytes = from_hex(&nonce_hex)?;
        let plain = self
            .cipher()
            .decrypt(Nonce::from_slice(&nonce_bytes), ct.as_ref())
            .map_err(|e| ProvisionError::Backend(format!("decrypt secret: {e}")))?;
        String::from_utf8(plain)
            .map_err(|e| ProvisionError::Backend(format!("secret not utf-8: {e}")))
    }

    async fn rotate(&self, r: &SecretRef) -> Result<SecretRef, ProvisionError> {
        let (project_id, name) = Self::split_path(&r.path);
        let interval =
            self.latest_row(project_id, name)
                .await?
                .and_then(|(_, _, meta, _, _, _)| {
                    meta.get("rotation_interval_days").and_then(|v| v.as_i64())
                });
        let new_ref = self
            .write(project_id, name, &super::random_secret(), interval)
            .await?;
        sqlx::query(&self.db.q(
            "UPDATE secrets SET rotated_at = ? WHERE project_id = ? AND name = ? AND version = ?",
        ))
        .bind(frontkeep_storage::now())
        .bind(project_id)
        .bind(name)
        .bind(new_ref.version)
        .execute(self.db.pool())
        .await?;
        Ok(new_ref)
    }

    async fn delete(&self, r: &SecretRef) -> Result<(), ProvisionError> {
        let (project_id, name) = Self::split_path(&r.path);
        sqlx::query(
            &self
                .db
                .q("DELETE FROM secrets WHERE project_id = ? AND name = ?"),
        )
        .bind(project_id)
        .bind(name)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    async fn list(&self, project_id: &str) -> Result<Vec<SecretInfo>, ProvisionError> {
        let rows = sqlx::query(&self.db.q(
            "SELECT name, MAX(version) AS version, MIN(created_at) AS created_at, \
             MAX(rotated_at) AS rotated_at, MAX(meta) AS meta FROM secrets \
             WHERE project_id = ? GROUP BY name ORDER BY name",
        ))
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let meta: String = r.get("meta");
                let meta: serde_json::Value =
                    serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
                SecretInfo {
                    project_id: project_id.to_string(),
                    name: r.get("name"),
                    version: r.get("version"),
                    created_at: r.get("created_at"),
                    rotated_at: r.get("rotated_at"),
                    rotation_interval_days: meta
                        .get("rotation_interval_days")
                        .and_then(|v| v.as_i64()),
                }
            })
            .collect())
    }

    async fn due_for_rotation(&self, now_rfc3339: &str) -> Result<Vec<SecretRef>, ProvisionError> {
        let now = match chrono::DateTime::parse_from_rfc3339(now_rfc3339) {
            Ok(t) => t.with_timezone(&chrono::Utc),
            Err(_) => return Ok(vec![]),
        };
        let mut due = Vec::new();
        let rows = sqlx::query(
            "SELECT project_id, name, MAX(version) AS version, MAX(rotated_at) AS rotated_at, \
             MIN(created_at) AS created_at, MAX(meta) AS meta FROM secrets \
             GROUP BY project_id, name",
        )
        .fetch_all(self.db.pool())
        .await?;
        for r in rows {
            let meta: String = r.get("meta");
            let meta: serde_json::Value =
                serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
            let Some(days) = meta.get("rotation_interval_days").and_then(|v| v.as_i64()) else {
                continue;
            };
            let rotated: Option<String> = r.get("rotated_at");
            let created: String = r.get("created_at");
            let last = rotated.unwrap_or(created);
            let Ok(last) = chrono::DateTime::parse_from_rfc3339(&last) else {
                continue;
            };
            if now.signed_duration_since(last.with_timezone(&chrono::Utc))
                >= chrono::Duration::days(days)
            {
                due.push(SecretRef {
                    store: self.name().to_string(),
                    path: format!(
                        "{}/{}",
                        r.get::<String, _>("project_id"),
                        r.get::<String, _>("name")
                    ),
                    version: r.get::<i64, _>("version"),
                });
            }
        }
        Ok(due)
    }
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
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

    async fn store() -> BuiltinSecretStore {
        let path =
            std::env::temp_dir().join(format!("frontkeep-sec-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        BuiltinSecretStore::new(db, [0x09; 32])
    }

    #[tokio::test]
    async fn put_get_roundtrip_and_versioning() {
        let s = store().await;
        let r = s.put("p", "db-pass", "s3cr3t", None).await.unwrap();
        assert_eq!(r.version, 1);
        assert_eq!(s.get(&r).await.unwrap(), "s3cr3t");
        // Re-put bumps the version; get resolves the latest for the stable path.
        let r2 = s.put("p", "db-pass", "rotated", None).await.unwrap();
        assert_eq!(r2.version, 2);
        assert_eq!(r2.path, r.path);
        assert_eq!(s.get(&r).await.unwrap(), "rotated");
    }

    #[tokio::test]
    async fn rotate_bumps_version_with_fresh_value() {
        let s = store().await;
        let r = s.put("p", "k", "v1", None).await.unwrap();
        let before = s.get(&r).await.unwrap();
        let r2 = s.rotate(&r).await.unwrap();
        assert!(r2.version > r.version);
        assert_ne!(s.get(&r2).await.unwrap(), before);
    }

    #[tokio::test]
    async fn list_and_delete() {
        let s = store().await;
        s.put("p", "a", "1", None).await.unwrap();
        s.put("p", "b", "2", None).await.unwrap();
        assert_eq!(s.list("p").await.unwrap().len(), 2);
        let r = SecretRef {
            store: "builtin".into(),
            path: "p/a".into(),
            version: 0,
        };
        s.delete(&r).await.unwrap();
        let names: Vec<String> = s
            .list("p")
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, vec!["b".to_string()]);
    }

    #[tokio::test]
    async fn due_for_rotation_respects_interval() {
        let s = store().await;
        // interval 0 days → immediately due; no interval → never due.
        s.put("p", "rotates", "v", Some(0)).await.unwrap();
        s.put("p", "static", "v", None).await.unwrap();
        let due = s.due_for_rotation(&frontkeep_storage::now()).await.unwrap();
        let paths: Vec<String> = due.into_iter().map(|r| r.path).collect();
        assert!(paths.contains(&"p/rotates".to_string()));
        assert!(!paths.contains(&"p/static".to_string()));
    }
}
