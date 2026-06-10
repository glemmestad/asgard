//! Identity: local users (Argon2-hashed passwords) + session tokens, and an OIDC
//! login scaffold (brief §6.5). SAML/SCIM are enterprise seams: present as a
//! trait with a not-implemented path, not built (brief §5).

pub mod oidc;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;

use asgard_storage::Db;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("db: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("user exists: {0}")]
    UserExists(String),
    #[error("password hashing error: {0}")]
    Hash(String),
    #[error("session expired or invalid")]
    InvalidSession,
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub provider: String,
    pub is_admin: bool,
    /// One of `admin` | `finance` | `manager` | `member`. The string form is what
    /// the API/UI carry; capability checks go through [`Role`].
    pub role: String,
    pub active: bool,
    pub created_at: String,
}

/// Outcome of [`IdentityService::ensure_admin`]. `generated_password` is set
/// only when the admin was just created without a password override — surface
/// it once and never persist it.
#[derive(Debug)]
pub struct AdminBootstrap {
    pub user: User,
    pub created: bool,
    pub generated_password: Option<String>,
}

/// A user's *org-wide* authority. Authority over a specific project (seeing its
/// cost, approving its requests) is not a role — it follows automatically from
/// being that project's owner or manager (recorded at registration). So the role
/// set is deliberately small: Admin (org control), Finance (org-wide cost), and
/// Member (the default, whose project authority is entirely relationship-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    Finance,
    Member,
}

/// A gated action. Each role grants a fixed set; see [`Role::can`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Create/list users and assign roles.
    ManageUsers,
    /// See cross-project cost (dashboard + reports). Without it a user sees only
    /// their own project's spend.
    ViewAllCost,
    /// Approve or reject resource requests.
    ApproveRequests,
    /// Register projects and request resources.
    Provision,
    /// Read the audit log.
    ViewAudit,
}

impl Role {
    pub fn parse(s: &str) -> Role {
        match s.trim().to_lowercase().as_str() {
            "admin" => Role::Admin,
            "finance" => Role::Finance,
            _ => Role::Member,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Finance => "finance",
            Role::Member => "member",
        }
    }
    pub fn is_admin(&self) -> bool {
        *self == Role::Admin
    }
    /// The capability matrix for *org-wide* authority. `ViewAllCost` is the
    /// see-everything override (Admin, Finance); everyone else sees cost and
    /// projects scoped to what they own or manage. `Provision` is the default
    /// authenticated capability. Per-project approval is a relationship check, not
    /// a role, so it is not granted here to a global role other than Admin.
    pub fn can(&self, cap: Capability) -> bool {
        use Capability::*;
        match self {
            Role::Admin => true,
            Role::Finance => matches!(cap, ViewAllCost | ViewAudit),
            Role::Member => matches!(cap, Provision),
        }
    }
}

/// The single authority for *who may clear a pending resource request*: an org-wide
/// approver (`ApproveRequests`, i.e. Admin) or the subject project's **manager**. The
/// project **owner** is deliberately excluded — the owner is typically the developer who
/// filed the request, and self-approving one's own over-budget request would defeat the
/// gate. Both the REST surface and the MCP surface call this so the two can't drift.
pub fn may_approve_request(role: Role, caller_email: &str, project_manager: &str) -> bool {
    role.can(Capability::ApproveRequests)
        || (!caller_email.is_empty() && caller_email.eq_ignore_ascii_case(project_manager))
}

/// How OIDC sign-ins map to roles. Two independent knobs, set from env:
///
/// - `admin_emails` is an *additive, promote-only* admin grant applied on every
///   login. It never demotes and never locks the UI — a break-glass list of
///   humans who should always be admin, even if the groups claim misfires.
/// - `admin_groups` / `finance_groups` turn on *authoritative* sync: when either
///   is non-empty (see [`OidcRoleConfig::authoritative`]) the IdP is the sole
///   source of truth and every login recomputes the role from the groups claim
///   (incl. demotion to `member`); the UI may no longer override OIDC roles.
#[derive(Debug, Clone, Default)]
pub struct OidcRoleConfig {
    pub admin_groups: Vec<String>,
    pub finance_groups: Vec<String>,
    pub admin_emails: Vec<String>,
    /// Userinfo claim the group values are read from (e.g. `groups`).
    pub groups_claim: String,
}

impl OidcRoleConfig {
    /// IdP-authoritative role sync is on once any group mapping is configured.
    pub fn authoritative(&self) -> bool {
        !self.admin_groups.is_empty() || !self.finance_groups.is_empty()
    }

    /// The role to apply on this login. In authoritative mode this is always
    /// `Some` (the fully-resolved role, demotion included); otherwise it is
    /// `Some(Admin)` only for an admin-email user (a promote-only signal) and
    /// `None` when nothing applies — leaving the role under manual control.
    pub fn target_role(&self, email: Option<&str>, groups: &[String]) -> Option<Role> {
        let is_admin_email = email
            .map(|e| self.admin_emails.iter().any(|a| a.eq_ignore_ascii_case(e)))
            .unwrap_or(false);
        let in_any = |set: &[String]| groups.iter().any(|g| set.iter().any(|s| s == g));
        if self.authoritative() {
            Some(if is_admin_email || in_any(&self.admin_groups) {
                Role::Admin
            } else if in_any(&self.finance_groups) {
                Role::Finance
            } else {
                Role::Member
            })
        } else if is_admin_email {
            Some(Role::Admin)
        } else {
            None
        }
    }
}

impl User {
    pub fn role(&self) -> Role {
        Role::parse(&self.role)
    }
    pub fn can(&self, cap: Capability) -> bool {
        self.role().can(cap)
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub user_id: String,
    pub expires_at: String,
}

/// A freshly minted personal access token. `token` is the plaintext, shown to
/// the user exactly once; only its hash is stored.
#[derive(Debug, Clone, Serialize)]
pub struct Pat {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The last 8 chars of the token — a non-secret display hint so the list can
    /// tell tokens apart without ever revealing the full value.
    #[serde(default)]
    pub suffix: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// PAT plaintext prefix — distinct from a project virtual key (`asg_…`) so the
/// MCP/REST bearer branch can tell a user credential from a project one.
pub const PAT_PREFIX: &str = "asg_pat_";

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_password(password: &str) -> Result<String, IdentityError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| IdentityError::Hash(e.to_string()))
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct IdentityService {
    db: Db,
}

impl IdentityService {
    pub fn new(db: Db) -> Self {
        IdentityService { db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub async fn create_local_user(
        &self,
        username: &str,
        password: &str,
        email: Option<&str>,
        display_name: Option<&str>,
        role: Role,
    ) -> Result<User, IdentityError> {
        if self.get_user_by_username(username).await?.is_some() {
            return Err(IdentityError::UserExists(username.to_string()));
        }
        let id = asgard_storage::new_uid();
        let created_at = asgard_storage::now();
        let hash = hash_password(password)?;
        let is_admin = role.is_admin();
        sqlx::query(&self.db.q(
            "INSERT INTO users (id, username, email, display_name, password_hash, provider, is_admin, role, active, created_at) \
             VALUES (?, ?, ?, ?, ?, 'local', ?, ?, 1, ?)",
        ))
        .bind(&id)
        .bind(username)
        .bind(email)
        .bind(display_name)
        .bind(&hash)
        .bind(is_admin as i64)
        .bind(role.as_str())
        .bind(&created_at)
        .execute(self.db.pool())
        .await?;
        Ok(User {
            id,
            username: username.to_string(),
            email: email.map(str::to_string),
            display_name: display_name.map(str::to_string),
            provider: "local".to_string(),
            is_admin,
            role: role.as_str().to_string(),
            active: true,
            created_at,
        })
    }

    /// First-boot admin, shared by `serve` and `asgard admin bootstrap`. Ensures
    /// `username` exists as a local Admin: returns the existing user untouched,
    /// or creates one — with `password_override` when given, otherwise a
    /// generated password returned once in `generated_password`.
    pub async fn ensure_admin(
        &self,
        username: &str,
        password_override: Option<String>,
    ) -> Result<AdminBootstrap, IdentityError> {
        if let Some(user) = self.get_user_by_username(username).await? {
            return Ok(AdminBootstrap {
                user,
                created: false,
                generated_password: None,
            });
        }
        let (pw, generated) = match password_override {
            Some(p) if !p.is_empty() => (p, false),
            _ => (
                format!("{}{}", asgard_storage::new_uid(), asgard_storage::new_uid()),
                true,
            ),
        };
        let user = self
            .create_local_user(username, &pw, None, Some("Administrator"), Role::Admin)
            .await?;
        Ok(AdminBootstrap {
            user,
            created: true,
            generated_password: generated.then_some(pw),
        })
    }

    /// All users, newest first — for the admin Users page.
    pub async fn list_users(&self) -> Result<Vec<User>, IdentityError> {
        let rows = sqlx::query(&self.db.q(
            "SELECT id, username, email, display_name, provider, is_admin, role, active, created_at \
             FROM users ORDER BY created_at DESC",
        ))
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.iter().map(row_to_user).collect())
    }

    /// Assign a role (keeps the legacy is_admin flag in sync).
    pub async fn set_role(&self, id: &str, role: Role) -> Result<(), IdentityError> {
        sqlx::query(
            &self
                .db
                .q("UPDATE users SET role = ?, is_admin = ? WHERE id = ?"),
        )
        .bind(role.as_str())
        .bind(role.is_admin() as i64)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Enable or disable an account. A disabled user cannot log in or hold a
    /// session — the way to cut off an OIDC user whose identity the IdP owns.
    pub async fn set_active(&self, id: &str, active: bool) -> Result<(), IdentityError> {
        sqlx::query(&self.db.q("UPDATE users SET active = ? WHERE id = ?"))
            .bind(active as i64)
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    pub async fn authenticate_local(
        &self,
        username: &str,
        password: &str,
    ) -> Result<User, IdentityError> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, username, email, display_name, password_hash, provider, is_admin, role, active, created_at \
             FROM users WHERE username = ?",
        ))
        .bind(username)
        .fetch_optional(self.db.pool())
        .await?
        .ok_or(IdentityError::InvalidCredentials)?;

        let stored: Option<String> = row.get("password_hash");
        let stored = stored.ok_or(IdentityError::InvalidCredentials)?;
        if !verify_password(password, &stored) {
            return Err(IdentityError::InvalidCredentials);
        }
        let user = row_to_user(&row);
        if !user.active {
            return Err(IdentityError::InvalidCredentials);
        }
        Ok(user)
    }

    pub async fn get_user_by_username(
        &self,
        username: &str,
    ) -> Result<Option<User>, IdentityError> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, username, email, display_name, provider, is_admin, role, active, created_at FROM users WHERE username = ?",
        ))
        .bind(username)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.as_ref().map(row_to_user))
    }

    pub async fn get_user(&self, id: &str) -> Result<Option<User>, IdentityError> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, username, email, display_name, provider, is_admin, role, active, created_at FROM users WHERE id = ?",
        ))
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.as_ref().map(row_to_user))
    }

    /// Provision (or update) a user from verified OIDC claims; keyed by username.
    ///
    /// `target_role` / `authoritative` come from [`OidcRoleConfig`]:
    /// - new user: created with `target_role` (else `member`);
    /// - existing + authoritative: role is overwritten to `target_role` every
    ///   login (full sync, demotion included);
    /// - existing + not authoritative: promoted to admin only if `target_role`
    ///   is `Some(Admin)` and it isn't already admin — never demoted, so manual
    ///   UI role management is preserved.
    pub async fn upsert_oidc_user(
        &self,
        username: &str,
        email: Option<&str>,
        display_name: Option<&str>,
        target_role: Option<Role>,
        authoritative: bool,
    ) -> Result<User, IdentityError> {
        if let Some(u) = self.get_user_by_username(username).await? {
            let current = u.role();
            let next = if authoritative {
                target_role.unwrap_or(Role::Member)
            } else if target_role == Some(Role::Admin) && current != Role::Admin {
                Role::Admin
            } else {
                current
            };
            if next != current {
                self.set_role(&u.id, next).await?;
                return Ok(User {
                    role: next.as_str().to_string(),
                    is_admin: next.is_admin(),
                    ..u
                });
            }
            return Ok(u);
        }
        let role = target_role.unwrap_or(Role::Member);
        let is_admin = role.is_admin();
        let id = asgard_storage::new_uid();
        let created_at = asgard_storage::now();
        sqlx::query(&self.db.q(
            "INSERT INTO users (id, username, email, display_name, password_hash, provider, is_admin, role, active, created_at) \
             VALUES (?, ?, ?, ?, NULL, 'oidc', ?, ?, 1, ?)",
        ))
        .bind(&id)
        .bind(username)
        .bind(email)
        .bind(display_name)
        .bind(is_admin as i64)
        .bind(role.as_str())
        .bind(&created_at)
        .execute(self.db.pool())
        .await?;
        Ok(User {
            id,
            username: username.to_string(),
            email: email.map(str::to_string),
            display_name: display_name.map(str::to_string),
            provider: "oidc".to_string(),
            is_admin,
            role: role.as_str().to_string(),
            active: true,
            created_at,
        })
    }

    pub async fn create_session(
        &self,
        user_id: &str,
        ttl_seconds: i64,
    ) -> Result<Session, IdentityError> {
        let token = format!("sess_{}{}", uuid_simple(), uuid_simple());
        let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(ttl_seconds))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query(&self.db.q(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
        ))
        .bind(sha256_hex(&token))
        .bind(user_id)
        .bind(asgard_storage::now())
        .bind(&expires_at)
        .execute(self.db.pool())
        .await?;
        Ok(Session {
            token,
            user_id: user_id.to_string(),
            expires_at,
        })
    }

    /// Returns the user for a non-expired session token.
    pub async fn validate_session(&self, token: &str) -> Result<User, IdentityError> {
        let row = sqlx::query(
            &self
                .db
                .q("SELECT user_id, expires_at FROM sessions WHERE token_hash = ?"),
        )
        .bind(sha256_hex(token))
        .fetch_optional(self.db.pool())
        .await?
        .ok_or(IdentityError::InvalidSession)?;
        let user_id: String = row.get("user_id");
        let expires_at: String = row.get("expires_at");
        if expires_at.as_str() < asgard_storage::now().as_str() {
            return Err(IdentityError::InvalidSession);
        }
        let user = self
            .get_user(&user_id)
            .await?
            .ok_or(IdentityError::InvalidSession)?;
        if !user.active {
            return Err(IdentityError::InvalidSession);
        }
        Ok(user)
    }

    pub async fn revoke_session(&self, token: &str) -> Result<(), IdentityError> {
        sqlx::query(&self.db.q("DELETE FROM sessions WHERE token_hash = ?"))
            .bind(sha256_hex(token))
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Mint a personal access token for a user. `ttl_seconds` of `None` means it
    /// never expires (revoke it to cut it off). The plaintext is returned once.
    pub async fn create_pat(
        &self,
        user_id: &str,
        name: &str,
        ttl_seconds: Option<i64>,
    ) -> Result<Pat, IdentityError> {
        let token = format!("{PAT_PREFIX}{}{}", uuid_simple(), uuid_simple());
        let suffix = token[token.len().saturating_sub(8)..].to_string();
        let id = asgard_storage::new_uid();
        let created_at = asgard_storage::now();
        let expires_at = ttl_seconds.map(|s| {
            (chrono::Utc::now() + chrono::Duration::seconds(s))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        });
        sqlx::query(&self.db.q(
            "INSERT INTO personal_access_tokens (id, user_id, name, token_hash, token_suffix, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(&id)
        .bind(user_id)
        .bind(name)
        .bind(sha256_hex(&token))
        .bind(&suffix)
        .bind(&created_at)
        .bind(&expires_at)
        .execute(self.db.pool())
        .await?;
        Ok(Pat {
            id,
            name: name.to_string(),
            token: Some(token),
            suffix,
            created_at,
            expires_at,
            revoked_at: None,
        })
    }

    /// Returns the user for a non-expired, non-revoked PAT (active user only).
    pub async fn validate_pat(&self, token: &str) -> Result<User, IdentityError> {
        let row = sqlx::query(&self.db.q(
            "SELECT user_id, expires_at, revoked_at FROM personal_access_tokens WHERE token_hash = ?",
        ))
        .bind(sha256_hex(token))
        .fetch_optional(self.db.pool())
        .await?
        .ok_or(IdentityError::InvalidSession)?;
        let revoked_at: Option<String> = row.get("revoked_at");
        if revoked_at.is_some() {
            return Err(IdentityError::InvalidSession);
        }
        let expires_at: Option<String> = row.get("expires_at");
        if let Some(exp) = expires_at {
            if exp.as_str() < asgard_storage::now().as_str() {
                return Err(IdentityError::InvalidSession);
            }
        }
        let user_id: String = row.get("user_id");
        let user = self
            .get_user(&user_id)
            .await?
            .ok_or(IdentityError::InvalidSession)?;
        if !user.active {
            return Err(IdentityError::InvalidSession);
        }
        Ok(user)
    }

    /// A user's PATs, newest first — never includes plaintext (only the hash is
    /// stored). For the account/token UI.
    pub async fn list_pats(&self, user_id: &str) -> Result<Vec<Pat>, IdentityError> {
        let rows = sqlx::query(&self.db.q(
            "SELECT id, name, token_suffix, created_at, expires_at, revoked_at FROM personal_access_tokens \
             WHERE user_id = ? ORDER BY created_at DESC",
        ))
        .bind(user_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .iter()
            .map(|r| Pat {
                id: r.get("id"),
                name: r.get("name"),
                token: None,
                suffix: r.get("token_suffix"),
                created_at: r.get("created_at"),
                expires_at: r.get("expires_at"),
                revoked_at: r.get("revoked_at"),
            })
            .collect())
    }

    /// Revoke a PAT by id, scoped to its owner so one user can't revoke another's.
    pub async fn revoke_pat(&self, user_id: &str, id: &str) -> Result<(), IdentityError> {
        sqlx::query(
            &self
                .db
                .q("UPDATE personal_access_tokens SET revoked_at = ? WHERE id = ? AND user_id = ?"),
        )
        .bind(asgard_storage::now())
        .bind(id)
        .bind(user_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

fn uuid_simple() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn row_to_user(row: &sqlx::any::AnyRow) -> User {
    User {
        id: row.get("id"),
        username: row.get("username"),
        email: row.get("email"),
        display_name: row.get("display_name"),
        provider: row.get("provider"),
        is_admin: row.get::<i64, _>("is_admin") != 0,
        role: row.get("role"),
        active: row.get::<i64, _>("active") != 0,
        created_at: row.get("created_at"),
    }
}

/// Enterprise directory sync (SAML/SCIM) is not part of the OSS core.
pub mod enterprise {
    #[derive(Debug, thiserror::Error)]
    #[error("enterprise feature not implemented in OSS core: {0}")]
    pub struct NotImplemented(pub &'static str);

    /// Seam for enterprise identity providers (SAML assertions, SCIM provisioning).
    pub trait DirectorySync: Send + Sync {
        fn sync(&self) -> Result<(), NotImplemented>;
    }

    pub struct ScimSync;
    impl DirectorySync for ScimSync {
        fn sync(&self) -> Result<(), NotImplemented> {
            Err(NotImplemented("SCIM provisioning"))
        }
    }

    pub struct SamlSync;
    impl DirectorySync for SamlSync {
        fn sync(&self) -> Result<(), NotImplemented> {
            Err(NotImplemented("SAML assertion consumer"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn svc() -> IdentityService {
        let path = std::env::temp_dir().join(format!("asgard-id-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        IdentityService::new(db)
    }

    #[tokio::test]
    async fn local_user_auth_and_session() {
        let s = svc().await;
        let u = s
            .create_local_user(
                "alice",
                "s3cret-pw",
                Some("alice@example.com"),
                Some("Alice"),
                Role::Admin,
            )
            .await
            .unwrap();
        assert_eq!(u.provider, "local");
        assert!(u.is_admin);
        assert_eq!(u.role, "admin");

        assert!(s.authenticate_local("alice", "wrong").await.is_err());
        let authed = s.authenticate_local("alice", "s3cret-pw").await.unwrap();
        assert_eq!(authed.id, u.id);

        let sess = s.create_session(&u.id, 3600).await.unwrap();
        let via = s.validate_session(&sess.token).await.unwrap();
        assert_eq!(via.id, u.id);

        s.revoke_session(&sess.token).await.unwrap();
        assert!(s.validate_session(&sess.token).await.is_err());
    }

    #[tokio::test]
    async fn duplicate_user_rejected() {
        let s = svc().await;
        s.create_local_user("bob", "pw", None, None, Role::Member)
            .await
            .unwrap();
        assert!(s
            .create_local_user("bob", "pw2", None, None, Role::Member)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn expired_session_rejected() {
        let s = svc().await;
        let u = s
            .create_local_user("carol", "pw", None, None, Role::Member)
            .await
            .unwrap();
        let sess = s.create_session(&u.id, -1).await.unwrap(); // already expired
        assert!(s.validate_session(&sess.token).await.is_err());
    }

    #[tokio::test]
    async fn oidc_provisions_user() {
        let s = svc().await;
        let u = s
            .upsert_oidc_user("dave", Some("dave@example.com"), Some("Dave"), None, false)
            .await
            .unwrap();
        assert_eq!(u.provider, "oidc");
        assert_eq!(u.role, "member"); // default when no role config applies
                                      // idempotent
        let again = s
            .upsert_oidc_user("dave", None, None, None, false)
            .await
            .unwrap();
        assert_eq!(again.id, u.id);
        assert_eq!(again.role, "member");
    }

    #[test]
    fn oidc_target_role_truth_table() {
        let cfg = OidcRoleConfig {
            admin_groups: vec!["platform-admins".into()],
            finance_groups: vec!["finance".into()],
            admin_emails: vec!["boss@corp.com".into()],
            groups_claim: "groups".into(),
        };
        assert!(cfg.authoritative());
        // group drives the role; no group -> demote to member (authoritative)
        assert_eq!(
            cfg.target_role(None, &["platform-admins".into()]),
            Some(Role::Admin)
        );
        assert_eq!(
            cfg.target_role(None, &["finance".into()]),
            Some(Role::Finance)
        );
        assert_eq!(cfg.target_role(Some("x@corp.com"), &[]), Some(Role::Member));
        // admin-email unions in even without the admin group
        assert_eq!(
            cfg.target_role(Some("BOSS@corp.com"), &[]),
            Some(Role::Admin)
        );

        // no group config -> not authoritative; only admin-email promotes
        let promote_only = OidcRoleConfig {
            admin_emails: vec!["boss@corp.com".into()],
            groups_claim: "groups".into(),
            ..Default::default()
        };
        assert!(!promote_only.authoritative());
        assert_eq!(
            promote_only.target_role(Some("boss@corp.com"), &[]),
            Some(Role::Admin)
        );
        assert_eq!(promote_only.target_role(Some("nobody@corp.com"), &[]), None);
    }

    #[tokio::test]
    async fn oidc_authoritative_sync_promotes_and_demotes() {
        let s = svc().await;
        let cfg = OidcRoleConfig {
            admin_groups: vec!["admins".into()],
            groups_claim: "groups".into(),
            ..Default::default()
        };
        // first login in the admin group -> admin
        let u = s
            .upsert_oidc_user(
                "frank",
                Some("frank@corp.com"),
                None,
                cfg.target_role(Some("frank@corp.com"), &["admins".into()]),
                cfg.authoritative(),
            )
            .await
            .unwrap();
        assert_eq!(u.role, "admin");
        // next login without the group -> demoted to member
        let again = s
            .upsert_oidc_user(
                "frank",
                Some("frank@corp.com"),
                None,
                cfg.target_role(Some("frank@corp.com"), &[]),
                cfg.authoritative(),
            )
            .await
            .unwrap();
        assert_eq!(again.id, u.id);
        assert_eq!(again.role, "member");
        assert!(!again.is_admin);
    }

    #[tokio::test]
    async fn oidc_promote_only_never_demotes_or_overrides_manual() {
        let s = svc().await;
        let cfg = OidcRoleConfig {
            admin_emails: vec!["boss@corp.com".into()],
            groups_claim: "groups".into(),
            ..Default::default()
        };
        // admin-email promotes a fresh user
        let u = s
            .upsert_oidc_user(
                "grace",
                Some("boss@corp.com"),
                None,
                cfg.target_role(Some("boss@corp.com"), &[]),
                cfg.authoritative(),
            )
            .await
            .unwrap();
        assert_eq!(u.role, "admin");

        // a non-listed user defaults to member, then a manual UI promotion sticks
        let h = s
            .upsert_oidc_user("heidi", Some("heidi@corp.com"), None, None, false)
            .await
            .unwrap();
        assert_eq!(h.role, "member");
        s.set_role(&h.id, Role::Finance).await.unwrap();
        let h2 = s
            .upsert_oidc_user("heidi", Some("heidi@corp.com"), None, None, false)
            .await
            .unwrap();
        assert_eq!(h2.role, "finance"); // not clobbered on re-login
    }

    #[test]
    fn role_capability_matrix() {
        use Capability::*;
        assert!(Role::Admin.can(ManageUsers));
        assert!(Role::Admin.can(ViewAllCost));
        // Finance is read-only money + audit, nothing else.
        assert!(Role::Finance.can(ViewAllCost));
        assert!(Role::Finance.can(ViewAudit));
        assert!(!Role::Finance.can(ManageUsers));
        assert!(!Role::Finance.can(Provision));
        // Member can only provision and sees only its own (scoped) spend; manager
        // authority is relationship-based, not a global role.
        assert!(Role::Member.can(Provision));
        assert!(!Role::Member.can(ViewAllCost));
        assert!(!Role::Member.can(ApproveRequests));
        // Run-logs (provision_runs) can carry provider secrets, so the read path is
        // ViewAudit-only — a plain member must not reach it.
        assert!(Role::Admin.can(ViewAudit));
        assert!(!Role::Member.can(ViewAudit));
        assert_eq!(Role::parse("FINANCE"), Role::Finance);
        assert_eq!(Role::parse("manager"), Role::Member); // no global manager role
        assert_eq!(Role::parse("nonsense"), Role::Member);
    }

    #[test]
    fn approval_authority_is_manager_or_admin_never_owner() {
        // Admin holds ApproveRequests, so approves regardless of the relationship.
        assert!(may_approve_request(
            Role::Admin,
            "anyone@x.com",
            "mgr@x.com"
        ));
        // The project's manager may approve (case-insensitive).
        assert!(may_approve_request(Role::Member, "mgr@x.com", "mgr@x.com"));
        assert!(may_approve_request(Role::Member, "MGR@x.com", "mgr@x.com"));
        // The owner (a non-manager member) cannot self-approve their own request.
        assert!(!may_approve_request(
            Role::Member,
            "owner@x.com",
            "mgr@x.com"
        ));
        // Finance is read-only money — not an approver, and not the manager here.
        assert!(!may_approve_request(
            Role::Finance,
            "fin@x.com",
            "mgr@x.com"
        ));
        // An empty caller (e.g. a project-key path with no email) never matches a blank
        // manager — approval must not fall open.
        assert!(!may_approve_request(Role::Member, "", ""));
    }

    #[tokio::test]
    async fn inactive_user_cannot_authenticate_or_hold_a_session() {
        let s = svc().await;
        let u = s
            .create_local_user("erin", "pw", None, None, Role::Member)
            .await
            .unwrap();
        let sess = s.create_session(&u.id, 3600).await.unwrap();
        assert!(s.validate_session(&sess.token).await.is_ok());
        s.set_active(&u.id, false).await.unwrap();
        // Existing session and password login both stop working once disabled.
        assert!(s.validate_session(&sess.token).await.is_err());
        assert!(s.authenticate_local("erin", "pw").await.is_err());
    }

    #[tokio::test]
    async fn set_role_promotes_and_syncs_is_admin() {
        let s = svc().await;
        let u = s
            .create_local_user("frank", "pw", None, None, Role::Member)
            .await
            .unwrap();
        s.set_role(&u.id, Role::Admin).await.unwrap();
        let got = s.get_user(&u.id).await.unwrap().unwrap();
        assert_eq!(got.role, "admin");
        assert!(got.is_admin);
    }

    #[tokio::test]
    async fn pat_roundtrip_revoke_and_expiry() {
        let s = svc().await;
        let u = s
            .create_local_user("gabe", "pw", Some("gabe@example.com"), None, Role::Member)
            .await
            .unwrap();
        let pat = s.create_pat(&u.id, "agent", None).await.unwrap();
        let token = pat.token.clone().unwrap();
        assert!(token.starts_with(PAT_PREFIX));
        // Valid PAT resolves to the user.
        let via = s.validate_pat(&token).await.unwrap();
        assert_eq!(via.id, u.id);
        // Listed (without plaintext).
        let list = s.list_pats(&u.id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].token.is_none());
        // Revoked PAT is rejected.
        s.revoke_pat(&u.id, &pat.id).await.unwrap();
        assert!(s.validate_pat(&token).await.is_err());
        // Expired PAT is rejected.
        let expired = s.create_pat(&u.id, "old", Some(-1)).await.unwrap();
        assert!(s.validate_pat(&expired.token.unwrap()).await.is_err());
        // A disabled user's PAT stops working.
        let fresh = s.create_pat(&u.id, "live", None).await.unwrap();
        assert!(s.validate_pat(fresh.token.as_ref().unwrap()).await.is_ok());
        s.set_active(&u.id, false).await.unwrap();
        assert!(s.validate_pat(&fresh.token.unwrap()).await.is_err());
    }

    #[test]
    fn enterprise_paths_are_stubbed() {
        use enterprise::DirectorySync;
        assert!(enterprise::ScimSync.sync().is_err());
        assert!(enterprise::SamlSync.sync().is_err());
    }
}
