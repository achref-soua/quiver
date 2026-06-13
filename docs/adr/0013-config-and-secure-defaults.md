# ADR-0013: Configuration & secure defaults

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

A security-first system must be safe in its **default** configuration and must fail fast on insecure or invalid settings rather than silently running unsafely.

## Decision

- **Typed, layered config** via `figment` (defaults → file → environment), **validated at startup**; the server refuses to boot on invalid or insecure-without-opt-out configuration (with a clear message).
- **Secure defaults:**
  - encryption-at-rest **on**;
  - **TLS required** for any non-loopback bind (plaintext on a public interface requires an explicit, warned `--insecure` opt-out);
  - bind to **localhost** by default;
  - **no default API key**, no anonymous writes;
  - durability = per-commit `fsync` (ADR-0005);
  - conservative query **cost caps** on.
- **Secrets** come only from env/KMS/files (referenced, not inlined); never logged; `.env.example` documents every variable with type, default, and required/optional.

## Consequences

- **+** A fresh deployment is safe without tuning; misconfiguration is caught at boot; the secrets posture is consistent.
- **−** Some friction for quick insecure local experiments (mitigated by an explicit, clearly-labelled `--insecure`/dev profile that is never the default).

## Alternatives considered

- **Insecure-by-default + hardening guide** — rejected: contradicts the security-first thesis; most users never read the guide.
- **Ad-hoc env parsing** — rejected: no validation, easy to misconfigure; `figment` + a typed schema gives fail-fast validation.
