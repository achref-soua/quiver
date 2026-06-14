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
- **Tenancy depth:** the current engine stores collections in one flat namespace, so org→namespace isolation is realized as collection-scope prefixes enforced above the engine. Per-tenant engine partitioning (separate id-spaces/handles) is a Phase-4 deepening.
- **Not yet implemented:** mTLS client-certificate identities (next slice); an append-only audit log and per-key rate/cost limits (the audit slice); a key-issuance API that stores only a hash and shows the secret once (keys are operator-provisioned via config today).

[`CollectionScope`]: ../../crates/quiver-server/src/auth.rs
