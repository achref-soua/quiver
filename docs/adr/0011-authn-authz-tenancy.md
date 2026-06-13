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
