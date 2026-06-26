# Security Policy

Security is Quiver's foundation. We welcome reports and will work with you in good faith.

## Supported versions

Quiver is pre-1.0 and under active development. Security fixes target the latest `main`. Once `v1.0.0` ships, this section will list supported release lines.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

- Use GitHub's **private vulnerability reporting** ("Report a vulnerability" under the repository's *Security* tab), or
- email **achref.soua@outlook.com** with details and, ideally, a reproduction.

You can expect an acknowledgement within a few days. We will confirm the issue, agree on a disclosure timeline, fix it, and credit you (unless you prefer otherwise). There is no paid bounty at this time.

## Scope & posture

Quiver's security design — assets, adversaries, trust boundaries, and what the server can and cannot see — is documented honestly in:

- [`docs/security/threat-model.md`](./docs/security/threat-model.md)
- [`docs/security/crypto.md`](./docs/security/crypto.md)
- [`docs/security/audit-0.29.0.md`](./docs/security/audit-0.29.0.md) — the latest security audit: a static OWASP-style review, a dynamic [OWASP ZAP](https://www.zaproxy.org/) scan of a live server, and the fuzzers re-run, with every finding fixed and regression-tested.

Key facts worth stating up front:

- Encryption-at-rest is **on by default** and covers **every durable byte** — segments, the manifest, and the record-framed write-ahead log — sealed with XChaCha20-Poly1305 under HKDF-SHA256 subkeys. It protects against stolen disks/backups, **not** against an attacker with root on a live host reading process memory.
- **Client-side payload encryption protects payloads, not vectors** — standard ANN requires plaintext vectors server-side.
- Quiver uses **only audited cryptographic libraries** (RustCrypto AEAD/KDF and `rustls`) and implements no primitives of its own. Any property-preserving encryption for vectors is experimental, behind a feature flag, with documented leakage caveats.

## Hardening checklist (operators)

Set a strong `QUIVER_ENCRYPTION_KEY` (256-bit, e.g. `openssl rand -hex 32`) sourced from a secret store or `0600` file — never the committed config; require TLS on non-loopback binds; scope API keys to least privilege; enable audit log retention; and keep Quiver updated.
