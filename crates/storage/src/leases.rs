//! Cross-instance coordination leases over the shared DB, so Asgard is safe to
//! run with more than one replica. A lease is a named row one process holds until
//! its TTL expires; whoever holds it owns the work it guards — a background-loop
//! tick, or a Terraform apply for one resource. Acquire/renew is a single
//! conditional upsert: the same `ON CONFLICT ... DO UPDATE ... RETURNING` shape
//! `id_counters` already runs on both backends, plus a `WHERE` that only lets the
//! current holder renew or a new holder steal an expired lease. On SQLite the lone
//! process always wins every lease, so behavior there is unchanged.

use crate::{Db, StorageError};

/// Acquires leases on behalf of one process. `holder` must be unique per process
/// (mint with [`crate::new_uid`]); cloning is cheap (just clones the [`Db`] handle).
#[derive(Clone)]
pub struct Leases {
    db: Db,
    holder: String,
}

impl Leases {
    pub fn new(db: Db, holder: impl Into<String>) -> Self {
        Leases {
            db,
            holder: holder.into(),
        }
    }

    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// Acquire `name` for `ttl_secs`, or renew it if we already hold it. Returns
    /// `true` iff we hold it afterwards: the lease was free, already ours, or
    /// expired. Returns `false` iff a different holder owns a still-valid lease.
    ///
    /// `fetch_optional` is required: when the `WHERE` is false (someone else holds
    /// it) the upsert touches no row and `RETURNING` yields nothing — that absence
    /// is the "lost the race" signal, not an error.
    pub async fn try_acquire(&self, name: &str, ttl_secs: i64) -> Result<bool, StorageError> {
        let now = crate::now();
        let expires = crate::plus_seconds(&now, ttl_secs);
        let row = sqlx::query(&self.db.q(
            "INSERT INTO leases (name, holder, expires_at) VALUES (?, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET holder = excluded.holder, \
             expires_at = excluded.expires_at \
             WHERE leases.holder = excluded.holder OR leases.expires_at < ? \
             RETURNING holder",
        ))
        .bind(name)
        .bind(&self.holder)
        .bind(&expires)
        .bind(&now)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.is_some())
    }

    /// Extend a lease we hold. Identical to [`Self::try_acquire`] — the `WHERE`
    /// re-checks we still hold it, so a lease stolen during a pause is not renewed.
    pub async fn renew(&self, name: &str, ttl_secs: i64) -> Result<bool, StorageError> {
        self.try_acquire(name, ttl_secs).await
    }

    /// Release a lease we hold (no-op if we no longer hold it), so the next
    /// acquirer can take it at once instead of waiting out the TTL.
    pub async fn release(&self, name: &str) -> Result<(), StorageError> {
        sqlx::query(
            &self
                .db
                .q("DELETE FROM leases WHERE name = ? AND holder = ?"),
        )
        .bind(name)
        .bind(&self.holder)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh() -> Db {
        let path = std::env::temp_dir().join(format!("asgard-lease-{}.db", crate::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        db
    }

    #[tokio::test]
    async fn live_lease_blocks_others_and_holder_renews() {
        let db = fresh().await;
        let a = Leases::new(db.clone(), "a");
        let b = Leases::new(db.clone(), "b");
        assert!(a.try_acquire("x", 60).await.unwrap()); // free -> a wins
        assert!(!b.try_acquire("x", 60).await.unwrap()); // a holds live -> b blocked
        assert!(a.renew("x", 60).await.unwrap()); // holder renews
        assert!(!b.try_acquire("x", 60).await.unwrap()); // still blocked
    }

    #[tokio::test]
    async fn release_frees_lease() {
        let db = fresh().await;
        let a = Leases::new(db.clone(), "a");
        let b = Leases::new(db.clone(), "b");
        assert!(a.try_acquire("x", 60).await.unwrap());
        a.release("x").await.unwrap();
        assert!(b.try_acquire("x", 60).await.unwrap()); // freed -> b wins
        a.release("x").await.unwrap(); // a no longer holds it -> no-op
        assert!(!a.try_acquire("x", 60).await.unwrap()); // b still holds it
    }

    #[tokio::test]
    async fn expired_lease_is_stealable() {
        let db = fresh().await;
        sqlx::query("INSERT INTO leases (name, holder, expires_at) VALUES (?, ?, ?)")
            .bind("x")
            .bind("ghost")
            .bind(crate::plus_seconds(&crate::now(), -10))
            .execute(db.pool())
            .await
            .unwrap();
        let b = Leases::new(db.clone(), "b");
        assert!(b.try_acquire("x", 60).await.unwrap()); // expired -> b steals
        let ghost = Leases::new(db.clone(), "ghost");
        assert!(!ghost.try_acquire("x", 60).await.unwrap()); // b now holds it live
    }
}
