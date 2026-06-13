# ADR-0003: Serialization formats

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Quiver serializes data at several boundaries with different requirements: the gRPC wire (interop), the REST wire (interop, human-debuggable), on-disk WAL/manifest framing (stable, versioned, corruption-detectable), structured metadata (compact), and user payloads (queryable, transparent). One format does not fit all.

## Decision

- **gRPC wire:** Protocol Buffers via `prost`/`tonic` — the schema source of truth (ADR-0018).
- **REST wire:** JSON (serde) with an OpenAPI 3.1 contract.
- **On-disk framing** (WAL records, page headers, manifest structure): an **explicit, hand-specified, versioned binary layout** with little-endian fields, length prefixes, and CRC32C — *not* a derive-driven format, so field reordering or library changes can never silently change the on-disk bytes. Specified in [`../storage/on-disk-format.md`](../storage/on-disk-format.md).
- **Structured metadata contents** (collection descriptors, manifest entries): `postcard` (compact, deterministic, `no_std`-friendly) embedded inside the explicitly-framed, format-versioned envelope above.
- **User payloads:** validated UTF-8 **JSON** in Phase 1 (transparent, debuggable, directly filterable); a binary encoding (e.g. CBOR) is a later optimization once the schema/filters stabilize.

## Consequences

- **+** Each boundary gets the right tradeoff: interop where it matters, an explicit stable layout where corruption-detection and forward-compat matter, compactness for metadata.
- **+** The on-disk format is reviewable byte-for-byte and versioned independently of product SemVer.
- **−** More than one serialization library in the tree; mitigated by confining each to a clear boundary.

## Alternatives considered

- **serde-derive (`bincode`/`postcard`) for on-disk framing** — rejected as the *outer* format: implicit layout is risky for long-lived files and corruption forensics. Used only for inner metadata contents.
- **JSON everywhere** — rejected: too large/slow for the WAL and metadata hot paths.
- **Protobuf on disk** — workable but couples storage to the gRPC schema and is less direct for fixed-layout pages/WAL.
