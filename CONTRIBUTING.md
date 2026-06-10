# Contributing to Frontkeep

Thanks for your interest. Frontkeep is Apache-2.0 and accepts contributions under
the [CLA](CLA.md) — opening a pull request constitutes acceptance.

## Ground rules

- **Conventional commits.** `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`, `ci:`. Keep subjects under ~72 chars.
- **Minimal, surgical changes.** Write the least code that solves the problem. No speculative abstractions or config nobody asked for. Match the surrounding style.
- **Comments explain *why*, never *what*.** No narrative comments.
- **No employer/vendor traces.** This is generic OSS. Do not add company names, internal hostnames, account IDs, or copied-not-reimplemented code. `scripts/cleanroom-check.sh` runs in CI and must return zero hits.
- **Tests are not optional.** Every behavioral change ships with a test. CI runs fmt, clippy (`-D warnings`), tests, the clean-room check, and a build.

## Dev setup

```sh
# Rust toolchain (stable) — https://rustup.rs
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings

# clean-room self-check (must be clean)
./scripts/cleanroom-check.sh

# web UI (embedded into the binary at build time)
cd web && npm ci && npm run build
```

## Architecture

Read [`RFC-0001`](RFC-0001-entity-model.md) (entity model) and
[`RFC-0002`](RFC-0002-policy-and-sandbox.md) (policy + sandbox) before changing
the catalog, policy, or runtime layers. New extension points are **Rust traits,
compiled in-tree** — there is no dynamic plugin runtime.

## Reporting bugs / requesting features

Open an issue. For security issues, see [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md)
for private contact rather than filing publicly.
