# ADR-0011: AuthN/Z & tenant isolation

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

The server is multi-tenant and must authenticate callers, authorize each resource access with least privilege, and guarantee one tenant cannot reach another's data. See the threat model adversaries A2/A5.

## Decision

- **AuthN:** **API keys** (a public prefix + a high-entropy secret; only a hash is stored; the secret is shown once) and optional **mTLS** (client identity = certificate subject mapped to a principal).
- **AuthZ:** **RBAC** with **scoped** keys over resources — e.g. `collection:read`, `collection:write`, `collection:admin`, `keys:admin`, scoped to specific collections/namespaces. **Default-deny**; no anonymous writes; no default credentials.
- **Tenancy:** `org → namespace → collection`. Tenant context is attached at authentication and **enforced at the data-access layer** — every storage/index access is scoped by tenant; there is no code path that crosses tenants. (Engine handles are partitioned per tenant; cross-tenant ids are unconstructable from a tenant-scoped context.)
- **Accountability:** an **append-only audit log** (actor, action, resource, time; optionally hash-chained for tamper-evidence) for mutating and admin operations; per-key/tenant **rate limits** and query **cost caps**.

## Consequences

- **+** Least-privilege access; stolen-key blast radius bounded by scope; tenant isolation is structural, not advisory; actions are auditable.
- **−** Key/role management surface (issue, scope, rotate, revoke) to build and document; audit log storage growth (rotated/retained per policy).

## Alternatives considered

- **Static shared token** — rejected: no scoping, no revocation, no tenancy.
- **Full OIDC/JWT for the data plane** — heavier than needed for service-to-service vector access in v1; API keys + mTLS suffice. OIDC for an admin console is a later option.
- **Isolation enforced only in the API layer** — rejected: defense must be at the data layer so a logic bug above can't cross tenants.

## Implementation

- **Scoped API keys + RBAC (shipped):** keys are provisioned in config (`quiver-server`'s `auth` module). Each key carries a **role** — `Read ⊆ Write ⊆ Admin` — and a [`CollectionScope`] (all, exact names, or a trailing-`*` prefix that namespaces collections, e.g. `acme.*`). A bare secret string stays an all-collections admin (back-compat). Authorization is **default-deny** and enforced at the engine-facing op layer (`AppState`), so both REST and gRPC share one choke point; `list` is filtered to the caller's scope so out-of-scope collection names never leak. Over-scope and cross-namespace access return HTTP `403` / gRPC `PermissionDenied`, proven end-to-end in `quiver-server/tests/rbac.rs`.
- **Mutual TLS (shipped):** setting `tls_client_ca` (a CA certificate) makes both transports require a client certificate chaining to that CA — REST via a rustls `WebPkiClientVerifier`, gRPC via tonic's `client_ca_root`, both mandatory. This is a transport-layer second factor *in addition to* the bearer key (which still carries the RBAC scope); `quiver-server/tests/tls.rs` proves a CA-signed client is served while a client with no certificate is refused.
- **Audit logging (shipped):** at the same `AppState` choke point, every **mutating** and **administrative** operation (`create_collection`, `delete_collection`, `upsert`, `delete_points`) and every **access-control denial** is recorded with the acting principal, the action, the resource, and the outcome (`ok`/`denied`/`error`). Records are emitted as structured `tracing` events (target `quiver::audit`) and, when `audit_log` (`QUIVER_AUDIT_LOG`) names a file, appended as JSON Lines for an external pipeline. A key's actor identity is its configured non-secret `id`, or else a short, preimage-resistant SHA-256 fingerprint of its secret (`key:<hex>`), so the log attributes an action to a key **without ever recording the secret**; successful reads are not logged (volume), but denied reads are. Proven end-to-end in `quiver-server/tests/audit.rs`; details in [`../security/audit.md`](../security/audit.md).
- **Tenancy depth:** the current engine stores collections in one flat namespace, so org→namespace isolation is realized as collection-scope prefixes enforced above the engine. Per-tenant engine partitioning (separate id-spaces/handles) is a Phase-4 deepening.
- **Not yet implemented:** mapping a client-certificate *subject* to a distinct principal (mTLS currently gates the connection; identity/scope still comes from the bearer key); per-key rate/cost limits; tamper-evident **hash-chaining** of the audit log (it is append-only today); a key-issuance API that stores only a hash and shows the secret once (keys are operator-provisioned via config today).

[`CollectionScope`]: ../../crates/quiver-server/src/auth.rs
