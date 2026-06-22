# ADR-0049 — Per-key rate limiting (token bucket, RateLimit headers, 429)

**Status:** Proposed
**Date:** 2026-06-22
**Deciders:** Achref Soua

---

## Context

ADR-0040 added per-*request* cost limits (caps on `k`, `ef_search`, dimension,
payload, batch) so a single oversized request cannot exhaust the single-writer
engine, and explicitly named **per-key rate limiting as a later phase**. The gap it
left: an authenticated key can still issue *unbounded numbers* of well-formed
requests, monopolizing the single-writer engine and starving other tenants. The
cost limits bound the size of one request; they do not bound the *rate*.

The constraints from ADR-0040 still hold: bound the work fairly, reject (don't
silently degrade) with a clear signal, keep it opt-in with generous/disabled
defaults, and enforce at one place so both transports are covered by one
implementation. Quiver is single-node single-writer (ADR-0006), so the limiter is
**in-process** — a distributed limiter (shared Redis bucket across replicas) is out
of scope until there is a cluster (it would belong with ADR-0017-distributed, #17).

## Decision

Add an **opt-in, in-memory token-bucket rate limiter keyed by API key**, enforced
at the authentication choke point of both transports, advertising the standard
`RateLimit-*` headers and returning **429 / gRPC `ResourceExhausted`** with
`Retry-After` when a key exceeds its rate.

### Token bucket (pure, `rate_limit` module)

A classic token bucket per key: a bucket holds up to `burst` tokens and refills at
`requests_per_second` tokens/sec; each request consumes one. The core is a pure
`TokenBucket { capacity, refill_per_sec, tokens, last_refill }` with
`try_consume(now) -> Result<Snapshot, RetryAfter>` — time is an injected parameter,
so it is deterministic and unit-tested to full coverage with no sleeps. The
`RateLimiter` wraps `Mutex<HashMap<actor, TokenBucket>>`.

> `// ponytail: one global Mutex over the bucket map — fine for a single-node
> server; shard by key hash if lock contention ever shows up under load.`

The bucket key is the key's **non-secret actor identity** (`Principal::actor()` —
the label or a SHA-256 fingerprint, never the secret), the same identity the audit
log uses. In `insecure` mode (no keys) all callers share one bucket.

### Configuration (opt-in, off by default)

A `[rate_limit]` table (and `QUIVER_RATE_LIMIT_*` env), mirroring ADR-0040's
`[limits]`:

```toml
[rate_limit]
requests_per_second = 50    # refill rate per key; 0 (default) = disabled
burst               = 100   # bucket capacity (max instantaneous burst)
```

`requests_per_second = 0` (the default) disables the limiter entirely — like every
other guard, it is opt-in. A per-key override (a different rate on a specific
`ApiKey`) is a documented follow-up; the global default covers the common need
first without enlarging the key-config surface now.

### Enforcement (one limiter, both transports)

- **REST:** in the existing `auth` middleware, after the principal is resolved,
  consume a token. On success, attach `RateLimit-Limit`, `RateLimit-Remaining`, and
  `RateLimit-Reset` (seconds until a token is available) to the response. On
  exhaustion, short-circuit with **429** + `Retry-After` + the same headers, before
  the handler runs.
- **gRPC:** the handlers already funnel through one `authenticate(&request)`; it
  gains a rate-limit check and the call sites use `authenticate_limited`, returning
  **`ResourceExhausted`** with the retry delay in the message (gRPC has no standard
  RateLimit header; the status is the signal).

Both call one `AppState::check_rate_limit(actor)`; the headers are computed from the
returned snapshot. Health/readiness/metrics endpoints are unauthenticated and are
not rate-limited.

## Consequences

- A single key can no longer monopolize the engine by request volume; other tenants
  get their fair share, closing the half of the DoS surface ADR-0040 left open.
- Clients get a standard, machine-readable signal (`RateLimit-*` + 429/`Retry-After`)
  to back off, rather than opaque slowness or errors.
- In-memory, per-process: limits are per node and reset on restart (acceptable for a
  single-node deployment; a distributed bucket is a clustering concern, noted as out
  of scope). Memory is one small bucket per active key.
- Opt-in and disabled by default, so existing deployments are unchanged until they
  set a rate; no on-disk format change, so the crash gate is untouched.

## Alternatives considered

- **Leaky bucket / fixed or sliding window counters.** Token bucket is the standard
  for "steady rate + allowed burst", is trivially pure-testable, and maps cleanly to
  the `RateLimit-*` semantics. A fixed window has burst-at-boundary unfairness; a
  sliding-log costs more memory. Rejected in favour of the token bucket.
- **A `tower`/middleware rate-limit crate.** Rejected: a per-key keyed limiter with
  our actor identity and the RateLimit headers is ~80 lines of pure, fully-tested
  code; a dependency would still need the same glue and the `cargo deny` vetting for
  less control.
- **Distributed limiter (shared Redis bucket).** Deferred: meaningless on a single
  node and a new infra dependency; it belongs with the distributed-mode ADR (#17).
- **Per-key configured limits now.** Deferred: a global default is the common need;
  per-key overrides can ride the `ApiKey` struct later behind the same limiter.

## Implementation

One PR: the `rate_limit` module (pure `TokenBucket` + `RateLimiter`), a
`[rate_limit]` config block with env overrides and validation, the REST middleware
headers + 429, the gRPC `authenticate_limited` returning `ResourceExhausted`,
`.env.example`, and the docs (`docs/api/rest-grpc.md` limits section +
self-hosting config reference).

## Verification

- `TokenBucket` unit tests with an injected clock: burst is allowed, the N+1th
  request in a burst is refused, tokens refill at the configured rate, the
  `Retry-After`/reset is correct, and a disabled limiter always admits.
- A REST integration test drives a burst past the limit and asserts the 429, the
  `Retry-After`/`RateLimit-*` headers, and that waiting restores capacity; a gRPC
  test asserts `ResourceExhausted` past the limit.
- Opt-in and in-memory, so there is no on-disk change and the `kill -9` crash gate
  is untouched.
