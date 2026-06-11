-- 0005: role-based access + user deactivation.
-- A user's authority is a single role (admin, finance, manager, member) mapped to
-- capabilities in code. `active` lets an operator disable an account (notably an
-- OIDC user, whose identity the IdP owns but whose access Frontkeep governs) without
-- deleting its history. Existing admins (is_admin=1) become role 'admin' so the
-- ladder upgrade is seamless.
-- ALTER ADD COLUMN with constant defaults is portable across SQLite and Postgres.
-- (Keep semicolons out of comments here, the migration splitter splits on them.)
ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'member';
ALTER TABLE users ADD COLUMN active INTEGER NOT NULL DEFAULT 1;
UPDATE users SET role = 'admin' WHERE is_admin = 1;
