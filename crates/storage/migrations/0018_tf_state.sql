-- 0018: durable Terraform state. The connector snapshots each resource's
-- terraform.tfstate into this table around every apply/destroy, so state lives in
-- the same DB as everything else and survives ephemeral compute (the work_dir
-- becomes scratch). The blob is envelope-encrypted (AES-256-GCM) with the
-- secret-store master key, since state carries provider secrets in the clear.
-- (Keep semicolons out of comments here, the migration splitter splits on them.)
CREATE TABLE IF NOT EXISTS tf_state (
  id TEXT PRIMARY KEY,
  ciphertext TEXT NOT NULL,
  nonce TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
