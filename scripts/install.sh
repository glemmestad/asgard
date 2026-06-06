#!/bin/sh
# Install the `asgard` binary (server + CLI) on macOS or Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/glemmestad/asgard/main/scripts/install.sh | sh
#
# Downloads the matching tarball from the latest GitHub release, verifies its
# checksum, and installs to ~/.local/bin (override with ASGARD_BIN_DIR). The
# Linux builds are static (musl), so no system libraries are required.
set -eu

REPO="glemmestad/asgard"
BIN_DIR="${ASGARD_BIN_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
      aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
      *) echo "asgard: unsupported Linux architecture: $arch" >&2; exit 1 ;;
    esac ;;
  Darwin)
    case "$arch" in
      x86_64) target="x86_64-apple-darwin" ;;
      arm64) target="aarch64-apple-darwin" ;;
      *) echo "asgard: unsupported macOS architecture: $arch" >&2; exit 1 ;;
    esac ;;
  *) echo "asgard: unsupported OS: $os (use the Docker image or build from source)" >&2; exit 1 ;;
esac

file="asgard-${target}.tar.gz"
# The checksum asset is named after the archive base, without `.tar.gz`.
sum="asgard-${target}.sha256"
base="https://github.com/${REPO}/releases/latest/download"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "asgard: downloading ${file}…"
curl -fsSL "${base}/${file}" -o "${tmp}/${file}"

# Verify the checksum when available (and a sha tool exists). HTTPS already
# protects the transfer; this is defense in depth.
if curl -fsSL "${base}/${sum}" -o "${tmp}/${sum}" 2>/dev/null; then
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && sha256sum -c "${sum}" >/dev/null) || { echo "asgard: checksum mismatch" >&2; exit 1; }
  elif command -v shasum >/dev/null 2>&1; then
    (cd "$tmp" && shasum -a 256 -c "${sum}" >/dev/null) || { echo "asgard: checksum mismatch" >&2; exit 1; }
  fi
fi

tar -xzf "${tmp}/${file}" -C "$tmp"
mkdir -p "$BIN_DIR"
install -m 0755 "${tmp}/asgard" "${BIN_DIR}/asgard" 2>/dev/null || {
  mv "${tmp}/asgard" "${BIN_DIR}/asgard" && chmod 0755 "${BIN_DIR}/asgard"
}

echo "asgard: installed to ${BIN_DIR}/asgard"
case ":${PATH}:" in
  *":${BIN_DIR}:"*) ;;
  *) echo "asgard: add ${BIN_DIR} to your PATH to run \`asgard\`." ;;
esac
echo "asgard: next → asgard login   (then: asgard tools)"
