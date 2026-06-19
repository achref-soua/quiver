# ADR-0044 — Automated, tag-triggered multi-platform release assets

**Status:** Accepted
**Date:** 2026-06-19
**Deciders:** Achref Soua

---

## Context

`quiver update`, `scripts/install.sh`, and `scripts/install.ps1` all download a
per-platform binary plus its `.sha256` from the GitHub release matching the
latest tag. The asset name is derived in `crates/quiver-cli/src/update.rs`:

```
quiver-<os>-<arch>        # unix
quiver-<os>-<arch>.exe    # windows
```

Two defects let a release ship without the assets those clients require:

1. **`release.yml` had no Windows job.** Its matrix built Linux (x86_64 +
   aarch64) and macOS (x86_64 + aarch64) only. The Windows `.exe` files attached
   to the v0.17.0 / v0.17.2 releases were built by hand on a Windows machine and
   uploaded manually — an out-of-band step that is easy to forget.

2. **v0.18.0 shipped with zero assets.** The tag was created but no release
   build ran, so `quiver update` from a Windows v0.17.2 client resolved
   `…/download/v0.18.0/quiver-windows-x86_64.exe` and failed with a 404:

   ```
   Error: failed to download
   https://github.com/achref-soua/quiver/releases/download/v0.18.0/quiver-windows-x86_64.exe
   ```

The deeper cause of (2): the release workflow is `workflow_dispatch`-only
(ADR-0015 made every workflow manual, with local `just verify` as the
authoritative gate). A tag can therefore be pushed with no asset build behind
it, and there is no signal that assets are missing.

## Decision

Make release-asset publishing **automatic and complete** for every supported
platform.

1. **Add the Windows job.** Extend the `release.yml` build matrix with
   `windows-latest` / `x86_64-pc-windows-msvc`, producing
   `quiver-windows-x86_64.exe` (icon embedded natively via `winresource`,
   ADR-0039) and its `.sha256`. Add both to the publish step's `files:` list.

2. **Trigger on tag push.** Add `on: push: tags: ['v[0-9]+.[0-9]+.[0-9]+']` so
   that tagging a release (the existing `develop → main`, tag-on-`main` flow)
   builds and attaches all five platform binaries with no extra step. Keep
   `workflow_dispatch` (with a `tag` input) as a manual fallback to (re)build
   assets for an existing tag.

3. **Unify packaging across platforms** in one `shell: bash` step (Git Bash on
   the Windows runner provides `cp` + `sha256sum`); the `.exe` suffix is the
   only per-OS difference.

4. **Pass the resolved tag through `env:`**, never interpolated into the script
   body, per GitHub's workflow-injection guidance.

This narrows ADR-0015 for the release workflow specifically: test/build *gates*
stay manual-by-design, but release *publishing* is automated on tag push — the
binaries an end user downloads must never depend on a remembered manual step.

5. **Document a local fallback** (`just release-local`) that reproduces the CI
   build on a maintainer machine and uploads via `gh release`, for when hosted
   runners are unavailable (see Constraint below).

## Constraint: GitHub Actions billing lock

At the time of this ADR the account's hosted runners are **billing-locked** — a
dispatched job is rejected in ~5 s with *"The job was not started because your
account is locked due to a billing issue."* Until that is cleared, **none** of
the four workflows can execute, so the automation above cannot run on push and
v0.18.0's missing assets cannot be backfilled by CI.

Therefore:

- The workflow change is the durable fix: the moment billing is restored,
  tagging a release publishes every platform binary automatically, Windows
  included — no manual upload, no missing-asset class of bug.
- In the interim, releases are cut locally. `just release-local` builds the
  targets the maintainer's toolchain supports (a Linux→Windows binary needs the
  `x86_64-pc-windows-gnu` target + `gcc-mingw-w64`; a native Windows build needs
  a Windows host), generates checksums, and uploads them to the tag with `gh`.

Clearing the billing lock is the single owner action that turns this from
"works locally" into "works automatically." It is out of scope for code.

## Alternatives considered

- **Cross-compile Windows from Linux in CI (`x86_64-pc-windows-gnu`).** Works
  (build.rs already supports it), but a native `windows-latest` MSVC build is
  the more compatible, lower-surprise artifact and is free on hosted runners.
  Rejected in favour of the native job; the gnu cross path remains the
  documented *local* fallback.
- **Keep Windows a manual upload.** This is exactly the process that produced
  the v0.18.0 outage. Rejected.
- **Trigger on GitHub `release: published` instead of tag push.** Equivalent,
  but tag push matches the existing tag-on-`main` release ritual and needs no
  separate "create release" click. Rejected for less ceremony.

## Consequences

**Positive**
- A pushed SemVer tag yields a complete, checksummed, multi-platform release —
  Windows included — with no out-of-band steps.
- `quiver update` / `install.sh` / `install.ps1` can no longer 404 on a missing
  platform asset for a tagged release built by CI.
- Injection-safe tag handling.

**Negative / risks**
- Automation is gated on the billing lock being cleared (documented above).
- Tag-triggered publishing means a mistakenly-pushed tag would publish a
  release; mitigated by the strict `v*.*.*` tag pattern and the existing
  protected `main` + tag-on-`main`-only convention.

## Implementation status

- [x] `release.yml` — Windows job + tag-push trigger + unified packaging + env-passed tag
- [x] `just release-local` fallback recipe
- [x] ADR recorded
- [ ] Billing lock cleared (owner action — enables CI automation)
- [ ] First tag-triggered release verified green (blocked on the line above)
