# Replication

Quiver supports **asynchronous leader-follower replication** (ADR-0030): one or
more **followers** continuously apply a **leader's** committed operations and
serve reads, lagging the leader by the replication delay. This scales reads and
gives you warm standbys — without consensus, failover, or the complexity of
distributed clustering. It is an **advanced / experimental** feature; single-node
remains the primary, fully-supported topology.

## Topology

- The **leader** is a normal Quiver server. It exposes an admin-scoped
  `Replicate` gRPC stream that yields a logical snapshot of current state
  followed by the live commit tail.
- A **follower** is a server started with `QUIVER_LEADER_URL` pointing at the
  leader's gRPC address. It connects, applies the stream, serves reads, and
  **refuses writes** (a write returns HTTP 403 / gRPC `PermissionDenied`).

## Running a follower

```bash
# Leader — a normal server
QUIVER_GRPC_ADDR=0.0.0.0:6334 quiver serve

# Follower — a read replica of that leader
QUIVER_LEADER_URL=http://leader-host:6334 \
QUIVER_LEADER_API_KEY=<an admin key on the leader> \
QUIVER_GRPC_ADDR=0.0.0.0:7334 quiver serve
```

The follower first bootstraps a full snapshot, then streams the live tail. Point
your read traffic at followers and your writes at the leader.

## Guarantees and limits (honest)

- **Asynchronous / eventually consistent.** A follower lags the leader; reads can
  be stale, and there is **no read-your-writes** across nodes.
- **No failover, no consensus.** If the leader fails, promoting a follower is a
  manual operator decision — Quiver does not elect a new leader.
- **Reconnect re-bootstraps.** On a stream error the follower keeps serving its
  last-known (stale) read-only state; restart it to re-sync from a fresh
  snapshot. There is no incremental resume yet.
- **TLS to the leader is a follow-up.** Run replication over a trusted network or
  a tunnel for now; the follower → leader connection is plaintext.
- **Security.** The `Replicate` stream is admin-scoped; a follower authenticates
  with `QUIVER_LEADER_API_KEY`. On-disk and in-transit encryption for client
  traffic are configured independently, as on any node.

See [ADR-0030](adr/0030-leader-follower-replication.md) for the design and the
explicit non-goals.
