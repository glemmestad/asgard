-- 0023: async provisioning. A provisioned_resources row in a work state
-- (provisioning or destroying) is the durable work item. worker_owner is a
-- row-lease (NULL means unclaimed) and updated_at doubles as the claim
-- heartbeat. error carries the failure detail for failed/destroy_failed rows.
-- ALTER ADD COLUMN with a constant/NULL default is portable across SQLite and
-- Postgres. Keep semicolons out of these comments — the splitter splits on them.
ALTER TABLE provisioned_resources ADD COLUMN worker_owner TEXT;
ALTER TABLE provisioned_resources ADD COLUMN error TEXT NOT NULL DEFAULT '';
