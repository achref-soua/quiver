# Self-hosting & configuration

Quiver is one static binary. Every option is an environment variable with a
**secure default**; the full list lives in
[`.env.example`](https://github.com/achref-soua/quiver/blob/main/.env.example) and
the rationale in
[ADR-0013](https://github.com/achref-soua/quiver/blob/main/docs/adr/0013-config-and-secure-defaults.md).

## Encryption at rest

Encryption-at-rest is **on by default**. The server requires a 256-bit key in
`QUIVER_ENCRYPTION_KEY` (generate one with `openssl rand -hex 32`) unless you opt
out with `QUIVER_INSECURE=true`. It seals segments, the manifest, and the WAL
alike. That key is a **master key** that wraps a per-collection data-encryption key
(envelope encryption,
[ADR-0010](https://github.com/achref-soua/quiver/blob/main/docs/adr/0010-crypto-envelope-aead.md)),
so dropping a collection **crypto-shreds** it. For production, hold the master key
in a file via `QUIVER_MASTER_KEY_FILE` rather than an environment variable.

```bash
export QUIVER_ENCRYPTION_KEY=$(openssl rand -hex 32)
quiver serve
```

## TLS

TLS (via `rustls`) is **required for any non-loopback bind**. Provide a certificate
and key; for an extra factor, set `QUIVER_TLS_CLIENT_CA` to require **mutual TLS**,
so both transports demand a client certificate chaining to that CA.

## Authentication & RBAC

Authentication is by API key; authorization is **default-deny RBAC**. A bare
`QUIVER_API_KEYS` secret is an all-collections admin key. For least privilege,
define scoped keys in `quiver.toml` with a `role` (`read` ⊆ `write` ⊆ `admin`) and
a `collections` scope — exact names or a trailing-`*` prefix (e.g. `acme.*`) for
per-namespace isolation. Over-scope and cross-namespace access return `403`, and
listing hides collections outside the scope. See
[ADR-0011](https://github.com/achref-soua/quiver/blob/main/docs/adr/0011-authn-authz-tenancy.md).

## Audit logging

Set `QUIVER_AUDIT_LOG` to record every mutating and administrative operation, and
every denial, to an append-only audit log — the acting key, the action, the
resource, and the outcome, **never the secret**.

## Running with Docker

```bash
just docker                              # build the image (infra/docker/Dockerfile)
docker run --rm -p 6333:6333 -p 6334:6334 \
  -e QUIVER_ENCRYPTION_KEY=$(openssl rand -hex 32) \
  -v quiver-data:/data quiver:dev serve
```

The image is multi-stage and runs as a non-root user. See
[`infra/`](https://github.com/achref-soua/quiver/tree/main/infra) for the
Dockerfile and deployment scaffolding.

## Replication

Run asynchronous read replicas by pointing a follower at a leader with
`QUIVER_LEADER_URL` (and `QUIVER_LEADER_API_KEY`). See [Replication](features/replication.md).

## Observability

Quiver exposes structured logs and metrics; see
[ADR-0014](https://github.com/achref-soua/quiver/blob/main/docs/adr/0014-observability.md).
The cockpit (`quiver tui`) shows live server metrics and a collection browser.
