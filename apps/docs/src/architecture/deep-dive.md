Quiver is a Cargo workspace: a from-scratch storage engine, index structures, SIMD
distance kernels, and a query planner, with a thin gRPC/REST shell and a TUI client.
The C4 views —
[system context](https://github.com/achref-soua/quiver/blob/main/docs/architecture/c4-context.md)
and
[container view](https://github.com/achref-soua/quiver/blob/main/docs/architecture/c4-container.md)
— map the system at a glance; the crate map below and the index design that follows
are the deep dive. Every significant decision is captured as an
[ADR](adrs.md).

{{#include ../../../../docs/architecture/overview.md}}

---

{{#include ../../../../docs/index/design.md}}
