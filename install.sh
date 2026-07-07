#!/usr/bin/env bash
set -euo pipefail

# v2 installer
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/vihaanshahh/v2/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/vihaanshahh/v2/main/install.sh | V2_VERSION=v0.1.0 bash
#
# Install to a custom prefix:
#   curl -fsSL ... | PREFIX="$HOME/bin" bash
#
# Override GitHub repo (owner/name):
#   V2_REPO=you/v2 curl -fsSL ... | bash

REPO="${V2_REPO:-vihaanshahh/v2}"
VERSION="${V2_VERSION:-latest}"

err() {
  echo "v2 install error: $*" >&2
  exit 1
}

info() {
  echo "v2: $*"
}

case "$(uname -s 2>/dev/null || echo unknown)" in
  Darwin) PLATFORM="darwin" ;;
  Linux) PLATFORM="linux" ;;
  MINGW*|MSYS*|CYGWIN*)
    err "Windows detected. Download v2-windows-x64.zip from https://github.com/${REPO}/releases/latest"
    ;;
  *)
    err "unsupported OS: $(uname -s)"
    ;;
esac

case "$(uname -m 2>/dev/null || echo unknown)" in
  x86_64|amd64) ARCH="x64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *)
    err "unsupported architecture: $(uname -m)"
    ;;
esac

ARTIFACT="v2-${PLATFORM}-${ARCH}.tar.gz"

if [ "${VERSION}" = "latest" ] || [ -z "${VERSION}" ]; then
  DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${ARTIFACT}"
  CHECKSUMS_URL="https://github.com/${REPO}/releases/latest/download/v2-checksums.txt"
else
  case "${VERSION}" in
    v*) ;;
    *) VERSION="v${VERSION}" ;;
  esac
  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARTIFACT}"
  CHECKSUMS_URL="https://github.com/${REPO}/releases/download/${VERSION}/v2-checksums.txt"
fi

if [ "$(id -u 2>/dev/null || echo 1)" -eq 0 ]; then
  PREFIX="${PREFIX:-/usr/local}"
else
  PREFIX="${PREFIX:-${HOME}/.local}"
fi
INSTALL_DIR="${PREFIX}/bin"

info "installing ${ARTIFACT} to ${INSTALL_DIR}"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf -- "$TMP_DIR"' EXIT
TMP_TARBALL="${TMP_DIR}/${ARTIFACT}"

if command -v curl >/dev/null 2>&1; then
  curl -fsSL "${DOWNLOAD_URL}" -o "${TMP_TARBALL}"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "${TMP_TARBALL}" "${DOWNLOAD_URL}"
else
  err "need curl or wget to download the release"
fi

if ! tar -tzf "${TMP_TARBALL}" >/dev/null 2>&1; then
  err "downloaded file is not a valid tarball (check V2_VERSION and V2_REPO)"
fi

TMP_CHECKSUMS="${TMP_DIR}/v2-checksums.txt"
CHECKSUMS_AVAILABLE=false
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "${CHECKSUMS_URL}" -o "${TMP_CHECKSUMS}" 2>/dev/null && CHECKSUMS_AVAILABLE=true
elif command -v wget >/dev/null 2>&1; then
  wget -qO "${TMP_CHECKSUMS}" "${CHECKSUMS_URL}" 2>/dev/null && CHECKSUMS_AVAILABLE=true
fi

if [ "${CHECKSUMS_AVAILABLE}" = true ]; then
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "${TMP_DIR}" && sha256sum -c --ignore-missing v2-checksums.txt >/dev/null 2>&1) \
      || err "checksum validation failed"
    info "checksum validated"
  elif command -v shasum >/dev/null 2>&1; then
    (cd "${TMP_DIR}" && shasum -a 256 -c --ignore-missing v2-checksums.txt >/dev/null 2>&1) \
      || err "checksum validation failed"
    info "checksum validated"
  else
    info "warning: no sha256sum/shasum found; skipping checksum validation"
  fi
fi

mkdir -p "${INSTALL_DIR}"
tar -xzf "${TMP_TARBALL}" -C "${INSTALL_DIR}"
chmod +x "${INSTALL_DIR}/v2"

info "installed to ${INSTALL_DIR}/v2"

if command -v v2 >/dev/null 2>&1; then
  info "run 'v2' to detect hardware and see which LLMs fit"
else
  PATH_LINE="export PATH=\"${INSTALL_DIR}:\$PATH\""
  info "${INSTALL_DIR} is not on PATH"
  info "add this to your shell profile: ${PATH_LINE}"
  info "then run: v2"
fi
