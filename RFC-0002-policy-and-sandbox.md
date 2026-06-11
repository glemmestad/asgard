# RFC-0002 — Policy Engine & Runtime Sandbox

- Status: **Accepted** (drives the `policy` and `runtime` crates)
- Scope: the authorization layer (default Cedar, OPA pivot seam) and the agent execution sandbox (default gVisor, `Runtime` trait for container/Firecracker).

---

## Part A — Policy engine

### A.1 The one-engine principle

A single authorization engine answers questions from the gateway, catalog, workflow, and runtime. The canonical question:

> *Can this **principal** perform this **action** on this **resource** in this **context**, and if it is allowed only with conditions, what are the **obligations** (e.g. "requires approval from group X")?*

This is exposed as one trait so the rest of the system never imports a policy vendor:

```rust
pub struct Request<'a> {
    pub principal: EntityRef,      // user:default/alice, agent:default/code-reviewer
    pub action: &'a str,           // "deploy" | "invoke" | "read" | "decommission" | "approve"
    pub resource: EntityRef,       // agent:..., model:..., project:..., dataset:...
    pub context: serde_json::Value // data_class, model, project budget state, time, ...
}

pub enum Effect { Allow, Deny }

pub struct Decision {
    pub effect: Effect,
    pub reasons: Vec<String>,             // policy ids / human reasons (for audit)
    pub obligations: Vec<Obligation>,     // e.g. RequiresApproval { approver: EntityRef }
}

#[async_trait]
pub trait PolicyEngine: Send + Sync {
    async fn is_authorized(&self, req: &Request<'_>) -> Decision;
}
```

Every `Decision` is written to the `audit_log` with the originating `trace_id`, so a denial is always explainable after the fact.

### A.2 Why Cedar is the default

- **Rust-native and embeddable** (`cedar-policy` crate) — no sidecar, no extra process, no network hop. This is the deciding factor for the lightweight goal: a 100-person company's `docker run frontkeep` must not require standing up a policy server.
- **Verified, analyzable policy language** with a typed schema and validation, designed for authorization specifically (PARC: principal/action/resource/condition).
- **Fast** — in-process evaluation, microsecond-range, which matters because the gateway is on the hot path of every model call (target < 50 ms p95 total overhead).

### A.3 Cedar modeling

- **Entity types**: `User`, `Group`, `Project`, `Agent`, `Model`, `Dataset`, `DataClass`. Group membership and ownership come from the catalog/identity graph and are loaded as Cedar entities at evaluation time.
- **Actions**: `deploy`, `invoke`, `read`, `decommission`, `approve`.
- **Data-class × model allowlist** is expressed as Cedar policies, e.g. *"permit invoke of a Model only if the request's `data_class` is in that Model's `dataClassAllowlist`."* A wrong data-class+model pairing is a `Deny` enforced at the gateway.
- **Approval routing** is an *obligation*, not a separate system: a policy may `permit` an action while attaching `RequiresApproval { approver }`, which the workflow layer turns into a request. This keeps "does this need approval and from whom" in the same engine as "is this allowed."

Default policies ship in-tree (`policy/policies/*.cedar`) and operators can extend them. The Cedar **schema** is validated in tests so a malformed policy fails CI, not production.

### A.4 The OPA / Rego pivot seam

OPA is the obvious alternative when an org has *already* standardized on Rego and wants one policy plane across infra and app. The cost is an external dependency (an OPA sidecar/daemon), a network hop on the hot path, and a non-Rust toolchain.

The seam: `PolicyEngine` is the only contract. An `OpaEngine` implementation would translate `Request` → OPA `input` document, `POST` to the OPA data API, and map the response back to `Decision`. Nothing else in the codebase changes. It is **not built** in this run (it is a documented pivot, selectable via config once implemented). Trade-off summary:

| | Cedar (default) | OPA/Rego (pivot) |
|---|---|---|
| Deployment | in-process, zero services | sidecar/daemon required |
| Hot-path latency | µs, no network | network hop |
| Language | Cedar (authz-specific, typed) | Rego (general, larger ecosystem) |
| Best when | lightweight, self-contained installs | org already standardized on OPA |

---

## Part B — Runtime sandbox

### B.1 What the runtime guarantees

Agent invocations run in **ephemeral, isolated** execution with **per-invocation caps enforced at the runtime, not in user code**:

- **wall-time cap** (kill on deadline),
- **step cap** (max tool/model iterations),
- **budget cap** (max USD spend for the invocation, enforced jointly with the gateway),
- **circuit breaker** (repeated failures / runaway loops trip and halt).

```rust
pub struct InvocationSpec {
    pub agent: EntityRef,
    pub image: String,                 // OCI image for container/gVisor backends
    pub command: Vec<String>,
    pub caps: Caps,                    // wall_time, max_steps, budget_usd
    pub env: BTreeMap<String, String>, // includes the per-invocation gateway virtual key
    pub trace_id: TraceId,
}

#[async_trait]
pub trait Runtime: Send + Sync {
    async fn run(&self, spec: InvocationSpec) -> Result<Invocation, RuntimeError>;
}
```

Caps are enforced by the supervisor around the backend (deadline timer, step accounting, budget checks against the gateway's running cost for the trace) so that an agent cannot exceed them regardless of what its own code does.

### B.2 Why gVisor is the default

- **Container ergonomics** — agents ship as OCI images; operators already know the workflow.
- **Strong isolation** — gVisor's user-space kernel (`runsc`) intercepts syscalls, giving a much smaller host attack surface than a shared-kernel container, without the full weight/boot cost of a VM.
- Fits the "runs on a single box" goal better than a hypervisor-per-invocation default.

### B.3 The `Runtime` trait and the other backends

- **`LocalProcess`** — runs the command as a child process with cgroup/rlimit-style caps where available; the zero-dependency fallback (used in dev and in this build's host, macOS).
- **`Container`** — Docker/OCI without gVisor; the fallback when `runsc` isn't installed.
- **`Gvisor`** — `runsc` runtime; **the documented default** in production Linux deployments.
- **`Firecracker`** (future) — microVM-per-invocation for the largest/strictest deployments; slots in behind the same trait.

Selection is config/feature-driven; the supervisor and cap enforcement are backend-agnostic.

### B.4 Host constraint recorded for this build

gVisor (`runsc`) is **Linux-only**; the build host is macOS, so the gVisor backend is compiled and selectable but exercised for *selection + cap-enforcement logic* only here. `LocalProcess` and `Container` backends are exercised live. A Linux CI job validates the gVisor path where `runsc` is available. See BUILD_LOG D-002.
