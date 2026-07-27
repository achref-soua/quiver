# Security Policy

Security is Quiver's foundation. Reports are welcome and will be handled in good
faith — including now that the project is finished.

## Project status, and what it means for security

**`v1.1.0` is Quiver's final planned release.** Feature development stopped
deliberately rather than drifting to a halt: what the project chose not to build, and
why, is written down in [ADR-0083](docs/adr/0083-closing-the-backlog.md).

That has to change this policy, because a security policy nobody will action is worse
than an honest one. Concretely:

- **Supported version: `v1.1.0`, the latest release, on a best-effort basis.** No
  earlier line is supported. There is no service-level agreement and no promise of a
  patched release on any timetable.
- **Reports are still read and still answered**, by the author, at the addresses
  below. Expect an acknowledgement within a few days in the ordinary case, and no
  guarantee beyond that.
- **A serious, credible vulnerability will get a fix and a release.** "Final planned
  release" is a statement about features, not about walking away from a security bug
  in a database other people may be running. If a report warrants it, `v1.1.1` will
  exist.
- **A low-severity or hardening-only report may get an advisory and no release.** It
  will still be acknowledged publicly, so anyone running Quiver — or forking it —
  knows what they are looking at.
- **Quiver is AGPL-3.0 and forkable.** If you need guaranteed maintenance, fork it;
  that is what the licence is for. The full history, every ADR, the threat model and
  the test suite come with it, which is most of what maintaining it actually takes.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

- Use GitHub's **private vulnerability reporting** ("Report a vulnerability" under the
  repository's *Security* tab), or
- email **achref.soua@outlook.com** with details and, ideally, a reproduction.

Both reach the author, Achref Soua, who is the sole maintainer — there is no security
team and no paid bounty, and saying so plainly is fairer than implying otherwise. If a
report leads to a fix you will be credited, unless you prefer otherwise.

If the repository has been archived by the time you read this, GitHub's private
reporting may be unavailable. **Use the email address**, which is not going anywhere.

## Scope & posture

Quiver's security design — assets, adversaries, trust boundaries, and what the server
can and cannot see — is documented honestly in:

- [`docs/security/threat-model.md`](./docs/security/threat-model.md)
- [`docs/security/crypto.md`](./docs/security/crypto.md)
- [`docs/security/audit-0.29.0.md`](./docs/security/audit-0.29.0.md) — the latest
  security audit: a static OWASP-style review, a dynamic
  [OWASP ZAP](https://www.zaproxy.org/) scan of a live server, and the fuzzers re-run,
  with every finding fixed and regression-tested.
- [`docs/security/fuzzing.md`](./docs/security/fuzzing.md) — the recorded fuzz soaks.

Key facts worth stating up front:

- Encryption-at-rest is **on by default** and covers **every durable byte** — segments,
  the manifest, and the record-framed write-ahead log — sealed with
  XChaCha20-Poly1305 under HKDF-SHA256 subkeys. It protects against stolen
  disks/backups, **not** against an attacker with root on a live host reading process
  memory.
- **Client-side payload encryption protects payloads, not vectors** — standard ANN
  requires plaintext vectors server-side.
- Quiver uses **only audited cryptographic libraries** (RustCrypto AEAD/KDF and
  `rustls`) and implements no primitives of its own. Any property-preserving
  encryption for vectors is experimental, behind a feature flag, with documented
  leakage caveats.
- The at-rest format is **deliberately unchanged in the final release**. Switching the
  AEAD to AES-256-GCM measured roughly 2.7× on this hardware and was declined anyway,
  because a late mistake in the at-rest format of an encrypted database means
  unreadable data rather than a slow query
  ([ADR-0083](docs/adr/0083-closing-the-backlog.md)).

## Hardening checklist (operators)

Set a strong `QUIVER_ENCRYPTION_KEY` (256-bit, e.g. `openssl rand -hex 32`) sourced
from a secret store or `0600` file — never the committed config; require TLS on
non-loopback binds; scope API keys to least privilege; enable audit log retention; and
run the latest release.
