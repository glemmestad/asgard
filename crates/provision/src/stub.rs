//! Dry-run connector. Computes the tag set and a cost estimate and returns
//! deterministic outputs without touching any cloud — enough to drive the full
//! request → approve → fulfill → cost loop in tests and demos. It is also the
//! fallback when a manifest's declared connector isn't registered in this
//! deployment, so the single binary works out of the box. Declared
//! `secret_outputs` get generated values routed to the secret store by the
//! caller; only refs land in the resource record.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::{
    secrets, Plan, ProvisionError, ProvisionRequest, Provisioned, Provisioner, RunLogStore,
};

#[derive(Default)]
pub struct StubProvisioner {
    run_log: Option<Arc<RunLogStore>>,
}

impl StubProvisioner {
    pub fn new() -> Self {
        StubProvisioner { run_log: None }
    }

    /// Attach the run-log store so the dry-run path also records a (synthetic) run
    /// entry — keeps the audit surface demonstrable without a real cloud backend.
    pub fn with_run_log(mut self, store: Arc<RunLogStore>) -> Self {
        self.run_log = Some(store);
        self
    }

    async fn record_run(&self, req: &ProvisionRequest, action: &str, output: &str) {
        if let (Some(store), Some(rid)) = (&self.run_log, &req.resource_id) {
            let now = frontkeep_storage::now();
            let _ = store
                .append(rid, &req.ctx.project_id, action, true, output, &now, &now)
                .await;
        }
    }
}

#[async_trait]
impl Provisioner for StubProvisioner {
    fn name(&self) -> &str {
        "stub"
    }

    fn dry_run(&self) -> bool {
        true
    }

    fn supports(&self, _resource_type: &str) -> bool {
        true
    }

    async fn plan(&self, req: &ProvisionRequest) -> Result<Plan, ProvisionError> {
        Ok(Plan {
            summary: format!(
                "[dry-run] would create {} '{}' for {}",
                req.resource_type, req.name, req.ctx.project_id
            ),
            tags: req.ctx.tags(),
            estimated_monthly_usd: req.estimated_monthly_usd,
        })
    }

    async fn apply(
        &self,
        req: &ProvisionRequest,
        _plan: &Plan,
    ) -> Result<Provisioned, ProvisionError> {
        let arn = format!(
            "arn:stub:{}:{}:{}",
            req.resource_type, req.ctx.project_id, req.name
        );
        let mut outputs = Map::new();
        outputs.insert("arn".into(), Value::String(arn.clone()));
        match req.resource_type.as_str() {
            "s3-bucket" => {
                outputs.insert("bucket".into(), Value::String(req.name.clone()));
            }
            "rds-postgres" => {
                outputs.insert(
                    "endpoint".into(),
                    Value::String(format!("{}.stub.local:5432", req.name)),
                );
            }
            _ => {}
        }
        // Declared secret outputs get freshly generated values; the caller moves
        // them to the secret store and records only a ref.
        for key in &req.secret_outputs {
            outputs.insert(key.clone(), Value::String(secrets::random_secret()));
        }
        self.record_run(
            req,
            "apply",
            &format!(
                "[dry-run] applied {} '{}'\n{}",
                req.resource_type, req.name, arn
            ),
        )
        .await;
        Ok(Provisioned {
            outputs: Value::Object(outputs),
            resource_ids: vec![arn],
            sensitive_keys: req.secret_outputs.clone(),
        })
    }

    async fn destroy(
        &self,
        req: &ProvisionRequest,
        _outputs: &Value,
    ) -> Result<(), ProvisionError> {
        self.record_run(
            req,
            "destroy",
            &format!("[dry-run] destroyed {} '{}'", req.resource_type, req.name),
        )
        .await;
        Ok(())
    }

    /// Simulate a successful suspend/resume so the governed kill→un-kill loop is
    /// exercisable end-to-end without an armed cloud backend.
    async fn stop(
        &self,
        _req: &ProvisionRequest,
        _outputs: &Value,
    ) -> Result<bool, ProvisionError> {
        Ok(true)
    }

    async fn resume(
        &self,
        _req: &ProvisionRequest,
        _outputs: &Value,
    ) -> Result<bool, ProvisionError> {
        Ok(true)
    }
}
