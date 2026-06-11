//! Terraform connector — the universal, unrestricted provisioning path. The
//! "how" lives in hub-supplied TF modules, so any AWS/GCP/Azure (or other)
//! resource Terraform can express is reachable with zero Frontkeep code per
//! service. The connector materializes a per-resource working dir, writes tfvars
//! from the request spec, injects the immutable project tags as a `tags`
//! variable, runs `init`/`apply`, and captures `terraform output -json`.
//! Sensitive outputs are reported as `sensitive_keys` so the caller routes their
//! values to the secret store — they never land in the resource record.
//!
//! State is snapshotted into Frontkeep's DB (encrypted) around every apply/destroy
//! via an attached [`TfStateStore`], so it survives an ephemeral `work_root`; the
//! work dir under `work_root/<project>/<type>/<name>` is just scratch. Without a
//! store attached (tests) state stays local to the work dir.
//!
//! Disk is bounded two ways so the control-plane volume can't fill as resources
//! accumulate. First, a shared `TF_PLUGIN_CACHE_DIR` ([`with_plugin_cache`]) means
//! one provider copy serves every working dir instead of each `init` carrying its
//! own ~600 MB download. Second, with a durable state store attached the scratch
//! work dir is removed after every apply/destroy ([`gc_work_dir`]) — state lives
//! in the DB, so each dir is rebuilt on its next run. Together these cap usage at
//! the single cache plus whatever is actively running, regardless of how many
//! distinct resource names (e.g. per-pipeline credentials) are ever provisioned.
//!
//! [`with_plugin_cache`]: TerraformConnector::with_plugin_cache
//! [`gc_work_dir`]: TerraformConnector::gc_work_dir

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::process::Command;
use tokio::sync::Mutex;

use frontkeep_storage::leases::Leases;

use crate::{
    Plan, ProvisionError, ProvisionRequest, Provisioned, Provisioner, RunLogStore, TfStateStore,
};

/// Wall-clock backstop for a single terraform invocation. A run that exceeds it
/// is killed and surfaced as an error, so a hung apply becomes a recorded
/// `failed` row instead of a worker that heartbeats forever and pins a claim.
/// Generous on purpose — this catches true hangs, not slow-but-progressing
/// infra; legitimate applies finish well inside an hour.
const RUN_TIMEOUT_SECS: u64 = 3600;

/// Per-stream cap on captured stdout/stderr. The stream is fully drained (so the
/// child never blocks on a full pipe) but only this many bytes are retained,
/// bounding the control-plane heap against a pathological apply that floods
/// output. The audit log keeps the head and a truncation marker.
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

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
    /// Captures the full plan+apply / destroy log per resource for operator audit.
    /// `None` keeps runs unlogged (tests).
    run_log: Option<Arc<RunLogStore>>,
    /// Shared provider plugin cache exported as `TF_PLUGIN_CACHE_DIR` to every
    /// terraform run, so one ~600 MB provider download is symlinked into all
    /// working dirs instead of each `init` fetching its own. `None` leaves
    /// terraform's per-workdir default (tests). The cache is concurrency-safe for
    /// reads; the only race is the first fetch of a given provider version (rare,
    /// and terraform retries) — a non-issue against a volume that fills otherwise.
    plugin_cache: Option<PathBuf>,
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
            run_log: None,
            plugin_cache: None,
        }
    }

    /// Attach a durable state store so state persists in the DB across runs and
    /// ephemeral work dirs.
    pub fn with_state(mut self, store: Arc<TfStateStore>) -> Self {
        self.state = Some(store);
        self
    }

    /// Attach the run-log store so each apply/destroy's combined output is captured
    /// (per resource) for the operator audit view.
    pub fn with_run_log(mut self, store: Arc<RunLogStore>) -> Self {
        self.run_log = Some(store);
        self
    }

    /// Point every terraform run at a shared provider plugin cache so providers
    /// are downloaded once and symlinked into each working dir, rather than every
    /// resource carrying its own ~600 MB copy. The directory must already exist
    /// (terraform silently skips caching otherwise) — the caller creates it.
    pub fn with_plugin_cache(mut self, dir: PathBuf) -> Self {
        self.plugin_cache = Some(dir);
        self
    }

    /// One-shot startup sweep: drop the per-resource scratch dirs left under
    /// `work_root` (keeping any dot-prefixed internals such as the plugin cache).
    /// Safe only with a durable state store — there the dirs are reconstructable
    /// from the DB on their next run, so this reclaims the disk that orphaned or
    /// pre-cache work dirs (each historically pinning its own provider copy) would
    /// otherwise hold until the volume fills. No-op without a store (tests).
    pub fn prune_work_root(&self) {
        if self.state.is_none() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.work_root) else {
            return;
        };
        let mut reclaimed = 0u32;
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if std::fs::remove_dir_all(entry.path()).is_ok() {
                reclaimed += 1;
            }
        }
        if reclaimed > 0 {
            tracing::info!(
                "tf startup reclaim: removed {reclaimed} stale work dir(s) under {}",
                self.work_root.display()
            );
        }
    }

    /// Remove a resource's scratch working dir once its state is safely in the DB.
    /// Without this every distinct resource name leaves a dir behind forever; with
    /// it, disk is bounded to the shared plugin cache plus what is actively
    /// running. No-op without a durable state store — there the dir *is* the state.
    fn gc_work_dir(&self, wd: &Path) {
        if self.state.is_none() {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(wd) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("tf work dir gc {}: {e}", wd.display());
            }
        }
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
        wd: &Path,
    ) -> Result<Option<i64>, ProvisionError> {
        let module = self.module_path(&req.config)?;
        copy_module(&module, wd)?;
        let vars = tfvars(req, plan, overrides);
        std::fs::write(
            wd.join("frontkeep.auto.tfvars.json"),
            serde_json::to_vec_pretty(&vars).unwrap_or_default(),
        )
        .map_err(|e| ProvisionError::Backend(format!("write tfvars: {e}")))?;
        let version = self.hydrate(&Self::state_id(req), wd).await?;
        self.run(wd, &["init", "-input=false", "-no-color"]).await?;
        Ok(version)
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
        let wd = self.work_dir(req);
        let result = self.apply_in_dir(req, plan, overrides, id, &wd).await;
        // Scratch dir served its purpose; state is in the DB. Reclaim on every
        // path so live-but-never-destroyed resources don't accumulate on disk.
        self.gc_work_dir(&wd);
        result
    }

    async fn apply_in_dir(
        &self,
        req: &ProvisionRequest,
        plan: &Plan,
        overrides: &Value,
        id: &str,
        wd: &Path,
    ) -> Result<Provisioned, ProvisionError> {
        let version = self.prepare(req, plan, overrides, wd).await?;
        let started = frontkeep_storage::now();
        let (log, applied) = self
            .run_capture(wd, &["apply", "-auto-approve", "-input=false", "-no-color"])
            .await;
        // Capture the full plan+apply log against the resource for operator audit,
        // success or failure, before anything else can short-circuit.
        self.record_run(req, "apply", applied.is_ok(), &log, &started)
            .await;
        // Persist whatever state exists — even after a partial-apply failure —
        // before surfacing the apply error, or we'd lose track of created
        // resources and orphan them.
        if let Err(e) = self.persist(id, wd, version).await {
            applied?;
            return Err(e);
        }
        applied?;
        let out = self.run(wd, &["output", "-json", "-no-color"]).await?;
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
        let wd = self.work_dir(req);
        // Nothing was ever provisioned: no local working dir and no stored state.
        if !wd.exists() && !has_stored {
            return Ok(());
        }
        let result = self.destroy_in_dir(req, id, &wd).await;
        // Reclaim the scratch dir whether destroy succeeded or failed: state stays
        // in the DB on failure so a retry rebuilds the dir, and a self-deadlocked
        // host can't destroy at all if every teardown first stages a fresh ~600 MB
        // workspace it has no room for.
        self.gc_work_dir(&wd);
        result
    }

    async fn destroy_in_dir(
        &self,
        req: &ProvisionRequest,
        id: &str,
        wd: &Path,
    ) -> Result<(), ProvisionError> {
        // Rebuild the working dir from the module + hydrated state — after a
        // restart the dir is gone but the state lives in the DB.
        let plan = self.plan(req).await?;
        let version = self.prepare(req, &plan, &Value::Null, wd).await?;
        let started = frontkeep_storage::now();
        let (log, destroyed) = self
            .run_capture(
                wd,
                &["destroy", "-auto-approve", "-input=false", "-no-color"],
            )
            .await;
        self.record_run(req, "destroy", destroyed.is_ok(), &log, &started)
            .await;
        let _ = self.persist(id, wd, version).await;
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
        let out = self.run_bounded(&full).await?;
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

    /// Like [`run`](Self::run) but always returns the combined stdout+stderr (the
    /// audit log) alongside the result, so a failed *or timed-out* run's output is
    /// captured rather than reduced to a one-line error (or, on a worker death,
    /// lost entirely).
    async fn run_capture(
        &self,
        dir: &Path,
        args: &[&str],
    ) -> (String, Result<std::process::Output, ProvisionError>) {
        let chdir = format!("-chdir={}", dir.display());
        let mut full = vec![chdir.as_str()];
        full.extend_from_slice(args);
        match self.run_bounded(&full).await {
            Ok(out) => {
                let log = combine_output(&out);
                if out.status.success() {
                    (log, Ok(out))
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let msg = format!(
                        "terraform {} failed: {}",
                        args.first().copied().unwrap_or(""),
                        stderr.trim()
                    );
                    (log, Err(ProvisionError::Backend(msg)))
                }
            }
            // Spawn failure or timeout: capture the error itself as the run log so
            // the operator sees why nothing ran.
            Err(e) => (e.to_string(), Err(e)),
        }
    }

    /// Spawn `terraform <args>` with a wall-clock timeout and a capped output
    /// buffer. The child is killed on drop, so a timeout (the future is dropped)
    /// terminates the process rather than leaking it; both pipes are drained
    /// concurrently to avoid a full-pipe deadlock while retaining only the first
    /// [`MAX_CAPTURE_BYTES`] of each. Bounds the blast radius of a runaway apply to
    /// an `Err` (→ `mark_failed`) instead of an OOM that takes the control plane down.
    async fn run_bounded(&self, args: &[&str]) -> Result<std::process::Output, ProvisionError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cache) = &self.plugin_cache {
            cmd.env("TF_PLUGIN_CACHE_DIR", cache);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| ProvisionError::Backend(format!("spawn terraform: {e}")))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let collect = async {
            let (o, e, status) = tokio::join!(
                read_capped(&mut stdout, MAX_CAPTURE_BYTES),
                read_capped(&mut stderr, MAX_CAPTURE_BYTES),
                child.wait(),
            );
            Ok::<_, std::io::Error>(std::process::Output {
                status: status?,
                stdout: o?,
                stderr: e?,
            })
        };
        match tokio::time::timeout(std::time::Duration::from_secs(RUN_TIMEOUT_SECS), collect).await
        {
            Ok(Ok(out)) => Ok(out),
            Ok(Err(e)) => Err(ProvisionError::Backend(format!("terraform io error: {e}"))),
            Err(_) => Err(ProvisionError::Backend(format!(
                "terraform {} timed out after {RUN_TIMEOUT_SECS}s; killed",
                args.iter()
                    .find(|a| !a.starts_with("-chdir"))
                    .copied()
                    .unwrap_or("")
            ))),
        }
    }

    /// Append one run entry to the resource's audit log (no-op without a store or a
    /// resource id). Best-effort: a logging failure never breaks the provision.
    async fn record_run(
        &self,
        req: &ProvisionRequest,
        action: &str,
        ok: bool,
        output: &str,
        started_at: &str,
    ) {
        if let (Some(store), Some(rid)) = (&self.run_log, &req.resource_id) {
            let finished = frontkeep_storage::now();
            if let Err(e) = store
                .append(
                    rid,
                    &req.ctx.project_id,
                    action,
                    ok,
                    output,
                    started_at,
                    &finished,
                )
                .await
            {
                tracing::warn!("run-log append for {rid} failed: {e}");
            }
        }
    }
}

/// Read an async stream to EOF, retaining at most `cap` bytes (plus a truncation
/// marker if it overflowed) but always draining the rest so the child's pipe
/// never blocks. This is what keeps a flood-of-output apply from ballooning the
/// control-plane heap.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let take = cap.saturating_sub(buf.len()).min(n);
        buf.extend_from_slice(&chunk[..take]);
        if take < n {
            truncated = true;
        }
    }
    if truncated {
        buf.extend_from_slice(format!("\n…[output truncated at {cap} bytes]\n").as_bytes());
    }
    Ok(buf)
}

/// Combined stdout+stderr of a finished command (the human-readable run log).
fn combine_output(out: &std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&err);
    }
    s
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

/// Resolve a manifest `config.defaults` value against the process env. A whole
/// string `${VAR}` becomes the env value; `${VAR:csv}` splits it into a list.
/// Any referenced var that is unset yields `None`, so the default is dropped and
/// the module's own default (or null) applies — a literal `${VAR}` never reaches
/// Terraform. Non-string scalars pass through; arrays resolve element-wise.
fn resolve_default(v: &Value) -> Option<Value> {
    match v {
        Value::String(s) => match s.strip_prefix("${").and_then(|x| x.strip_suffix('}')) {
            Some(inner) => {
                let (name, csv) = match inner.strip_suffix(":csv") {
                    Some(n) => (n, true),
                    None => (inner, false),
                };
                let val = std::env::var(name).ok()?;
                if csv {
                    let items: Vec<Value> = val
                        .split(',')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(|p| Value::String(p.to_string()))
                        .collect();
                    (!items.is_empty()).then_some(Value::Array(items))
                } else {
                    Some(Value::String(val))
                }
            }
            None => Some(Value::String(s.clone())),
        },
        Value::Array(items) => {
            let resolved: Vec<Value> = items.iter().filter_map(resolve_default).collect();
            (!resolved.is_empty()).then_some(Value::Array(resolved))
        }
        other => Some(other.clone()),
    }
}

/// Build the tfvars: manifest `config.defaults` (operator/env-sourced) as the
/// floor, overlaid by every top-level spec field, plus `name`, the immutable
/// project `tags` map, and any `overrides` (a suspend/resume re-apply layers
/// these over the spec — e.g. `desired_count: 0`).
fn tfvars(req: &ProvisionRequest, plan: &Plan, overrides: &Value) -> Value {
    let mut m = Map::new();
    if let Some(Value::Object(defaults)) = req.config.get("defaults") {
        for (k, v) in defaults {
            if let Some(resolved) = resolve_default(v) {
                m.insert(k.clone(), resolved);
            }
        }
    }
    if let Value::Object(o) = &req.spec {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
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
            resource_id: None,
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

    #[test]
    fn config_defaults_source_env_yield_to_spec_and_drop_when_unset() {
        std::env::set_var("FRONTKEEP_TEST_DEF_SUBNET", "subnet-grp-1");
        std::env::set_var("FRONTKEEP_TEST_DEF_SGS", "sg-1, sg-2");
        std::env::remove_var("FRONTKEEP_TEST_DEF_REGION");
        let mut r = req();
        r.spec = serde_json::json!({"name": "db", "instance_class": "db.t3.small"});
        r.config = serde_json::json!({"defaults": {
            "subnet_group_name": "${FRONTKEEP_TEST_DEF_SUBNET}",
            "vpc_security_group_ids": "${FRONTKEEP_TEST_DEF_SGS:csv}",
            "region": "${FRONTKEEP_TEST_DEF_REGION}",
            "instance_class": "db.t3.micro"
        }});
        let v = tfvars(&r, &plan(), &Value::Null);
        assert_eq!(v["subnet_group_name"], serde_json::json!("subnet-grp-1"));
        assert_eq!(
            v["vpc_security_group_ids"],
            serde_json::json!(["sg-1", "sg-2"])
        );
        assert!(v.get("region").is_none(), "unset env default is dropped");
        assert_eq!(
            v["instance_class"],
            serde_json::json!("db.t3.small"),
            "the request spec overrides a manifest default"
        );
        std::env::remove_var("FRONTKEEP_TEST_DEF_SUBNET");
        std::env::remove_var("FRONTKEEP_TEST_DEF_SGS");
    }

    /// The durability guarantee, without needing terraform installed: state
    /// written to the work dir survives the work dir being wiped, because it was
    /// snapshotted into the DB and is re-hydrated on the next run.
    #[tokio::test]
    async fn state_survives_an_ephemeral_work_dir() {
        use frontkeep_storage::Db;

        let dbpath =
            std::env::temp_dir().join(format!("frontkeep-tf-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", dbpath.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let work_root =
            std::env::temp_dir().join(format!("frontkeep-tfwork-{}", frontkeep_storage::new_uid()));
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

    /// The startup reclaim drops orphaned per-resource scratch dirs (the disk a
    /// self-deadlocking pileup would pin) but keeps the shared `.plugin-cache`.
    /// Gated on a durable store: without one the work dir *is* the state.
    #[tokio::test]
    async fn prune_work_root_clears_orphans_but_keeps_cache() {
        use frontkeep_storage::Db;

        let work_root = std::env::temp_dir().join(format!(
            "frontkeep-tfprune-{}",
            frontkeep_storage::new_uid()
        ));
        let orphan = work_root.join("proj-2026-0003").join("ecr-credential");
        let cache = work_root.join(".plugin-cache");
        std::fs::create_dir_all(orphan.join("ci-push-42")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("provider.bin"), b"x").unwrap();

        // No store → no-op: the local work dirs are the only copy of state.
        let no_store =
            TerraformConnector::new("terraform", PathBuf::from("/modules"), work_root.clone());
        no_store.prune_work_root();
        assert!(orphan.exists(), "without a store nothing is reclaimed");

        let dbpath = std::env::temp_dir().join(format!(
            "frontkeep-tfprune-{}.db",
            frontkeep_storage::new_uid()
        ));
        let db = Db::connect(&format!("sqlite://{}", dbpath.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let conn = TerraformConnector::new("terraform", PathBuf::from("/modules"), work_root)
            .with_state(Arc::new(TfStateStore::new(db, [0x33; 32])));
        conn.prune_work_root();
        assert!(!orphan.exists(), "orphaned scratch dir reclaimed");
        assert!(
            cache.join("provider.bin").exists(),
            "the shared plugin cache is preserved"
        );
    }

    /// `gc_work_dir` removes a finished resource's scratch dir when state is
    /// durable, and leaves it alone otherwise (tests keep state in the dir).
    #[tokio::test]
    async fn gc_work_dir_removes_scratch_only_with_durable_state() {
        use frontkeep_storage::Db;

        let work_root =
            std::env::temp_dir().join(format!("frontkeep-tfgc-{}", frontkeep_storage::new_uid()));
        let r = req();

        let no_store =
            TerraformConnector::new("terraform", PathBuf::from("/modules"), work_root.clone());
        let wd = no_store.work_dir(&r);
        std::fs::create_dir_all(&wd).unwrap();
        no_store.gc_work_dir(&wd);
        assert!(wd.exists(), "without a store the work dir is left in place");

        let dbpath = std::env::temp_dir().join(format!(
            "frontkeep-tfgc-{}.db",
            frontkeep_storage::new_uid()
        ));
        let db = Db::connect(&format!("sqlite://{}", dbpath.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let conn = TerraformConnector::new("terraform", PathBuf::from("/modules"), work_root)
            .with_state(Arc::new(TfStateStore::new(db, [0x33; 32])));
        let wd = conn.work_dir(&r);
        std::fs::create_dir_all(&wd).unwrap();
        conn.gc_work_dir(&wd);
        assert!(!wd.exists(), "durable state → scratch dir reclaimed");
        // Idempotent: a missing dir is not an error.
        conn.gc_work_dir(&wd);
    }

    /// When another replica holds a resource's lease, this connector refuses to
    /// apply rather than racing its state — no terraform needed, the lock is
    /// checked before the working dir is touched.
    #[tokio::test]
    async fn cross_instance_lease_blocks_concurrent_apply() {
        use frontkeep_storage::leases::Leases;
        use frontkeep_storage::Db;

        let dbpath = std::env::temp_dir().join(format!(
            "frontkeep-tflease-{}.db",
            frontkeep_storage::new_uid()
        ));
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

    #[tokio::test]
    async fn read_capped_bounds_a_flood_and_marks_truncation() {
        let data = vec![b'x'; 100];
        let mut src: &[u8] = &data;
        let out = read_capped(&mut src, 10).await.unwrap();
        assert!(out.starts_with(&[b'x'; 10]), "keeps the head");
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("output truncated at 10 bytes"), "marks the cut");
        // Bounded: the 100-byte flood never lands in the buffer in full.
        assert!(out.len() < 50);
    }

    #[tokio::test]
    async fn read_capped_keeps_short_output_verbatim() {
        let mut src: &[u8] = b"hello";
        let out = read_capped(&mut src, 1024).await.unwrap();
        assert_eq!(out, b"hello");
    }
}
