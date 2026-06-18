#!/usr/bin/env sh
# Quiver — one-command installer for Linux and macOS (ADR-0039).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/achref-soua/quiver/main/scripts/install.sh | sh
#
# Environment overrides:
#   QUIVER_VERSION        specific version to install (e.g. "0.17.0"); default: latest
#   QUIVER_INSTALL_DIR    directory to install the binary to; default: ~/.local/bin
#
# The script:
#   1. Detects OS + architecture.
#   2. Resolves the target version via the GitHub Releases API (or $QUIVER_VERSION).
#   3. Downloads the binary and its SHA-256 checksum.
#   4. Verifies the checksum before writing anything to disk.
#   5. Installs to $QUIVER_INSTALL_DIR and prints a PATH hint if needed.
#
# Requires: curl, sha256sum (Linux) or shasum (macOS), chmod, mv.
set -eu

REPO="achref-soua/quiver"
INSTALL_DIR="${QUIVER_INSTALL_DIR:-${HOME}/.local/bin}"

# ── helpers ──────────────────────────────────────────────────────────────────

info()  { printf '\033[1;34m[quiver]\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m[quiver]\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m[quiver]\033[0m %s\n' "$*" >&2; }
die()   { printf '\033[1;31m[quiver]\033[0m error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" > /dev/null 2>&1 || die "required tool not found: $1"
}

# ── platform detection ────────────────────────────────────────────────────────

detect_os() {
    case "$(uname -s)" in
        Linux*)  echo "linux";;
        Darwin*) echo "macos";;
        *)       die "unsupported OS: $(uname -s)";;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64) echo "x86_64";;
        aarch64|arm64) echo "aarch64";;
        *)            die "unsupported architecture: $(uname -m)";;
    esac
}

# ── checksum verification ─────────────────────────────────────────────────────

verify_sha256() {
    # $1 = file to check, $2 = checksum file (contains "hash  filename" or just "hash")
    if command -v sha256sum > /dev/null 2>&1; then
        # GNU coreutils — read the .sha256 file directly
        # The file may contain "hash  binary_name"; sha256sum --check needs the
        # filename in the checksum to match, so we normalise it.
        expected="$(awk '{print $1}' "$2")  $1"
        printf '%s' "$expected" | sha256sum --check --status - || \
            die "SHA-256 checksum mismatch — aborting install"
    elif command -v shasum > /dev/null 2>&1; then
        # macOS
        expected="$(awk '{print $1}' "$2")  $1"
        printf '%s' "$expected" | shasum -a 256 --check --status - || \
            die "SHA-256 checksum mismatch — aborting install"
    else
        warn "no sha256sum or shasum found — skipping checksum verification (not recommended)"
    fi
}

# ── main ──────────────────────────────────────────────────────────────────────

main() {
    need curl
    need chmod
    need mv

    OS="$(detect_os)"
    ARCH="$(detect_arch)"
    ASSET="quiver-${OS}-${ARCH}"

    # Resolve version
    if [ -n "${QUIVER_VERSION:-}" ]; then
        VERSION="${QUIVER_VERSION#v}"  # strip leading 'v' if present
        info "Installing quiver v${VERSION} (pinned)"
    else
        info "Resolving latest release..."
        API_RESP="$(curl -fsSL \
            -H "Accept: application/vnd.github+json" \
            "https://api.github.com/repos/${REPO}/releases/latest")"
        VERSION="$(printf '%s' "$API_RESP" | \
            grep '"tag_name"' | \
            head -1 | \
            sed 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/')"
        [ -n "$VERSION" ] || die "could not determine latest version from GitHub API"
        info "Latest version: v${VERSION}"
    fi

    BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ASSET}"
    BINARY_URL="${BASE_URL}"
    CHECKSUM_URL="${BASE_URL}.sha256"

    # Work in a temp directory
    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT

    BINARY_TMP="${TMP_DIR}/${ASSET}"
    CHECKSUM_TMP="${TMP_DIR}/${ASSET}.sha256"

    info "Downloading ${ASSET}..."
    curl -fsSL --progress-bar -o "$BINARY_TMP" "$BINARY_URL" || \
        die "download failed: ${BINARY_URL}"

    info "Downloading checksum..."
    curl -fsSL -o "$CHECKSUM_TMP" "$CHECKSUM_URL" || \
        die "checksum download failed: ${CHECKSUM_URL}"

    info "Verifying SHA-256 checksum..."
    verify_sha256 "$BINARY_TMP" "$CHECKSUM_TMP"
    ok "Checksum verified."

    # Install
    mkdir -p "$INSTALL_DIR"
    chmod 755 "$BINARY_TMP"
    mv "$BINARY_TMP" "${INSTALL_DIR}/quiver"

    ok "Quiver v${VERSION} installed to ${INSTALL_DIR}/quiver"

    # PATH hint
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            warn "${INSTALL_DIR} is not in your PATH."
            warn "Add the following to your shell profile:"
            warn "  export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
    esac

    ok "Run 'quiver --version' to confirm."
    ok "Run 'quiver serve' to start the server, or 'quiver --help' for all commands."
}

main "$@"
