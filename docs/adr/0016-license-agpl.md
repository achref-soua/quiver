# ADR-0016: License — AGPL-3.0

- **Status:** Accepted
- **Date:** 2026-06-13
- **Deciders:** Achref Soua

## Context

Quiver is open-source and self-hostable, but we want hosted/SaaS forks to contribute improvements back rather than building closed value on top of the project without reciprocity. We must pick a license that reflects "open and self-hostable, but protected."

## Decision

License the project under **AGPL-3.0-only**. The repository root carries the full license text in `LICENSE`; source files reference it via SPDX headers (`// SPDX-License-Identifier: AGPL-3.0-only`) added during Phase 1 scaffolding.

## Consequences

- **+** The network-use copyleft clause closes the "SaaS loophole": anyone offering Quiver as a network service must make their modified source available. This aligns openness with sustainability.
- **−** Some organizations prohibit AGPL dependencies. Mitigations: (a) clean module boundaries so a separate commercial license could be offered later without re-architecture; (b) the **client SDKs and wire-protocol definitions** are candidates for a more permissive license (Apache-2.0/MIT) so client code can be embedded freely — this is flagged for an explicit decision when the SDKs ship (Phase 2), since AGPL on a client library would deter adoption.
- **Contribution terms:** no CLA initially (minimizes friction). A DCO sign-off requirement may be added; recorded here if adopted.

## Alternatives considered

- **Apache-2.0 / MIT** — permissive; rejected for the core because they allow closed SaaS forks with no reciprocity (still under consideration for the SDKs).
- **BSL / SSPL** — source-available but not OSI-approved open source; rejected because we want genuine OSS.
- **GPL-3.0 (non-Affero)** — does not trigger on network use, leaving the SaaS loophole open.
