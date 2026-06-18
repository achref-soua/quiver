# ADR-0039 — One-command install and self-update

**Status:** Proposed
**Date:** 2026-06-18
**Deciders:** Achref Soua

---

## Context

Quiver's current installation story requires the user to either:
- Build from source: `git clone … && cargo install --path crates/quiver-cli` (~5 min, requires a Rust toolchain), or
- Download a release binary manually from GitHub Releases and place it in PATH.

Neither path is suitable for the "spin up Quiver in under two minutes" first-run experience that v0.17.0 targets. Popular developer tools (Rustup, Deno, Bun, Homebrew, uv) converge on the same pattern: a single shell command that downloads, verifies, and installs.

Additionally, once installed there is no first-class upgrade path — the user must repeat the manual download process for each release.

---

## Decision

Ship three artefacts:

### 1. `scripts/install.sh` (Linux / macOS / WSL)

A curl-pipeable POSIX shell script that:

1. Detects the host OS and architecture (`uname -s`, `uname -m`) and maps to the matching GitHub release asset (`quiver-linux-x86_64`, `quiver-linux-aarch64`, `quiver-macos-x86_64`, `quiver-macos-aarch64`).
2. Resolves the latest version (or a caller-specified `QUIVER_VERSION`) via the GitHub Releases API (`/releases/latest`).
3. Downloads the binary and its `.sha256` checksum file.
4. Verifies the checksum with `sha256sum` (Linux) or `shasum -a 256` (macOS).
5. Moves the verified binary to `$QUIVER_INSTALL_DIR` (default `~/.local/bin`; falls back to `~/bin`; honours override).
6. Prints a one-line install confirmation and a PATH hint if the install dir is not in `$PATH`.

Non-goals: no root, no `sudo`, no system-level package manager integration at this stage. No network retries beyond what `curl` provides.

### 2. `scripts/install.ps1` (Windows / PowerShell)

Equivalent logic in PowerShell 5.1+:

1. Detect architecture (`[System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture`).
2. Resolve latest version via GitHub API (`Invoke-RestMethod`).
3. Download binary and `.sha256` checksum.
4. Verify checksum (`Get-FileHash -Algorithm SHA256`).
5. Install to `$env:LOCALAPPDATA\quiver\bin` (created if absent); suggest adding to PATH.

### 3. `quiver update` CLI subcommand

A built-in self-update mechanism so users who already have Quiver can upgrade without re-running the install script:

1. Query the GitHub Releases API for the latest release tag.
2. Compare against the current binary's embedded `CARGO_PKG_VERSION`.
3. If up-to-date, print a confirmation and exit.
4. Otherwise download the new binary for the current platform + architecture and its `.sha256` checksum; verify the checksum.
5. Perform an atomic swap: write to a temp file alongside the current binary, set executable bit, rename over the original (`std::fs::rename`).
6. Exec the new binary with `--version` to confirm success.

The subcommand requires no root on any platform (the binary is user-owned). It uses `reqwest` (already a quiver-server transitive dep via `axum`/`hyper`) with TLS via `rustls` — no new network-stack dependency.

### 4. Release asset checksum generation (CI)

Add a step to `.github/workflows/release.yml` that, for every release binary produced:
- Computes `sha256sum <binary>` and writes `<binary>.sha256`.
- Uploads the `.sha256` file as a release asset alongside the binary.

The scripts and `quiver update` both download this file and verify before installing.

---

## Alternatives considered

### A. Publish to `crates.io` (`cargo install quiver-cli`)

Already done (or can be done). But `cargo install` requires a Rust toolchain and recompiles from source — not suitable for a sub-two-minute first-run. Keep as a secondary option; document it.

### B. OS package managers (Homebrew tap, AUR, Scoop, winget, APT PPA)

High value long-term, but out of scope for v0.17.0 — each requires external setup and ongoing maintenance. The script approach ships now and unblocks package-manager contributions later.

### C. Docker (`docker run ghcr.io/achref-soua/quiver`)

Already documented. The script targets users who want a native binary, not Docker.

### D. GitHub CLI extension (`gh extension install`)

Limited to GitHub CLI users and adds a dependency. The curl pipe pattern reaches anyone with a shell.

---

## Implementation

**Files added/changed:**

- `scripts/install.sh` — POSIX install script
- `scripts/install.ps1` — PowerShell install script
- `crates/quiver-cli/src/commands/update.rs` — `quiver update` implementation
- `crates/quiver-cli/src/main.rs` — wire `Update` subcommand
- `.github/workflows/release.yml` — add checksum generation step
- `README.md` — replace manual-install section with one-liner

**Dependencies added:**

- `reqwest` with `rustls-tls` feature in `quiver-cli` (for the GitHub API call in `quiver update`). If `reqwest` is already in the workspace, reuse; otherwise pin to current stable.
- No new dependencies for the shell scripts.

**Tests:**

- Unit tests for the version-comparison logic (current < latest, current == latest, pre-release tags).
- Integration test: mock the GitHub API response and verify the update flow selects the correct asset URL and validates the checksum gate (correct hash passes, wrong hash errors before install).
- `just verify` must remain green.

---

## Consequences

**Positive:**
- Zero-to-running Quiver in under 90 seconds on a fast connection (download ~10 MB binary, no compilation).
- First-class upgrade path: `quiver update`.
- Competitive with modern developer tools on install experience.

**Negative / risks:**
- Curl-pipe install has a well-known security tradeoff: the user trusts the server at download time. Mitigated by: SHA-256 verification, HTTPS-only, documented `--verify` flag for manual inspection, instructions to download and inspect before running.
- `quiver update` requires write access to the binary's location. If the binary is in a root-owned path (e.g., `/usr/local/bin`), the update will fail with a clear error message pointing back to the install script.
- The atomic rename is not truly atomic on Windows (two processes can both hold handles). Mitigated: write to `<binary>.new`, close handles, delete old, rename — standard pattern for Windows self-update.

---

## Implementation status

- [ ] `scripts/install.sh`
- [ ] `scripts/install.ps1`
- [ ] `quiver update` subcommand
- [ ] Release CI checksum step
- [ ] README updated
- [ ] `just verify` green
- [ ] ADR status → Accepted
