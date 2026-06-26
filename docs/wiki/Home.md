# Quiver Wiki

**Quiver** is a security-first, memory-frugal **vector database** written in Rust,
with a retro terminal cockpit. Self-hostable, encrypted by default, single-node by
design (optionally clustered).

This wiki is a quick orientation and a hub. The **authoritative, versioned docs
live in the repository** so they never drift from the code — this page just points
you to them.

## Start here

- **[README](https://github.com/achref-soua/quiver#readme)** — overview, quickstart, and feature highlights.
- **["Quiver, Explained" field guide](https://github.com/achref-soua/quiver/blob/main/docs/quiver-explained.pdf)** — a 60-page, beginner-to-expert PDF: embeddings and ANN from first principles, the engine block by block, durability, the security model, and honest benchmarks.
- **Quickstart:** clone, then `cargo run -p quiverdb-cli -- demo` to seed a demo database, start the server, and open the cockpit — zero config.

## Documentation

The docs site (mdBook) lives under [`apps/docs/`](https://github.com/achref-soua/quiver/tree/main/apps/docs) — build it locally with `mdbook build apps/docs`. It covers:

- **Concepts**, **Quickstart**, **Self-hosting & configuration**, **Kubernetes & Helm**
- **Guides:** RAG, agentic patterns (MCP), tuning
- **Features:** indexing & memory frugality, concurrency, hybrid search, server-side embedding, multi-vector, encrypted search, migration importers, replication, snapshots, observability
- **API & SDKs:** CLI reference, REST & gRPC, MCP server, Python/TypeScript/Go SDKs
- **Security** and **Architecture**

Quick links: the **REST API** is specified as [OpenAPI 3.1](https://github.com/achref-soua/quiver/tree/main/docs/api); the **CLI** and **every `QUIVER_*` setting** are documented in the docs site's reference pages.

## Security

Security is Quiver's foundation, stated honestly:

- **[Threat model](https://github.com/achref-soua/quiver/blob/main/docs/security/threat-model.md)** — what is defended, against whom, and what is *not* protected.
- **Security audit** — a static OWASP-style review plus a dynamic **OWASP ZAP** scan and the fuzzers re-run, with every finding fixed and regression-tested (see `docs/security/`).
- **[Report a vulnerability](https://github.com/achref-soua/quiver/blob/main/SECURITY.md)** — please use private reporting, not a public issue.

## Project

- **[Roadmap & phases](https://github.com/achref-soua/quiver/blob/main/docs/roadmap.md)**
- **[Architecture decision records (ADRs)](https://github.com/achref-soua/quiver/tree/main/docs/adr)**
- **[Contributing](https://github.com/achref-soua/quiver/blob/main/CONTRIBUTING.md)** · **[Support](https://github.com/achref-soua/quiver/blob/main/SUPPORT.md)** · **[Code of Conduct](https://github.com/achref-soua/quiver/blob/main/CODE_OF_CONDUCT.md)**
- **Questions:** [Discussions](https://github.com/achref-soua/quiver/discussions) · **Bugs:** [Issues](https://github.com/achref-soua/quiver/issues)

See also the **[FAQ](FAQ)**.
