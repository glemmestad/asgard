---
sidebar_position: 2.3
title: Use the CLI
---

# The Asgard CLI

The `asgard` binary is both the server and a full command-line client. The CLI is
a **PAT-authenticated MCP client**: it talks to a deployment's `/mcp` endpoint
with the same `asg_pat_…` token an agent uses, so it has **parity with the agent
surface by construction** — every MCP tool is reachable — plus things an agent
surface can't do (run inference, write seed files to disk, shell completions).

Why both a CLI and an MCP? They're two surfaces over one core. The MCP is for
agents and stateful, audited, multi-tenant flows; the CLI is for humans, scripts,
and CI — composable, low-token, and instantly familiar.

## Authenticate

Every control-plane command needs a **user PAT** (`asg_pat_…`). Mint one in the
dashboard under **Get Started**, then either pass it per-command or save a
profile:

```bash
# Save a reusable profile (prompts for the PAT if --pat is omitted):
asgard --url https://asgard.example login

# …or pass it ad hoc / via the environment:
asgard --url https://asgard.example --pat asg_pat_… project ls
export ASGARD_URL=https://asgard.example ASGARD_PAT=asg_pat_…
```

Profiles live in `~/.config/asgard/config.toml`. Settings resolve in order:
**flag → environment → selected profile → built-in default**. Pick a profile with
`--profile <name>` (or `ASGARD_PROFILE`); `asgard login` writes the one named by
`--profile` (default `default`).

A user PAT acts across **every project you own or manage** — register new ones,
read cost, mint keys — with no per-project credential.

## Discover everything

```bash
asgard tools                      # every tool the server exposes
asgard call <tool> --json '{…}'   # call any tool directly (raw arguments)
asgard call cost_by --arg by=group
```

`call` is the engine; every typed subcommand below is sugar over it, so a new
server tool is reachable via `asgard call` the day it ships. `--arg key=value`
coerces values as JSON when they parse (`budget_usd=100` → number,
`spec={"size":1}` → object) and as strings otherwise; `--json -` reads stdin.

## Command groups

| Group | What it covers |
| --- | --- |
| `project` | `ls`, `register`, `update`, `get`, `state`, `credential`, `promotion`, `promote`, `escalate` — the registration gate + lifecycle |
| `catalog` | `search`, `get`, `services`, `service`, `groups` — entities + the service catalog |
| `cost` | `report`, `project`, `series`, `by`, `forecast`, `anomalies`, `tree`, `movers` |
| `resource` | `request`, `grant`, `ls`, `get`, `runs`, `retry`, `deprovision` — infrastructure |
| `secret` | `get`, `rotate`, `ls` |
| `standards` / `guidance` / `recipe` | the knowledge base (`ls`/`get`, plus `put` for guidance & recipes) |
| `mcp-catalog` | `ls`, `get`, `publish`, `set-state` |
| `skills` | `ls`, `get`, `publish`, `set-state`, `export`, `install` — the published skills catalog |
| `seed` | `ls`, `plan`, `get`, `apply` — agent-seed modules |
| `governance` | org-wide portfolio metrics |

Examples:

```bash
asgard project register --name "Billing API" --manager lead@corp.example --group platform
asgard project ls
asgard cost tree
asgard resource request --project proj-2026-0001 --resource-type s3-bucket --name assets
asgard secret get --project proj-2026-0001 --name db-password
```

## Output formats

`-o table` (default, human), `-o json`, or `-o yaml` (also `ASGARD_OUTPUT`). Use
`-o json` for scripting:

```bash
asgard -o json project ls | jq -r '.[].project_id'
```

## Beyond the agent surface

- **Run inference.** The control plane mints credentials but never calls models;
  the CLI closes the loop — it mints (and caches) a project gateway key for you
  and calls the gateway:

  ```bash
  asgard chat --project proj-2026-0001 --model model:default/mock --message "hello"
  ```

- **Seed a repo to disk.** The `bootstrap` tool returns file bodies; the CLI
  writes them:

  ```bash
  asgard seed apply --languages rust --task "build a service" --write
  # dry-run by default; --write creates AGENTS.md + .agent/** ; --force overwrites
  ```

- **Publish & install skills from disk.** The `skills_catalog_install`/`export`
  tools return a file tree for the agent to write; the CLI writes it for you, and
  `publish --dir` bundles a local skill folder (each file base64-encoded):

  ```bash
  asgard skills publish --dir ./my-skill            # or --bundle <json|->
  asgard skills install <id> --dest claude-code     # writes into ~/.claude/skills/<name>
  asgard skills export <id> --runtime codex --out ./out
  # install writes by default; --dry-run previews, --force overwrites
  ```

- **Shell completions.**

  ```bash
  asgard completions zsh > ~/.zfunc/_asgard
  ```

- **Offline manifest validation** (no server): `asgard validate service.yaml`.

## Exit codes

`0` success · `2` the tool ran but returned an error · `3` authentication failed
(bad/missing PAT — the message tells you to mint one) · `1` transport/other. Logs
go to stderr; results go to stdout, so piping (`| jq`, `| head`) is clean.
