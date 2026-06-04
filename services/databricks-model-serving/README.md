# Databricks Model Serving Endpoint

Provisions a `databricks_model_serving` endpoint via the terraform connector + the
Databricks provider. Review-tier (serving compute).

**Spec fields**: `name`, `entity_name` (registered model / UC model name),
optional `entity_version`, `workload_size` (`Small`/`Medium`/`Large`),
`scale_to_zero_enabled`. The `project` tag is stamped on the endpoint and
propagates to billing.

**Outputs**: `id` (endpoint name), `serving_endpoint_id`.

**Closes the loop with inference**: once created, add the endpoint name to the
`databricks` inference module's `models[]` (as a `route`) so the gateway
fronts it. (Auto-registration of provisioned endpoints is a roadmap item.)
