---
sidebar_position: 15
title: Upgrade — Asgard → Frontkeep
---

# Upgrading from Asgard to Frontkeep

The project, binary, crates, and on-the-wire identifiers were renamed from
**Asgard** to **Frontkeep**. The rename is built to be **non-breaking**: legacy
names keep working through a back-compat layer, so most deployments upgrade in
place with no changes. This guide lists what changed and the few one-time actions
that apply to specific deployment shapes.

## TL;DR

For a typical deployment — Postgres (or any explicitly-set `DATABASE_URL`), tokens
issued to clients, an explicit resource name in Terraform — an in-place upgrade
needs **no config changes**:

- Legacy `ASGARD_*` environment variables are still read (promoted to `FRONTKEEP_*`).
- Existing `asg_…` / `asg_pat_…` tokens still authenticate.
- An existing `asgard.yaml` is still auto-loaded.

Pull the new binary/image and restart. The items below are only relevant if you
relied on a **default** (bare binary name, default DB path, default Terraform
resource name) rather than setting things explicitly.

## What changed, and what to do

### Binary name
`asgard` → `frontkeep`. The release artifact and `install.sh` now install a
`frontkeep` binary. Update any wrapper scripts, cron entries, or `PATH` shims that
invoke the binary by name.

### Environment variables
`ASGARD_*` → `FRONTKEEP_*`. **Both work.** On startup any unset `FRONTKEEP_*` var
is promoted from its `ASGARD_*` sibling; if both are set, the new name wins.
Rename at your convenience — nothing breaks if you don't.

### API tokens & project keys
New project keys mint as `fk_…` and personal access tokens as `fk_pat_…`. **Existing
`asg_…` / `asg_pat_…` tokens remain valid** — they are accepted on the validation
path indefinitely. No token reissue is required.

### Config file
The default is now `frontkeep.yaml`. If you don't pass `--config` and there is no
`frontkeep.yaml`, an existing `asgard.yaml` in the working directory is still
auto-loaded. `frontkeep init` writes `frontkeep.yaml`.

### SQLite default DB path
The bare default changed `asgard.db` → `frontkeep.db`. **Action only if you relied
on the bare default** (no `FRONTKEEP_DATABASE_URL` / `--database-url` set):

- point at the old file: `--database-url sqlite://asgard.db` (or
  `FRONTKEEP_DATABASE_URL=sqlite://asgard.db`), **or**
- rename it once: `mv asgard.db frontkeep.db`.

Deployments that set the DB URL explicitly (all Postgres; docker/systemd/Helm with
env) need nothing.

### systemd
The unit is renamed `asgard.service` → `frontkeep.service`; `ExecStart` is now
`/usr/local/bin/frontkeep` and the state dir is `/var/lib/frontkeep`. For an
existing install that used the default state dir:

```bash
# install the new binary as `frontkeep`, then:
sudo systemctl stop asgard
sudo mv /var/lib/asgard /var/lib/frontkeep        # carry the SQLite DB over
sudo cp packaging/systemd/frontkeep.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl disable asgard && sudo systemctl enable --now frontkeep
```

If your DB is external (Postgres via `FRONTKEEP_DATABASE_URL`), skip the `mv` step.

### Docker / container image
The image is now `ghcr.io/glemmestad/frontkeep`. Pull the new tag; pass
`FRONTKEEP_*` env (or keep `ASGARD_*`, which is promoted).

### Helm chart
The chart is renamed `charts/asgard` → `charts/frontkeep`, with env vars and labels
updated to `frontkeep`. **Kubernetes `spec.selector` labels are immutable**, so a
`helm upgrade` of an existing release onto the renamed chart will be rejected.
Migrate by reinstalling against the **same** database so no data is lost:

```bash
# Point both at the same external/managed Postgres (or a retained PVC):
helm uninstall asgard
helm install frontkeep ./charts/frontkeep --set databaseUrl="postgres://…"
```

Use external Postgres or a retained `PersistentVolume` (the default chart uses an
ephemeral `emptyDir`, so set up persistence before relying on it).

### Terraform — the self-host module (`terraform/frontkeep`)
Resource **addresses** were renamed `asgard` → `frontkeep`. The module ships
`moved {}` blocks, so `terraform plan` reports in-place **moves**, not
destroy/recreate. **Action**: if you deployed with the *default* `name = "asgard"`
(rather than passing `-var name=…`), pass `-var name=asgard` on the next apply so
the container and its data volume are not renamed. Deployments that set `name` (or
`database_url`) explicitly are unaffected.

### Terraform — resources Frontkeep provisions for you
**Nothing to do.** Provisioned resource names derive from the project tag Frontkeep
injects at apply time, which is unchanged. The only branded value was a fallback
default that is never used when a project tag is present (it always is). Existing
live resources keep their current names and are untouched.

### LiteLLM connector aliases
Newly provisioned LiteLLM model aliases are prefixed `frontkeep-…` instead of
`asgard-…`. Existing `asgard-…` aliases are **not** destroyed; they simply become
unmanaged. If you use this module, delete the old aliases manually after
re-provisioning, or leave them (harmless).

### Trace header
`x-asgard-trace-id` → `x-frontkeep-trace-id`. If a client sets this header
explicitly for trace propagation, update it. It is optional.

## What is deliberately *not* renamed

These keep legacy deployments working and are permanent back-compat, not leftovers:

- The `asg_` / `asg_pat_` token-validation path.
- `ASGARD_*` environment-variable promotion.
- Legacy `asgard.yaml` auto-load.

You can remove your own use of the legacy names at any time; the product will keep
accepting them.
