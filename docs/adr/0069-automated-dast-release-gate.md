# ADR-0069: Automated OWASP ZAP DAST gate that blocks a release

- **Status:** Accepted
- **Date:** 2026-07-01
- **Deciders:** Achref Soua

## Context

Quiver's security has been verified by a **manual** OWASP ZAP scan at audit time
(the v0.29.0 audit ran `zap-baseline.py` + `zap-api-scan.py` against a live
server: 0 FAIL, [`audit-0.29.0.md`](../security/audit-0.29.0.md)). A manual scan
proves the posture at one moment; it does not stop a *future* change from
regressing it. For a security-first database, a dynamic scan should run on its own
— every release — and a real finding should **stop the release**, not be
discovered afterwards.

ADR-0015 keeps the fast `just verify` gate (fmt/clippy/test/doc) as the
pre-commit check and CI mirror; this ADR adds the heavier dynamic (DAST) layer on
top, where it belongs — at the release boundary — without slowing every push.

## Decision

Add a reusable **`dast`** workflow (`.github/workflows/dast.yml`,
`on: workflow_call`) that:

1. builds the server and boots it with a **production-style secure config**
   (encryption-at-rest on, an admin API key required, loopback bind), then seeds a
   collection so the API surface is real;
2. runs an **OWASP ZAP baseline** scan (spider + passive rules) against the live
   server, and an **OWASP ZAP API scan** (the full active rule set) over the
   committed OpenAPI spec (`docs/api/openapi.yaml`), authenticated with the admin
   key via a ZAP request-header replacer;
3. reads `.zap/rules.tsv` for the alert policy and **fails the job on a FAIL-level
   alert**; the HTML/MD/XML reports are always uploaded as artifacts.

Wiring:

- **`release.yml`** calls it as a job `dast`, and the `release` job (and thus
  every downstream `publish-*`) `needs: [build, dast]`. A FAIL-level alert fails
  `dast`, which blocks the release and all package publishes — **no release until
  fixed**.
- **`ci.yml`** calls the same reusable workflow on **main/develop pushes and
  manual dispatch** (skipped on pull requests — it builds and boots the server and
  pulls the ZAP image, too heavy for every push), so a regression is caught on the
  mainline before a release tag reaches the hard gate.

### Alert policy (`.zap/rules.tsv`)

ZAP marks findings WARN by default. The policy file:

- **promotes the security-critical active rules to FAIL** (SQL/OS-command/LDAP/
  template/code injection, reflected & stored XSS, path traversal, remote file
  inclusion, XXE, CRLF injection, cloud-metadata exposure) so a genuine
  vulnerability blocks the release;
- **IGNOREs reviewed benign findings, with a reason** (today: `10049`
  Non-Storable Content on 401 responses — auth failures are intentionally
  non-cacheable and the JSON API serves no static/robots/sitemap content).

WARN-level header/informational findings do not block (they would red the gate on
ZAP's opinionated, JSON-API-inapplicable header heuristics); they are uploaded as
reports for triage, and any that *should* block are promoted to FAIL in the policy
file. This keeps the gate a true **0-FAIL** signal rather than a brittle one that
blocks releases on non-issues.

## Consequences

- **+** Every release is gated on a live dynamic scan; a real injection/disclosure
  regression stops the release automatically.
- **+** The scan is versioned, reproducible, and reuses the committed OpenAPI spec
  and the same ZAP image the audit used.
- **+** New benign findings are triaged into `.zap/rules.tsv` with a reason,
  keeping the audit trail honest and the gate meaningful.
- **−** The release path gains a ~several-minute DAST job (build + boot + two
  scans). Acceptable at the release cadence; skipped on PRs to keep them fast.
- **−** The pinned `ghcr.io/zaproxy/zaproxy:stable` image floats; a ZAP update that
  adds a rule can turn a release red until the finding is triaged — which is the
  intended "human reviews before release" behavior, not a regression.

## Alternatives considered

- **Keep DAST manual (status quo).** Rejected: proves a point-in-time posture,
  cannot stop a future regression from shipping.
- **Run DAST on every PR (blocking).** Rejected as the default: too heavy for
  every push; the mainline (main/develop) + release coverage catches regressions
  before a tag with far less cost. Still available via manual dispatch.
- **Fail on any WARN (strictest).** Rejected: ZAP's passive header rules
  (CSP/X-Frame-Options/cache) fire on a JSON API where they do not apply, which
  would block releases on non-issues; promoting the real vuln classes to FAIL is
  the meaningful, non-brittle line.
