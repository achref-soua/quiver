#!/usr/bin/env sh
# Quiver — one-command installer for Linux and macOS (ADR-0039).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/achref-soua/quiver/main/scripts/install.sh | sh
#
# Environment overrides:
#   QUIVER_VERSION        specific version to install (e.g. "0.17.0"); default: latest
#   QUIVER_INSTALL_DIR    directory to install the binary to; default: ~/.local/bin
set -eu

REPO="achref-soua/quiver"
INSTALL_DIR="${QUIVER_INSTALL_DIR:-${HOME}/.local/bin}"

# ── colour helpers ────────────────────────────────────────────────────────────
# Only emit colour codes when stdout is a real terminal.
if [ -t 1 ]; then
  C_BRONZE='\033[38;2;205;127;50m'  # #CD7F32  theme CHROME
  C_GREEN='\033[38;2;143;179;57m'   # #8FB339  theme OK
  C_CYAN='\033[38;2;63;182;168m'    # #3FB6A8  theme ACCENT
  C_YELLOW='\033[38;2;215;200;0m'   # warnings
  C_RED='\033[38;2;210;85;47m'      # theme ALERT
  C_GRAY='\033[38;2;160;160;160m'   # mid-gray
  C_DARK='\033[38;2;90;90;90m'      # dark gray
  C_RESET='\033[0m'
  C_BOLD='\033[1m'
else
  C_BRONZE=''; C_GREEN=''; C_CYAN=''; C_YELLOW=''
  C_RED=''; C_GRAY=''; C_DARK=''; C_RESET=''; C_BOLD=''
fi

logo() {
  printf "${C_BRONZE}"
  printf '    ██████╗ ██╗   ██╗██╗██╗   ██╗███████╗██████╗ \n'
  printf '   ██╔═══██╗██║   ██║██║██║   ██║██╔════╝██╔══██╗\n'
  printf '   ██║   ██║██║   ██║██║╚██╗ ██╔╝█████╗  ██████╔╝\n'
  printf '   ██║▄▄ ██║██║   ██║██║ ╚████╔╝ ██╔══╝  ██╔══██╗\n'
  printf '   ╚██████╔╝╚██████╔╝██║  ╚██╔╝  ███████╗██║  ██║\n'
  printf '    ╚══▀▀═╝  ╚═════╝ ╚═╝   ╚═╝   ╚══════╝╚═╝  ╚═╝\n'
  printf "${C_RESET}"
  if [ -n "${1:-}" ]; then
    printf "${C_CYAN}        security-first vector database  v%s${C_RESET}\n" "$1"
  else
    printf "${C_CYAN}        security-first vector database${C_RESET}\n"
  fi
  printf '\n'
  printf "${C_DARK}  ┌──────────────────────────────────────────────┐${C_RESET}\n"
  printf "${C_DARK}  │  encrypted · memory-frugal · self-hostable   │${C_RESET}\n"
  printf "${C_DARK}  └──────────────────────────────────────────────┘${C_RESET}\n"
  printf '\n'
}

step()  { printf "  ${C_CYAN}%s${C_RESET} %s\n" "$1" "$2"; }
ok()    { printf "  ${C_GREEN}✔${C_RESET}  %s\n" "$1"; }
warn()  { printf "  ${C_YELLOW}!${C_RESET}  %s\n" "$1" >&2; }
die()   { printf "\n  ${C_RED}ERROR:${C_RESET} %s\n\n" "$1" >&2; exit 1; }

need() { command -v "$1" > /dev/null 2>&1 || die "required tool not found: $1"; }

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
    x86_64|amd64)  echo "x86_64";;
    aarch64|arm64) echo "aarch64";;
    *)             die "unsupported architecture: $(uname -m)";;
  esac
}

# ── checksum verification ─────────────────────────────────────────────────────

verify_sha256() {
  # $1 = binary file, $2 = checksum file
  expected="$(awk '{print $1}' "$2")"
  if command -v sha256sum > /dev/null 2>&1; then
    actual="$(sha256sum "$1" | awk '{print $1}')"
  elif command -v shasum > /dev/null 2>&1; then
    actual="$(shasum -a 256 "$1" | awk '{print $1}')"
  else
    warn "no sha256sum or shasum found — skipping checksum verification"
    return 0
  fi
  [ "$actual" = "$expected" ] || die "SHA-256 mismatch\n  expected: $expected\n  got:      $actual"
}

# ── progress download ─────────────────────────────────────────────────────────

download() {
  # $1 = url, $2 = output file, $3 = label
  printf "  ${C_CYAN}⬇${C_RESET}  Downloading %s" "$3"
  curl -fsSL --progress-bar -o "$2" "$1" 2>/dev/null \
    && printf " ${C_DARK}done${C_RESET}\n" \
    || die "download failed: $1"
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
  step '⟳' 'Checking latest release...'
  if [ -n "${QUIVER_VERSION:-}" ]; then
    VERSION="${QUIVER_VERSION#v}"
  else
    VERSION="$(curl -fsSL \
      -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/${REPO}/releases/latest" \
      | grep '"tag_name"' \
      | head -1 \
      | sed 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/')"
    [ -n "$VERSION" ] || die "could not determine latest version from GitHub API"
  fi

  # Show logo now that we have the version
  logo "$VERSION"

  BINARY_URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ASSET}"
  CHECKSUM_URL="${BINARY_URL}.sha256"

  TMP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TMP_DIR"' EXIT
  BINARY_TMP="${TMP_DIR}/${ASSET}"
  CHECKSUM_TMP="${TMP_DIR}/${ASSET}.sha256"

  step '⬇' "Fetching v${VERSION} for ${OS}/${ARCH}..."
  download "$BINARY_URL"   "$BINARY_TMP"   "$ASSET"
  download "$CHECKSUM_URL" "$CHECKSUM_TMP" "${ASSET}.sha256"

  step '🔒' 'Verifying SHA-256 checksum...'
  verify_sha256 "$BINARY_TMP" "$CHECKSUM_TMP"
  ok "Checksum verified."

  mkdir -p "$INSTALL_DIR"
  chmod 755 "$BINARY_TMP"
  mv "$BINARY_TMP" "${INSTALL_DIR}/quiver"

  # ── icon + launcher integration ──────────────────────────────────────────
  ICON_URL="https://github.com/${REPO}/releases/download/v${VERSION}/quiver-256.png"

  if [ "$OS" = "linux" ]; then
    ICON_DIR="${HOME}/.local/share/icons/hicolor/256x256/apps"
    APPS_DIR="${HOME}/.local/share/applications"
    ICON_PATH="${ICON_DIR}/quiver.png"
    mkdir -p "$ICON_DIR" "$APPS_DIR"
    if curl -fsSL -o "$ICON_PATH" "$ICON_URL" 2>/dev/null; then
      # Write the .desktop file so app launchers (GNOME, KDE, etc.) pick it up.
      cat > "${APPS_DIR}/quiver.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Quiver
Comment=Security-first, memory-frugal vector database
Exec=${INSTALL_DIR}/quiver demo
Icon=quiver
Terminal=true
Categories=Development;Science;
DESKTOP
      command -v update-desktop-database > /dev/null 2>&1 \
        && update-desktop-database "$APPS_DIR" 2>/dev/null || true
      ok "App launcher entry created (icon + .desktop)."
    else
      warn "Could not fetch icon — skipping .desktop integration."
    fi

  elif [ "$OS" = "macos" ]; then
    # On macOS, bare Mach-O binaries don't carry embedded icons.  We create a
    # minimal .app bundle so Finder / Spotlight / the Dock can show the custom icon.
    APP_BUNDLE="${HOME}/Applications/Quiver.app"
    MACOS_DIR="${APP_BUNDLE}/Contents/MacOS"
    RES_DIR="${APP_BUNDLE}/Contents/Resources"
    mkdir -p "$MACOS_DIR" "$RES_DIR"
    # Symlink the real binary inside the bundle (update keeps the symlink valid).
    ln -sf "${INSTALL_DIR}/quiver" "${MACOS_DIR}/quiver"
    # Convert the PNG icon to icns using sips + iconutil (both ship with macOS).
    ICON_PNG="${RES_DIR}/quiver.png"
    if curl -fsSL -o "$ICON_PNG" "$ICON_URL" 2>/dev/null && command -v sips > /dev/null 2>&1; then
      ICONSET="${RES_DIR}/quiver.iconset"
      mkdir -p "$ICONSET"
      for sz in 16 32 64 128 256 512; do
        sips -z $sz $sz "$ICON_PNG" --out "${ICONSET}/icon_${sz}x${sz}.png" > /dev/null 2>&1 || true
      done
      if command -v iconutil > /dev/null 2>&1; then
        iconutil -c icns "$ICONSET" -o "${RES_DIR}/quiver.icns" 2>/dev/null \
          && rm -rf "$ICONSET" || true
      fi
    fi
    # Write a minimal Info.plist so Finder registers the bundle.
    cat > "${APP_BUNDLE}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>quiver</string>
  <key>CFBundleIdentifier</key><string>io.quiver.app</string>
  <key>CFBundleName</key><string>Quiver</string>
  <key>CFBundleIconFile</key><string>quiver</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>LSUIElement</key><true/>
</dict></plist>
PLIST
    ok "Quiver.app bundle created at ${APP_BUNDLE}"
  fi

  printf '\n'
  printf "${C_DARK}  ┌──────────────────────────────────────────────┐${C_RESET}\n"
  printf "${C_GREEN}  │  ✔  Quiver v%-35s│${C_RESET}\n" "${VERSION} installed!"
  printf "${C_DARK}  │     %-44s│${C_RESET}\n" "${INSTALL_DIR}/quiver"
  printf "${C_DARK}  └──────────────────────────────────────────────┘${C_RESET}\n"
  printf '\n'

  # ── auto-add to PATH (no manual steps) ──────────────────────────────────
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
      _added=0
      for _rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
        if [ -f "$_rc" ] && ! grep -qF "$INSTALL_DIR" "$_rc" 2>/dev/null; then
          printf '\n# Quiver\nexport PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$_rc"
          _added=1
        fi
      done
      if [ "$_added" = "1" ]; then
        ok "Added ${INSTALL_DIR} to PATH in your shell profiles."
        warn "Open a new terminal (or run: source ~/.bashrc) to use 'quiver'."
      fi
      ;;
  esac

  printf '  Next steps:\n'
  printf "  ${C_BRONZE}quiver demo${C_RESET}              ${C_DARK}# zero-config: seed vectors + open cockpit${C_RESET}\n"
  printf "  ${C_BRONZE}quiver serve${C_RESET}             ${C_DARK}# start the server (gRPC + REST on :6333)${C_RESET}\n"
  printf "  ${C_BRONZE}quiver tui${C_RESET}               ${C_DARK}# open the retro cockpit${C_RESET}\n"
  printf "  ${C_BRONZE}quiver update${C_RESET}            ${C_DARK}# self-update to the latest release${C_RESET}\n"
  printf "  ${C_BRONZE}quiver --help${C_RESET}            ${C_DARK}# all commands${C_RESET}\n"
  printf '\n'
}

main "$@"
