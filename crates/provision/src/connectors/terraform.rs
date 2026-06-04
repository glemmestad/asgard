//! Terraform connector — the universal, unrestricted provisioning path. The
//! "how" lives in hub-supplied TF modules, so any AWS/GCP/Azure (or other)
//! resource Terraform can express is reachable with zero Asgard code per
//! service. The connector materializes a per-resource working dir, writes tfvars
//! from the request spec, injects the immutable project tags as a `tags`
//! variable, runs `init`/`apply`, and captures `terraform output -json`.
//! Sensitive outputs are reported as `sensitive_keys` so the caller routes their
//! values to the secret store — they never land in the resource record.
//!
//! State is snapshotted into Asgard's DB (encrypted) around every apply/destroy
//! via an attached [`TfStateStore`], so it survives an ephemeral `work_root`; the
//! work dir under `work_root/<project>/<type>/<name>` is just scratch. Without a
//! store attached (tests) state stays local to the work dir.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::process::Command;
use tokio::sync::Mutex;

use asgard_storage::leases::Leases;

use crate::{Plan, ProvisionError, ProvisionRequest, Provisioned, Provisioner, TfStateStore};

/// Renews a held lease in the background while a long terraform run executes, and
/// stops on drop. We never block on an async release in `Drop`: a graceful path
/// releases the lease explicitly, and a crash leaves it to expire by TTL.
struct RenewGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl RenewGuard {
    fn spawn(leases: Leases, name: String, ttl_secs: i64) -> Self {
        let handle = tokio::spawn(async move {
            let period = std::time::Duration::from_secs((ttl_secs / 3).max(1) as u64);
            loop {
                tokio::time::sleep(period).await;
                if !matches!(leases.renew(&name, ttl_secs).await, Ok(true)) {
                    // Lost the lease (paused past the TTL) or a DB hiccup: stop
                    // renewing. The tf_state version CAS still guards the persist.
                    tracing::error!(
                        "tf lease {name} renew failed or lost; relying on state version CAS"
                    );
                    break;
                }
            }
        });
        RenewGuard { handle }
    }
}

impl Drop for RenewGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub struct TerraformConnector {
    bin: String,
    modules_dir: PathBuf,
    work_root: PathBuf,
    /// Durable state store. When set, each resource's `terraform.tfstate` is
    /// hydrated from the DB before a run and snapshotted back after, so state
    /// survives an ephemeral `work_root`. `None` keeps state local to `work_root`
    /// (the zero-dependency default used in tests).
    state: Option<Arc<TfStateStore>>,
    /// Per-state-id locks: serialize hydrate→run→persist for one resource within
    /// this process. The cross-instance `leases` lock below then only ever has to
    /// contend across processes.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Cross-instance lock so two replicas can't run an apply against one
    /// resource's state at once. `None` keeps the in-process lock only (MCP stdio
    /// and tests, which are never a server replica).
    leases: Option<Leases>,
    /// TTL for the per-resource lease, renewed at a third of this while an apply
    /// runs (so a multi-minute apply keeps the lock) and freed by expiry on crash.
    tf_lease_ttl: i64,
}

impl TerraformConnector {
    pub fn new(bin: impl Into<String>, modules_dir: PathBuf, work_root: PathBuf) -> Self {
        TerraformConnector {
            bin: bin.into(),
            modules_dir,
            work_root,
            state: None,
            locks: Mutex::new(HashMap::new()),
            leases: None,
            tf_lease_ttl: 600,
        }
    }

    /// Attach a durable state store so state persists in the DB across runs and
    /// ephemeral work dirs.
    pub fn with_state(mut self, store: Arc<TfStateStore>) -> Self {
        self.state = Some(store);
        self
    }

    /// Attach a cross-instance lease so multiple replicas serialize applies for
    /// the same resource (on top of the in-process lock). `ttl_secs` is the lease
    /// lifetime, renewed while an apply runs.
    pub fn with_leases(mut self, leases: Leases, ttl_secs: i64) -> Self {
        self.leases = Some(leases);
        self.tf_lease_ttl = ttl_secs;
        self
    }

    /// Acquire the cross-instance lease for `id` and return a guard that renews it
    /// until dropped. `Ok(None)` when no leases are wired. Errors if another
    /// replica holds it (retryable — the request stays Approved).
    async fn acquire_remote(&self, id: &str) -> Result<Option<RenewGuard>, ProvisionError> {
        let Some(leases) = &self.leases else {
            return Ok(None);
        };
        let name = format!("tf:{id}");
        let held = leases
            .try_acquire(&name, self.tf_lease_ttl)
            .await
            .map_err(|e| ProvisionError::Backend(format!("tf lease: {e}")))?;
        if !held {
            return Err(ProvisionError::Conflict(format!(
                "resource {id} is being provisioned by another instance; retry shortly"
            )));
        }
        Ok(Some(RenewGuard::spawn(
            leases.clone(),
            name,
            self.tf_lease_ttl,
        )))
    }

    /// Release the cross-instance lease for `id` so the next acquirer needn't wait
    /// out the TTL (no-op without leases).
    async fn release_remote(&self, id: &str) {
        if let Some(leases) = &self.leases {
            let _ = leases.release(&format!("tf:{id}")).await;
        }
    }

    fn state_id(req: &ProvisionRequest) -> String {
        format!("{}/{}/{}", req.ctx.project_id, req.resource_type, req.name)
    }

    async fn lock_for(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let m = {
            let mut map = self.locks.lock().await;
            map.entry(id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        m.lock_owned().await
    }

    /// Write the DB-held state into the working dir before a run and return its
    /// version for the persist-time CAS (no-op / `None` without a store or any
    /// stored state).
    async fn hydrate(&self, id: &str, wd: &Path) -> Result<Option<i64>, ProvisionError> {
        let Some(store) = &self.state else {
            return Ok(None);
        };
        if let Some((bytes, version)) = store.load(id).await? {
            std::fs::write(wd.join("terraform.tfstate"), bytes)
                .map_err(|e| ProvisionError::Backend(format!("write tf state: {e}")))?;
            Ok(Some(version))
        } else {
            Ok(None)
        }
    }

    /// Snapshot the working dir's state back into the DB after a run, as a
    /// compare-and-swap on the `expected` version hydrate returned (no-op without a
    /// store; an absent state file is treated as empty and skipped). A `Conflict`
    /// means another instance advanced the state — we log loudly and write nothing.
    async fn persist(
        &self,
        id: &str,
        wd: &Path,
        expected: Option<i64>,
    ) -> Result<(), ProvisionError> {
        let Some(store) = &self.state else {
            return Ok(());
        };
        match std::fs::read(wd.join("terraform.tfstate")) {
            Ok(bytes) => match store.save_cas(id, &bytes, expected).await {
                Err(ProvisionError::Conflict(msg)) => {
                    tracing::error!(
                        "tf state {id} not persisted: {msg}; another instance advanced it (local apply may be drift)"
                    );
                    Err(ProvisionError::Conflict(msg))
                }
                other => other,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ProvisionError::Backend(format!("read tf state: {e}"))),
        }
    }

    /// Materialize the working dir: copy the module, write tfvars, hydrate state
    /// from the DB, and `init`. Shared by apply and destroy.
    async fn prepare(
        &self,
        req: &ProvisionRequest,
        plan: &Plan,
        overrides: &Value,
    ) -> Result<(PathBuf, Option<i64>), ProvisionError> {
        let module = self.module_path(&req.config)?;
        let wd = self.work_dir(req);
        copy_module(&module, &wd)?;
        let vars = tfvars(req, plan, overrides);
        std::fs::write(
            wd.join("asgard.auto.tfvars.json"),
            serde_json::to_vec_pretty(&vars).unwrap_or_default(),
        )
        .map_err(|e| ProvisionError::Backend(format!("write tfvars: {e}")))?;
        let version = self.hydrate(&Self::state_id(req), &wd).await?;
        self.run(&wd, &["init", "-input=false", "-no-color"])
            .await?;
        Ok((wd, version))
    }

    fn module_path(&self, config: &Value) -> Result<PathBuf, ProvisionError> {
        let m = config
            .get("module")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ProvisionError::Backend("terraform connector needs config.module".into())
            })?;
        let p = Path::new(m);
        Ok(if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.modules_dir.join(m)
        })
    }

    fn work_dir(&self, req: &ProvisionRequest) -> PathBuf {
        self.work_root
            .join(&req.ctx.project_id)
            .join(&req.resource_type)
            .join(&req.name)
    }

    /// Take both locks (in-process, then cross-instance) around an apply and
    /// release the cross-instance lease afterwards on every path.
    async fn apply_with(
        &self,
        req: &ProvisionRequest,
        plan: &Plan,
        overrides: &Value,
    ) -> Result<Provisioned, ProvisionError> {
        let id = Self::state_id(req);
        let _guard = self.lock_for(&id).await;
        let renew = self.acquire_remote(&id).await?;
        let result = self.apply_inner(req, plan, overrides, &id).await;
        drop(renew);
        self.release_remote(&id).await;
        result
    }

    /// Materialize the working dir and `terraform apply` the module with the spec
    /// tfvars layered with `overrides` (empty for a normal apply; the suspend vars
    /// for a stop). Captures `output -json`. The same path provisions, suspends,
    /// and resumes — only the override layer differs.
    async fn apply_inner(
        &self,
        req: &ProvisionRequest,
        plan: &Plan,
        overrides: &Value,
        id: &str,
    ) -> Result<Provisioned, ProvisionError> {
        let (wd, version) = self.prepare(req, plan, overrides).await?;
        let applied = self
            .run(
                &wd,
                &["apply", "-auto-approve", "-input=false", "-no-color"],
            )
            .await;
        // Persist whatever state exists — even after a partial-apply failure —
        // before surfacing the apply error, or we'd lose track of created
        // resources and orphan them.
        if let Err(e) = self.persist(id, &wd, version).await {
            applied?;
            return Err(e);
        }
        applied?;
        let out = self.run(&wd, &["output", "-json", "-no-color"]).await?;
        let raw: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);

        let mut outputs = Map::new();
        let mut sensitive_keys = Vec::new();
        if let Some(obj) = raw.as_object() {
            for (k, decl) in obj {
                let value = decl.get("value").cloned().unwrap_or(Value::Null);
                if decl.get("sensitive").and_then(|s| s.as_bool()) == Some(true) {
                    sensitive_keys.push(k.clone());
                }
                outputs.insert(k.clone(), value);
            }
        }
        Ok(Provisioned {
            outputs: Value::Object(outputs),
            resource_ids: vec![],
            sensitive_keys,
        })
    }

    /// Rebuild the working dir, `terraform destroy`, and drop the stored state.
    async fn destroy_inner(&self, req: &ProvisionRequest, id: &str) -> Result<(), ProvisionError> {
        let has_stored = match &self.state {
            Some(s) => s.exists(id).await?,
            None => false,
        };
        // Nothing was ever provisioned: no local working dir and no stored state.
        if !self.work_dir(req).exists() && !has_stored {
            return Ok(());
        }
        // Rebuild the working dir from the module + hydrated state — after a
        // restart the dir is gone but the state lives in the DB.
        let plan = self.plan(req).await?;
        let (wd, version) = self.prepare(req, &plan, &Value::Null).await?;
        let destroyed = self
            .run(
                &wd,
                &["destroy", "-auto-approve", "-input=false", "-no-color"],
            )
            .await;
        let _ = self.persist(id, &wd, version).await;
        destroyed?;
        if let Some(s) = &self.state {
            let _ = s.delete(id).await;
        }
        Ok(())
    }

    async fn run(&self, dir: &Path, args: &[&str]) -> Result<std::process::Output, ProvisionError> {
        let chdir = format!("-chdir={}", dir.display());
        let mut full = vec![chdir.as_str()];
        full.extend_from_slice(args);
        let out = Command::new(&self.bin)
            .args(&full)
            .output()
            .await
            .map_err(|e| ProvisionError::Backend(format!("spawn terraform: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(ProvisionError::Backend(format!(
                "terraform {} failed: {}",
                args.first().copied().unwrap_or(""),
                stderr.trim()
            )));
        }
        Ok(out)
    }
}

/// Copy module files into the working dir (skipping persisted state + the
/// `.terraform` cache) so re-apply refreshes the module without clobbering state.
fn copy_module(src: &Path, dst: &Path) -> Result<(), ProvisionError> {
    std::fs::create_dir_all(dst)
        .map_err(|e| ProvisionError::Backend(format!("mkdir {}: {e}", dst.display())))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| ProvisionError::Backend(format!("read module {}: {e}", src.display())))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".terraform" || name_str.starts_with("terraform.tfstate") {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_module(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| ProvisionError::Backend(format!("copy {}: {e}", from.display())))?;
        }
    }
    Ok(())
}

/// Build the tfvars: every top-level spec field, plus `name`, the immutable
/// project `tags` map, and any `overrides` (a suspend/resume re-apply layers
/// these over the spec — e.g. `desired_count: 0`).
fn tfvars(req: &ProvisionRequest, plan: &Plan, overrides: &Value) -> Value {
    let mut m = match &req.spec {
        Value::Object(o) => o.clone(),
        _ => Map::new(),
    };
    m.insert("name".into(), Value::String(req.name.clone()));
    let tags: Map<String, Value> = plan
        .tags
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    m.insert("tags".into(), Value::Object(tags));
    if let Value::Object(o) = overrides {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

#[async_trait]
impl Provisioner for TerraformConnector {
    fn name(&self) -> &str {
        "terraform"
    }
    fn dry_run(&self) -> bool {
        false
    }
    fn supports(&self, _resource_type: &str) -> bool {
        true
    }

    async fn plan(&self, req: &ProvisionRequest) -> Result<Plan, ProvisionError> {
        let module = self.module_path(&req.config)?;
        Ok(Plan {
            summary: format!(
                "terraform apply {} for {}/{}",
                module.display(),
                req.ctx.project_id,
                req.name
            ),
            tags: req.ctx.tags(),
            estimated_monthly_usd: req.estimated_monthly_usd,
        })
    }

    async fn apply(
        &self,
        req: &ProvisionRequest,
        plan: &Plan,
    ) -> Result<Provisioned, ProvisionError> {
        self.apply_with(req, plan, &Value::Null).await
    }

    async fn destroy(
        &self,
        req: &ProvisionRequest,
        _outputs: &Value,
    ) -> Result<(), ProvisionError> {
        let id = Self::state_id(req);
        let _guard = self.lock_for(&id).await;
        let renew = self.acquire_remote(&id).await?;
        let result = self.destroy_inner(req, &id).await;
        drop(renew);
        self.release_remote(&id).await;
        result
    }

    /// Suspend by re-applying the module with `config.stop_tfvars` layered over
    /// the spec (e.g. `desired_count: 0`). A service with no `stop_tfvars` has no
    /// meaningful stop → `false`.
    async fn stop(&self, req: &ProvisionRequest, _outputs: &Value) -> Result<bool, ProvisionError> {
        let Some(overrides) = req.config.get("stop_tfvars") else {
            return Ok(false);
        };
        let plan = self.plan(req).await?;
        self.apply_with(req, &plan, overrides).await?;
        Ok(true)
    }

    /// Resume by re-applying the module with the original spec (no overrides),
    /// restoring the pre-stop state. Only meaningful for a stoppable service.
    async fn resume(
        &self,
        req: &ProvisionRequest,
        _outputs: &Value,
    ) -> Result<bool, ProvisionError> {
        if req.config.get("stop_tfvars").is_none() {
            return Ok(false);
        }
        let plan = self.plan(req).await?;
        self.apply_with(req, &plan, &Value::Null).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResourceContext;
    use std::collections::BTreeMap;

    fn req() -> ProvisionRequest {
        ProvisionRequest {
            resource_type: "ecs-service".into(),
            name: "web".into(),
            ctx: ResourceContext {
                project_id: "proj-x".into(),
                owner: "o".into(),
                manager: "m".into(),
                group: "g".into(),
                cost_center: "cc".into(),
                classification: "poc".into(),
                environment: "dev".into(),
                cloud: "aws".into(),
                account: "123".into(),
            },
            spec: serde_json::json!({"name": "web", "desired_count": 2}),
            config: serde_json::json!({"stop_tfvars": {"desired_count": 0}}),
            estimated_monthly_usd: 35.0,
            secret_outputs: vec![],
        }
    }

    fn plan() -> Plan {
        Plan {
            summary: String::new(),
            tags: BTreeMap::new(),
            estimated_monthly_usd: 35.0,
        }
    }

    #[test]
    fn stop_tfvars_override_the_spec() {
        let r = req();
        let suspend = tfvars(&r, &plan(), &r.config["stop_tfvars"]);
        assert_eq!(suspend["desired_count"], serde_json::json!(0));
        // A normal apply leaves the spec's value intact.
        let normal = tfvars(&r, &plan(), &Value::Null);
        assert_eq!(normal["desired_count"], serde_json::json!(2));
    }

    /// The durability guarantee, without needing terraform installed: state
    /// written to the work dir survives the work dir being wiped, because it was
    /// snapshotted into the DB and is re-hydrated on the next run.
    #[tokio::test]
    async fn state_survives_an_ephemeral_work_dir() {
        use asgard_storage::Db;

        let dbpath =
            std::env::temp_dir().join(format!("asgard-tf-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", dbpath.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let work_root =
            std::env::temp_dir().join(format!("asgard-tfwork-{}", asgard_storage::new_uid()));
        let conn = TerraformConnector::new("terraform", PathBuf::from("/modules"), work_root)
            .with_state(Arc::new(TfStateStore::new(db, [0x33; 32])));

        let r = req();
        let id = TerraformConnector::state_id(&r);
        let wd = conn.work_dir(&r);
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::write(wd.join("terraform.tfstate"), b"{\"serial\":7}").unwrap();

        // Snapshot into the DB, then simulate compute replacement: wipe the dir.
        conn.persist(&id, &wd, None).await.unwrap();
        std::fs::remove_dir_all(&wd).unwrap();
        std::fs::create_dir_all(&wd).unwrap();
        assert!(!wd.join("terraform.tfstate").exists());

        // Re-hydrate from the DB restores the exact state.
        assert_eq!(conn.hydrate(&id, &wd).await.unwrap(), Some(1));
        assert_eq!(
            std::fs::read(wd.join("terraform.tfstate")).unwrap(),
            b"{\"serial\":7}"
        );
    }

    /// When another replica holds a resource's lease, this connector refuses to
    /// apply rather than racing its state — no terraform needed, the lock is
    /// checked before the working dir is touched.
    #[tokio::test]
    async fn cross_instance_lease_blocks_concurrent_apply() {
        use asgard_storage::leases::Leases;
        use asgard_storage::Db;

        let dbpath =
            std::env::temp_dir().join(format!("asgard-tflease-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", dbpath.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();

        let r = req();
        let id = TerraformConnector::state_id(&r);
        let other = Leases::new(db.clone(), "instance-a");
        assert!(other.try_acquire(&format!("tf:{id}"), 60).await.unwrap());

        let conn =
            TerraformConnector::new("terraform", PathBuf::from("/modules"), std::env::temp_dir())
                .with_leases(Leases::new(db.clone(), "instance-b"), 60);
        assert!(matches!(
            conn.apply(&r, &plan()).await,
            Err(ProvisionError::Conflict(_))
        ));
    }
}
