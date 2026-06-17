# Quickstart

Pre-built binaries and a container image are on the roadmap; today you build from
source. The whole loop — clone, build, run, first query — takes a few minutes.

## Prerequisites

- [`rustup`](https://rustup.rs) with the **stable** toolchain
- [`just`](https://github.com/casey/just) (`cargo install just`)
- [`uv`](https://github.com/astral-sh/uv) (for the demo seed script and the Python SDK)

## Clone and run the demo

```bash
git clone https://github.com/achref-soua/quiver
cd quiver
just demo             # build, start an encrypted server, seed a demo collection
```

`just demo` brings up a server with **encryption-at-rest on**, seeds a small
collection through the Python SDK, and prints how to open the cockpit. Then, in
another terminal:

```bash
quiver tui --api-key quiver-demo-key   # the retro cockpit
```

In the cockpit, press `v` (or `enter`) on a collection to open the **constellation
view** — a 2-D random-projection scatter of its vector space with the query's
nearest neighbour highlighted; move the cursor and press `enter` to re-query around
any point.

## Install the CLI

```bash
# install the `quiver` binary from the cloned repo:
cargo install --path crates/quiver-cli
quiver serve                   # gRPC + REST, encrypted by default
quiver tui                     # the cockpit
quiver mcp                     # MCP server (stdio) for AI agents
```

> **Heads-up:** the `quiver-cli` crate currently on crates.io is an unrelated
> third-party project — install from this repository (above), not with
> `cargo install quiver-cli`.

## Your first query (Python)

```python
from quiver import Client, Point

with Client("http://127.0.0.1:6333", api_key="…") as q:
    q.create_collection("items", dim=3, metric="cosine")
    q.upsert("items", [Point("a", [0.1, 0.2, 0.3], {"tag": "x"})])
    hits = q.search("items", [0.1, 0.2, 0.3], k=5)
    print(hits)
```

The same flow is available over [REST & gRPC](api/rest-grpc.md), the
[MCP server](api/mcp.md), and the [TypeScript SDK](api/sdks.md).

## Build, test, and the gate

```bash
just build            # compile the workspace
just verify           # the full local quality gate (lint · test · doc · deny · audit)
cargo run -p quiver-cli -- --help
```

`just verify` is the authoritative gate (the CI workflows are manual-only by
design). Next: [Self-hosting & configuration](self-hosting.md).
