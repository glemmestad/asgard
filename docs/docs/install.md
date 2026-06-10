---
sidebar_position: 2.1
title: Install the CLI
---

# Install Frontkeep

Frontkeep is **one statically-linked binary** that is both the server (`asgard
serve`, `frontkeep mcp`) and the command-line client (`frontkeep project`, `asgard
cost`, `frontkeep chat`, …). Installing it gives you both.

## macOS / Linux (one-liner)

```bash
curl -fsSL https://raw.githubusercontent.com/glemmestad/asgard/main/scripts/install.sh | sh
```

This downloads the right tarball for your OS/architecture from the latest
release, verifies its checksum, and installs to `~/.local/bin` (override with
`FRONTKEEP_BIN_DIR`). The Linux builds are static (musl) — no system libraries
required, works on Alpine too.

Then point it at a deployment and go:

```bash
frontkeep login                 # stores a server URL + PAT in a profile
frontkeep tools                 # list everything the server exposes
```

See [Use the CLI](./cli.md) for the full command surface.

## Manual download

Grab a tarball from the [latest release](https://github.com/glemmestad/asgard/releases/latest):

| Platform | Asset |
| --- | --- |
| Linux x86-64 | `asgard-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64 | `asgard-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple Silicon | `asgard-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `asgard-x86_64-apple-darwin.tar.gz` |

```bash
tar -xzf asgard-*.tar.gz && sudo install -m755 frontkeep /usr/local/bin/asgard
frontkeep --version
```

Each tarball ships with a matching `.sha256`.

## From source

```bash
cargo install --git https://github.com/glemmestad/frontkeep asgard
```

## Docker

For running the **server**, the container image bundles Terraform and the
provisioning modules — see [Deploy](./deploy.md):

```bash
docker run -p 8080:8080 ghcr.io/glemmestad/asgard:latest
```

## Notes

- **Armed provisioning** (`frontkeep serve` against real AWS/Auth0) needs
  `terraform` on your `PATH`; the native binary ships only itself, while the
  Docker image bundles Terraform. The control plane and the CLI work with no
  extra dependencies.
- The in-app `/docs` route is empty in the native binary (the docs site is built
  separately and lives at [asgard.build](https://asgard.build)); the Docker image
  embeds it.
- **macOS Gatekeeper:** binaries are not yet notarized. `brew`/`curl` downloads
  generally run without a prompt; if macOS quarantines it, clear the flag with
  `xattr -d com.apple.quarantine "$(command -v asgard)"`.
