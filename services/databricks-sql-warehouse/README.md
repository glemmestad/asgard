# Databricks SQL Warehouse

Provisions a `databricks_sql_endpoint` via the universal terraform connector +
the Databricks provider (auth from `DATABRICKS_HOST`/`DATABRICKS_TOKEN` in the
control plane's env). Review-tier (cost-bearing).

**Spec fields** (become tfvars): `name`, `cluster_size` (e.g. `Small`),
optional `auto_stop_mins`, `max_num_clusters`, `enable_serverless_compute`,
`warehouse_type` (`PRO`/`CLASSIC`). The immutable `project` tag is stamped as a
warehouse `custom_tag`, so `system.billing.usage` attributes spend per project
(the `databricks-billing` cost source).

**Outputs**: `id`, `jdbc_url`, `http_path`.

> Kill is a no-op for a warehouse (it auto-stops when idle); set a low
> `auto_stop_mins` to cap idle spend. Decommission destroys it.
