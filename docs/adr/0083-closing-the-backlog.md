# ADR-0083: Closing the backlog — what Quiver deliberately will not do

- **Status:** Accepted
- **Date:** 2026-07-27
- **Deciders:** Achref Soua
- **Scope:** every deferred item outstanding at `v1.0.0`, resolved individually

## Context

`v1.1.0` is Quiver's **final planned release**. The project is being closed —
finished, not abandoned. Those are different states, and the difference is entirely
in the paperwork: a finished project has written down what it chose not to build and
why; an abandoned one leaves the reader to guess whether a gap is a decision, an
oversight, or work that stopped mid-sentence.

Over eighty-two ADRs, Quiver accumulated deferred items. Each was deferred honestly
at the time, with a stated reason and usually a sketched upgrade path. But "deferred"
is a promise about the future, and this project has no more future releases in which
to keep it. Leaving them as deferrals would quietly convert a set of considered
decisions into a set of broken promises.

So each one is resolved here, individually, with its reasoning. Where an item was
delivered, it says so and points at the release. Where it was not, it says *why not*
— and "we chose not to, because X" is a complete answer. Silence is not.

Two rules govern every judgement below:

1. **A durable-format change is disqualifying in a closing release.** A format break
   needs a version gate, an upgrade path, and — most of all — a follow-up release to
   fix what the first attempt got wrong on real data. None of those exist after this
   one. The cost of getting a format wrong is paid by users, permanently.
2. **An unmeasured optimisation is unmeasured forever.** The usual justification for
   landing a plausible improvement — that the next release will refine it once
   someone runs it in anger — is not available.

## Decision

### Delivered in `v1.1.0`

| Item | Source | Outcome |
| --- | --- | --- |
| GPU build wiring (G1, G2, G4) | [ADR-0079](0079-gpu-build-search-wiring.md) | **Delivered.** The seam plus the k-means Lloyd and IVF build assign passes. Measured 4.08× on the assign kernel at 1M points, **2.40× on an end-to-end IVF build**. See [ADR-0082](0082-gpu-seam-and-dispatch-policy.md). |

### Retired: durable-format changes, disqualified by rule 1

**Single-log-entry Raft batches (`D = Vec<WalOp>`).** Listed as the leading
alternative in [ADR-0077](0077-batched-raft-proposals.md), where pipelining shipped
instead. It would turn `N` ops into one append, one `fsync`, one commit and one apply,
and would make a bulk write atomic — a genuine win, and the one that would also show
on the single-box `fsync`-bound harness where pipelining measured only ~1.1×.

**Retired** because it is a change to the *durable Raft log format*. Existing logs
would need a fail-loud version gate to avoid silently dropping a tail on upgrade, and
the semantics of a bulk write would shift to all-or-nothing. On a real multi-node
cluster — where RTT dominates `fsync` — most of its win is already collected by the
pipelining that did ship. A format break, in a final release, for a win that is
largely redundant where it matters, is the wrong trade.

**AES-256-GCM as an alternative AEAD.** The per-collection codec cache turned out to
be a non-item (the codec and HKDF extract are already cached; the residual is ~0.4% of
a seal), which left the AEAD cipher itself as the real lever — it is essentially the
whole cost of a seal. On this CPU the headroom is **substantial and should not be
understated**: `openssl speed` at an 8 KiB block measures AES-256-GCM at
**5,209,350 k/s against ChaCha20-Poly1305's 1,958,010 k/s — about 2.7×** — because the
i7-12700H has AES-NI and Quiver's XChaCha20-Poly1305 does not use it.

**Retired anyway**, and the size of the foregone win is the reason to be explicit
rather than a reason to reach for it. Selecting a cipher per-seal changes the at-rest
format (the nonce widths differ, 24 bytes against 12, and every sealed page would have
to record which cipher wrote it). That is rule 1 applied to the most unforgiving
surface in the system: a mistake in the at-rest format of an encrypted database is
not a performance regression, it is unreadable data. Quiver's stated posture is that
it uses only audited primitives and takes no chances with the at-rest format, and
"we left 2.7× on the table in the crypto path" is a much better final state than
"we changed the encryption format in the release after which nobody was watching."

*(The comparison is OpenSSL's implementations, not RustCrypto's, and is therefore
indicative of the hardware's capability rather than a measurement of Quiver. It is
reported that way deliberately.)*

**Per-segment bloom filter for random ids.** [ADR-0076](0076-resident-id-range-fingerprint.md)
named it as the upgrade that would prune the existence probe for *random* (unsortable)
ids, the case the resident id-range fingerprint cannot help — sortable ids already got
~6.1× at 1M.

**Retired** on both rules at once, as ADR-0076 itself anticipated: at ~8 bits per point
a bloom adds roughly 100 MiB of resident state at 100M, which is the exact term
[ADR-0072](0072-on-disk-primary-index.md) and [ADR-0073](0073-on-disk-embed-id-map.md)
spent three releases removing, **and** it requires an on-disk format change to persist
the filters. Trading the memory wedge back, via a format break, in the final release,
for the non-default id shape, is three strikes.

### Retired: too large to land well in a closing release

**Fully off-lock background compaction.** [ADR-0068](0068-streaming-bounded-compaction.md)
describes it as the documented next step and is candid that it is XL: it threads the
plan/build/commit seam across the core/server boundary. The RAM wall it was really
about was closed by the bounded streaming compaction that shipped in `v0.30.x`; what
remains is a **latency** ceiling during compaction, documented in that ADR and
measurable by anyone who cares. **Retired**: a cross-boundary refactor of the write
path is not something to land in a release nobody will follow up.

**Filtered-selectivity and churn competitor sweeps.**
[ADR-0041](0041-deep-benchmark.md) deferred these explicitly and gave the reason:
they need significant per-adapter work across six competitor systems, and doing them
half-way produces misleading numbers. That reasoning has not changed. **Retired** on
ADR-0041's own terms.

### Retired: the hardware does not exist

**The 10M disk-path head-to-head against Qdrant and LanceDB.** Needs materially more
RAM than the 15,855 MB reference box. It has been "pending" across several releases;
pending forever is not a status. **Retired** — if someone with a larger machine wants
the number, the harness is in `bench/` and the runbook is written.

**The 100M single-box run.** Attempted at `v1.0.0` and **it failed**: 70,400,000 of
100,000,000 vectors in about 2 h 15 m, then SIGKILL from the Linux OOM killer at
~10.3 GiB resident and 71 GB on disk. That result is published in
[scale characterisation](../benchmarks/scale-characterization.md) and it *corrects
this project's own earlier extrapolation*, which had put 100M resident state at "a few
GiB". The hardware has not changed. **The honest failure stands as the result** — it
is not re-attempted, not softened, and no 100M recall, latency or serving-RSS figure
is published anywhere, because none was ever measured.

### Retired: measured, and the measurement said no

**GPU search wiring (G3), and GPU k-means++ seeding.** Both are the "one query against
many rows" shape. Measured on the reference card, the device loses at every size and
is still **2.8× slower than the CPU at a million rows**. Full table and the arithmetic
-intensity explanation in [ADR-0082](0082-gpu-seam-and-dispatch-policy.md). **Retired**
rather than deferred: this is not "not yet", it is "measured, and it does not work
here."

### Retired in place: named ceilings that are already honest

These are documented shortcuts with their upgrade path written at the call site. They
stay exactly as they are; this ADR simply records that leaving them is a decision.

| Where | Ceiling | Why it stays |
| --- | --- | --- |
| `quiver-server/src/rate_limit.rs` | One global mutex over the bucket map | Single-node scope is documented; sharding it is an upgrade path, not a defect ([ADR-0049](0049-per-key-rate-limiting.md)) |
| `quiver-server/src/metrics.rs` | One global mutex on recording | Same shape, same reasoning |
| `quiver-server/src/grpc.rs` | `UpsertStream` buffers the whole stream before building | Bounded by `max_bulk_batch_size`, so it cannot OOM the node; chunked flush is the written upgrade path |
| `quiver-embed/src/lib.rs` | Streaming rebuild is IVF-only | Disk-Vamana has no streaming build primitive; scoped deliberately to the case that hits the RAM wall ([ADR-0070](0070-streaming-index-build.md)) |

### Not retired: things that remain true

The **benchmark tables** stay as they are, each labelled with the release it was
measured on. The GIST1M table is the `v0.20.0` run and says so in the README; a stale
number that is labelled stale is honest, and relabelling it without re-measuring would
not be.

## Consequences

- **+** Every deferred item in the repository now has a written resolution. A reader
  in two years can tell what Quiver does, what it deliberately never did, and why —
  without reading eighty-two ADRs or guessing.
- **+** Two of the retirements (the Raft log format, the AEAD cipher) are refusals to
  change a durable format at the last possible moment. That is the conservative call
  and it is the right one for a database.
- **−** Real performance is left on the table, most visibly the ~2.7× AES-NI headroom
  in the seal path. Quantified above rather than glossed, so a forker knows exactly
  what the opportunity is and why it was not taken.
- **−** Anyone hoping the deferred items were a roadmap now learns they were not.
  Better learned from this document than from a stale issue tracker.

## Alternatives considered

- **Leave the deferrals as they are and let the archive speak for itself.** Rejected:
  it converts considered decisions into apparent abandonment, which is precisely the
  impression a finished project should not leave.
- **Ship the cheap ones anyway (the AEAD cipher especially) since the win is large.**
  Rejected on rule 1. The win being large is what makes shipping it tempting; the
  format being at-rest and encrypted is what makes it unforgivable to get wrong with
  no release left to fix it in.
- **Open GitHub issues for each retirement instead of an ADR.** Rejected: issues on an
  archived repository are frozen and invisible. The decision record is in the
  repository, versioned with the code it describes.
