#!/usr/bin/env bash
# air-drive — one-liner installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Toilal/air-drive/main/install.sh | bash
#
# Flags (after `bash -s --`):
#   --prefix PATH      install the binary to PATH/bin instead of ~/.local/bin
#   --systemd          also run `air-drive setup --install-service` post-install
#   --version vX.Y.Z   pin a specific release tag (default: latest)
#   --target TRIPLE    force a target triple (auto-detected from uname)
#   --musl / --gnu     force the libc flavour (default: musl, max compat)
#
# The script downloads the matching tarball from the GitHub Release, verifies
# its SHA-256 against the published sibling .sha256 file, extracts it, and
# moves the binary into place. The systemd unit + LICENSE + NOTICE + the
# bundled INSTALL.md land in $XDG_DATA_HOME/air-drive/<version>/ for the
# user to inspect.

set -euo pipefail

REPO="Toilal/air-drive"
PREFIX="${PREFIX:-$HOME/.local}"
INSTALL_SYSTEMD=0
PIN_VERSION=""
FORCE_TARGET=""
LIBC="musl"

err()  { printf '\033[31m[install]\033[0m %s\n' "$*" >&2; }
info() { printf '\033[32m[install]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[install]\033[0m %s\n' "$*" >&2; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)  PREFIX="$2"; shift 2 ;;
        --systemd) INSTALL_SYSTEMD=1; shift ;;
        --version) PIN_VERSION="$2"; shift 2 ;;
        --target)  FORCE_TARGET="$2"; shift 2 ;;
        --musl)    LIBC="musl"; shift ;;
        --gnu)     LIBC="gnu"; shift ;;
        -h|--help)
            sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) err "unknown flag: $1"; exit 2 ;;
    esac
done

# ---- prerequisites --------------------------------------------------------

need() {
    command -v "$1" >/dev/null 2>&1 || { err "missing dependency: $1"; exit 1; }
}
need curl
need tar
need sha256sum
need uname

# ---- target triple --------------------------------------------------------

detect_target() {
    local os arch
    os=$(uname -s)
    arch=$(uname -m)
    if [[ "$os" != "Linux" ]]; then
        err "only Linux is supported (got $os)"
        exit 1
    fi
    case "$arch" in
        x86_64|amd64)      echo "x86_64-unknown-linux-${LIBC}" ;;
        aarch64|arm64)     echo "aarch64-unknown-linux-${LIBC}" ;;
        *) err "unsupported architecture: $arch"; exit 1 ;;
    esac
}

TARGET="${FORCE_TARGET:-$(detect_target)}"
info "target: $TARGET"

# ---- resolve version ------------------------------------------------------

if [[ -n "$PIN_VERSION" ]]; then
    TAG="$PIN_VERSION"
else
    info "querying latest release..."
    TAG=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | head -1 \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
    if [[ -z "$TAG" ]]; then
        err "could not resolve the latest tag from the GitHub API"
        exit 1
    fi
fi
VERSION="${TAG#v}"
info "version: $VERSION"

PKG="air-drive-${VERSION}-${TARGET}"
TARBALL="${PKG}.tar.gz"
CHECKSUM="${TARBALL}.sha256"
BASE="https://github.com/${REPO}/releases/download/${TAG}"

# ---- download + verify ----------------------------------------------------

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

info "downloading ${TARBALL}..."
curl -fsSL --retry 3 -o "${TMP}/${TARBALL}" "${BASE}/${TARBALL}"
curl -fsSL --retry 3 -o "${TMP}/${CHECKSUM}" "${BASE}/${CHECKSUM}"

info "verifying checksum..."
( cd "$TMP" && sha256sum -c "${CHECKSUM}" )

# ---- extract + install ----------------------------------------------------

info "extracting..."
tar -C "$TMP" -xzf "${TMP}/${TARBALL}"

BIN_DST="${PREFIX}/bin/air-drive"
mkdir -p "${PREFIX}/bin"
install -m 0755 "${TMP}/${PKG}/air-drive" "$BIN_DST"
info "installed binary: ${BIN_DST}"

# Keep the assets (systemd unit, LICENSE, NOTICE, INSTALL.md) somewhere
# the user can inspect them later. XDG_DATA_HOME defaults to
# ~/.local/share when unset.
ASSETS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/air-drive/${VERSION}"
mkdir -p "${ASSETS_DIR}"
cp "${TMP}/${PKG}/LICENSE" "${TMP}/${PKG}/NOTICE" \
   "${TMP}/${PKG}/air-drive.service" "${TMP}/${PKG}/INSTALL.md" \
   "${ASSETS_DIR}/"
info "assets staged: ${ASSETS_DIR}"

# ---- PATH check -----------------------------------------------------------

if ! command -v air-drive >/dev/null 2>&1 && ! echo "$PATH" | tr : '\n' | grep -qx "${PREFIX}/bin"; then
    warn "${PREFIX}/bin is not on your \$PATH"
    warn "  add to your shell rc: export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

# ---- optional systemd -----------------------------------------------------

if (( INSTALL_SYSTEMD )); then
    if ! command -v systemctl >/dev/null 2>&1; then
        warn "--systemd was requested but systemctl is not installed; skipping"
    else
        info "installing systemd user unit..."
        "${BIN_DST}" setup --install-service
    fi
fi

# ---- done -----------------------------------------------------------------

info "done."
info "verify with: $BIN_DST --version"
info "next: air-drive link  &&  air-drive map <local> <remote-spec>"
