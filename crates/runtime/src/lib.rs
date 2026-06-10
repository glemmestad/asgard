//! Ephemeral, isolated agent execution with per-invocation caps enforced at the
//! runtime, not in user code (RFC-0002 Part B). Wall-time is hard-enforced by the
//! supervisor (kill on deadline); step/budget caps are surfaced to the workload
//! and enforced jointly with the gateway. gVisor is the documented default Linux
//! backend; local-process and container backends are the fallbacks.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("failed to spawn: {0}")]
    Spawn(String),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("circuit breaker open")]
    CircuitOpen,
    #[error("io error: {0}")]
    Io(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Caps {
    pub wall_time: Duration,
    pub max_steps: u32,
    pub budget_usd: f64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            wall_time: Duration::from_secs(60),
            max_steps: 25,
            budget_usd: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InvocationSpec {
    pub agent: String,
    pub image: String,
    pub command: Vec<String>,
    pub caps: Caps,
    pub env: BTreeMap<String, String>,
    pub trace_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapBreach {
    WallTime,
    Steps,
    Budget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invocation {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub wall_time_ms: u128,
    /// Set if the supervisor terminated the workload for breaching a cap.
    pub terminated: Option<CapBreach>,
}

impl Invocation {
    pub fn succeeded(&self) -> bool {
        self.terminated.is_none() && self.exit_code == Some(0)
    }
}

#[async_trait]
pub trait Runtime: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, spec: InvocationSpec) -> Result<Invocation, RuntimeError>;
}

fn caps_env(spec: &InvocationSpec) -> Vec<(String, String)> {
    vec![
        (
            "FRONTKEEP_MAX_STEPS".into(),
            spec.caps.max_steps.to_string(),
        ),
        (
            "FRONTKEEP_BUDGET_USD".into(),
            format!("{}", spec.caps.budget_usd),
        ),
        ("FRONTKEEP_TRACE_ID".into(), spec.trace_id.clone()),
    ]
}

/// Runs the command as a child process with a hard wall-time deadline. The
/// zero-dependency fallback (and the only backend exercisable on macOS).
pub struct LocalProcessRuntime;

#[async_trait]
impl Runtime for LocalProcessRuntime {
    fn name(&self) -> &str {
        "local-process"
    }

    async fn run(&self, spec: InvocationSpec) -> Result<Invocation, RuntimeError> {
        if spec.command.is_empty() {
            return Err(RuntimeError::Spawn("empty command".into()));
        }
        let mut cmd = tokio::process::Command::new(&spec.command[0]);
        cmd.args(&spec.command[1..]);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        for (k, v) in caps_env(&spec) {
            cmd.env(k, v);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // kill_on_drop: when the wall-time future is dropped on timeout, the
        // child is killed rather than orphaned.
        cmd.kill_on_drop(true);

        let started = Instant::now();
        let child = cmd
            .spawn()
            .map_err(|e| RuntimeError::Spawn(e.to_string()))?;
        match tokio::time::timeout(spec.caps.wall_time, child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(Invocation {
                exit_code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                wall_time_ms: started.elapsed().as_millis(),
                terminated: None,
            }),
            Ok(Err(e)) => Err(RuntimeError::Io(e.to_string())),
            Err(_) => Ok(Invocation {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                wall_time_ms: started.elapsed().as_millis(),
                terminated: Some(CapBreach::WallTime),
            }),
        }
    }
}

/// Container backend (Docker/OCI). With `gvisor`, selects the `runsc` runtime —
/// the documented production default. gVisor requires Linux; on other hosts the
/// gVisor path returns `Unavailable` (see BUILD_LOG D-002).
pub struct ContainerRuntime {
    pub gvisor: bool,
    pub memory: String,
    pub cpus: String,
}

impl Default for ContainerRuntime {
    fn default() -> Self {
        ContainerRuntime {
            gvisor: false,
            memory: "512m".into(),
            cpus: "1".into(),
        }
    }
}

impl ContainerRuntime {
    pub fn gvisor() -> Self {
        ContainerRuntime {
            gvisor: true,
            ..Default::default()
        }
    }

    /// Build the `docker run` argument vector (testable without Docker present).
    pub fn docker_args(&self, spec: &InvocationSpec) -> Vec<String> {
        let mut args = vec!["run".into(), "--rm".into()];
        if self.gvisor {
            args.push("--runtime=runsc".into());
        }
        args.push(format!("--memory={}", self.memory));
        args.push(format!("--cpus={}", self.cpus));
        args.push("--network=none".into());
        for (k, v) in spec.env.iter() {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        for (k, v) in caps_env(spec) {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push(spec.image.clone());
        args.extend(spec.command.iter().cloned());
        args
    }
}

#[async_trait]
impl Runtime for ContainerRuntime {
    fn name(&self) -> &str {
        if self.gvisor {
            "gvisor"
        } else {
            "container"
        }
    }

    async fn run(&self, spec: InvocationSpec) -> Result<Invocation, RuntimeError> {
        if self.gvisor && !cfg!(target_os = "linux") {
            return Err(RuntimeError::Unavailable(
                "gVisor (runsc) requires Linux".into(),
            ));
        }
        // Delegate to the local supervisor running `docker run ...`, preserving
        // the wall-time deadline.
        let mut command = vec!["docker".to_string()];
        command.extend(self.docker_args(&spec));
        let delegated = InvocationSpec {
            command,
            env: BTreeMap::new(), // env is passed via docker -e flags above
            ..spec
        };
        LocalProcessRuntime.run(delegated).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Gvisor,
    Container,
    LocalProcess,
}

/// Construct a runtime for a backend. gVisor is the documented default in
/// production Linux deployments; callers fall back to container/local elsewhere.
pub fn build(backend: Backend) -> Box<dyn Runtime> {
    match backend {
        Backend::Gvisor => Box::new(ContainerRuntime::gvisor()),
        Backend::Container => Box::new(ContainerRuntime::default()),
        Backend::LocalProcess => Box::new(LocalProcessRuntime),
    }
}

/// Trips after `threshold` consecutive failures and refuses further runs until reset.
pub struct CircuitBreaker {
    threshold: u32,
    consecutive_failures: AtomicU32,
    open: AtomicBool,
}

impl CircuitBreaker {
    pub fn new(threshold: u32) -> Self {
        CircuitBreaker {
            threshold,
            consecutive_failures: AtomicU32::new(0),
            open: AtomicBool::new(false),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Records a failure; returns true if the breaker is now open.
    pub fn record_failure(&self) -> bool {
        let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= self.threshold {
            self.open.store(true, Ordering::Relaxed);
        }
        self.is_open()
    }

    pub fn reset(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.open.store(false, Ordering::Relaxed);
    }
}

/// A runtime wrapped with a circuit breaker: runs are refused once the breaker
/// is open; non-zero/terminated invocations count as failures.
pub struct Sandbox {
    runtime: Box<dyn Runtime>,
    breaker: CircuitBreaker,
}

impl Sandbox {
    pub fn new(runtime: Box<dyn Runtime>, failure_threshold: u32) -> Self {
        Sandbox {
            runtime,
            breaker: CircuitBreaker::new(failure_threshold),
        }
    }

    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    pub async fn run(&self, spec: InvocationSpec) -> Result<Invocation, RuntimeError> {
        if self.breaker.is_open() {
            return Err(RuntimeError::CircuitOpen);
        }
        match self.runtime.run(spec).await {
            Ok(inv) => {
                if inv.succeeded() {
                    self.breaker.record_success();
                } else {
                    self.breaker.record_failure();
                }
                Ok(inv)
            }
            Err(e) => {
                self.breaker.record_failure();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: Vec<&str>, wall: Duration) -> InvocationSpec {
        InvocationSpec {
            agent: "agent:default/test".into(),
            image: "busybox".into(),
            command: command.into_iter().map(String::from).collect(),
            caps: Caps {
                wall_time: wall,
                ..Default::default()
            },
            env: BTreeMap::new(),
            trace_id: "tr_test".into(),
        }
    }

    #[tokio::test]
    async fn local_process_runs_and_captures_output() {
        let inv = LocalProcessRuntime
            .run(spec(vec!["echo", "hello-sandbox"], Duration::from_secs(5)))
            .await
            .unwrap();
        assert!(inv.succeeded());
        assert!(inv.stdout.contains("hello-sandbox"));
        assert!(inv.terminated.is_none());
    }

    #[tokio::test]
    async fn wall_time_cap_kills_runaway() {
        let inv = LocalProcessRuntime
            .run(spec(vec!["sleep", "10"], Duration::from_millis(200)))
            .await
            .unwrap();
        assert_eq!(inv.terminated, Some(CapBreach::WallTime));
        assert!(!inv.succeeded());
    }

    #[tokio::test]
    async fn gvisor_unavailable_off_linux() {
        let rt = ContainerRuntime::gvisor();
        let res = rt
            .run(spec(vec!["echo", "hi"], Duration::from_secs(5)))
            .await;
        if cfg!(target_os = "linux") {
            // On Linux it would attempt docker; we don't assert success here.
        } else {
            assert!(matches!(res, Err(RuntimeError::Unavailable(_))));
        }
    }

    #[test]
    fn docker_args_include_isolation_flags() {
        let rt = ContainerRuntime::gvisor();
        let args = rt.docker_args(&spec(vec!["python", "agent.py"], Duration::from_secs(5)));
        assert!(args.contains(&"--runtime=runsc".to_string()));
        assert!(args.contains(&"--network=none".to_string()));
        assert!(args.iter().any(|a| a.starts_with("--memory=")));
        assert!(args.contains(&"agent.py".to_string()));
    }

    #[tokio::test]
    async fn circuit_breaker_trips_and_blocks() {
        let sandbox = Sandbox::new(Box::new(LocalProcessRuntime), 2);
        // Two failing runs (nonexistent command -> spawn error counts as failure).
        let _ = sandbox
            .run(spec(
                vec!["definitely-not-a-real-binary-xyz"],
                Duration::from_secs(1),
            ))
            .await;
        let _ = sandbox
            .run(spec(
                vec!["definitely-not-a-real-binary-xyz"],
                Duration::from_secs(1),
            ))
            .await;
        assert!(sandbox.breaker().is_open());
        let blocked = sandbox
            .run(spec(vec!["echo", "hi"], Duration::from_secs(1)))
            .await;
        assert!(matches!(blocked, Err(RuntimeError::CircuitOpen)));
    }
}
