# Reference-Hardware Benchmark Runbook

This is the procedure to produce Quiver's **published** disk-path numbers — the
recall@10-vs-RAM headline and the head-to-head against **Qdrant** and
**LanceDB** — on dedicated reference hardware. The day-to-day WSL2 dev box is a
resource-shared VM whose memory and I/O are virtualized, so it is *not* a source
for published throughput/memory figures (recall and byte-footprints are
host-independent and stand regardless). Run this on a **dedicated, otherwise-idle
machine**. See [`methodology.md`](./methodology.md) for the rules; the golden
rule is **measure every system identically and never fabricate** — if Quiver
loses a metric, publish it.

## 0. Document the hardware

Record, and put at the top of the results file:

```
cpu / cores / ram / ssd model / os build / rustc version
```

## 1. Why native, and a note on fairness

Measure all three systems in the **same environment on the same machine**. Two
honest setups:

- **All-native on Windows** — cleanest for Quiver (a native `.exe`) and LanceDB
  (native Python). Qdrant is Linux-first with no official Windows binary, so run
  it under **Docker Desktop** and measure its memory with `docker stats`
  (container working set). Note in the writeup that Qdrant ran containerized.
- **All-native on Linux** (the machine dual-booted or booted from a live USB) —
  the most apples-to-apples three-way comparison, since all three run native and
  RSS is sampled the same way (`/proc/<pid>/status` → `VmRSS`).

Either is valid as long as it is **uniform and documented**. Pin exact versions
of Quiver, Qdrant, and LanceDB, and configure each competitor per **its own**
current recommended settings (record them verbatim).

## 2. Prerequisites (Windows)

- **Rust** — install via <https://rustup.rs> (the MSVC toolchain).
- **Git**.
- **Python 3.12 + uv** — `uv` from <https://docs.astral.sh/uv/>, for LanceDB.
- **Docker Desktop** — for Qdrant.

## 3. Datasets

Fetch into a working folder and pin the SHA-256 of each archive.

- **SIFT1M** (128-d, 1 M base, 10 k queries, L2) — the standard TEXMEX corpus
  (`sift.tar.gz`); gives `sift_base.fvecs`, `sift_query.fvecs`,
  `sift_groundtruth.ivecs`.
- **Deep10M** (96-d, 10 M, L2) — the memory headline; a 10 M slice of Deep1B.
  The [big-ann-benchmarks](https://github.com/harsha-simhadri/big-ann-benchmarks)
  dataset fetchers produce `.fvecs`/`.ivecs` (or convert to that layout).

> The `.fvecs`/`.ivecs` layout is a little-endian `int32` length per row followed
> by that many `float32`/`int32`. Quiver's reader and the snippets below assume it.

## 4. Quiver

```powershell
git clone https://github.com/achref-soua/quiver ; cd quiver

# Build the encrypted disk index (RAM-heavy, one-time).
cargo run --release --example disk_recall -- build C:\data\sift\sift_base.fvecs C:\data\sift.qvx

# Serve it: only the PQ codes are RAM-resident. The process holds at the end so
# you can sample its resident set.
cargo run --release --example disk_recall -- serve C:\data\sift.qvx `
  C:\data\sift\sift_query.fvecs C:\data\sift\sift_groundtruth.ivecs
```

The `build` step prints the **on-disk index size** and the **RAM-resident
PQ-code footprint vs full-precision**. The `serve` step prints **recall@10** and
QPS per `l_search`, then waits. While it waits, sample its resident set in a
second PowerShell:

```powershell
(Get-Process disk_recall).WorkingSet64 / 1MB   # steady-state RSS in MB
```

Record the RSS at the `l_search` that hits your chosen recall operating point
(see §7). The end-to-end server path (`quiver serve` + the `bench/` harness)
additionally reports p50/p95/p99 and is the form used once the disk index is
exposed over REST/gRPC.

## 5. Qdrant (competitor)

```powershell
docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant:<pin-version>
```

Create a collection (HNSW; add `quantization_config` only if you also quantize —
keep the comparison at the same recall point), upsert the SIFT base, then query
with the same 10 k query set and compute recall@10 against `sift_groundtruth`.
Use Qdrant's recommended `hnsw_config` and tune `hnsw_ef` to the operating point.
Memory:

```powershell
docker stats --no-stream    # the qdrant container's memory usage
```

## 6. LanceDB (competitor)

```powershell
uv venv ; uv pip install lancedb pyarrow numpy
```

In Python: read the SIFT base into an Arrow table, `db.create_table(...)`, build
an `IVF_PQ` index (Lance's recommended ANN index), then search the query set and
compute recall@10 vs the ground truth. Tune `nprobes` / refine factor to the
operating point. Sample the Python process's memory at steady state:

```powershell
(Get-Process python).WorkingSet64 / 1MB
```

## 7. Operating point (apples-to-apples)

Pick one target **recall@10** (e.g. `0.95`). For each system, tune its search
knob (`l_search` for Quiver, `hnsw_ef` for Qdrant, `nprobes`/refine for LanceDB)
to land at that recall, **then** compare:

- **RSS** (steady state, after index load + a warmup pass) — the headline,
- **QPS** (single-thread and saturated),
- **on-disk index size** and **build time**.

Sample RSS identically for all three (the commands above), after a warmup query
pass, with nothing else running.

## 8. Record the results

Fill in [`results/disk-path.md`](./results/disk-path.md) and the README
benchmark table with the measured numbers, and commit alongside:

- the exact versions of Quiver / Qdrant / LanceDB,
- each system's config (Quiver `IndexSpec` / `pq_subspaces`; Qdrant collection
  config; LanceDB index params),
- the raw CSVs and the hardware block from §0.

State plainly which metric each system wins. That honesty — real numbers,
reproducible, losses included — is the point of this whole document.

---

## 9. Multi-DB comparison harness (ADR-0037)

The full comparison harness (`bench/quiver_bench/comparison.py`) extends the
single-DB harness and drives all seven competitors via a uniform interface.
Prerequisites: Docker (for Qdrant, pgvector, Weaviate), plus the Python
competitor clients installed in the bench venv:

```bash
# From the quiver repo root on the reference machine:
/path/to/quiver/bench/.venv/bin/python -m pip install \
    faiss-cpu lancedb pyarrow chromadb pymilvus milvus-lite \
    qdrant-client psycopg2-binary weaviate-client
```

Pull the pinned competitor images:

```bash
docker pull qdrant/qdrant:v1.13.4
docker pull pgvector/pgvector:pg16
docker pull cr.weaviate.io/semitechnologies/weaviate:1.27.0
```

Start a Quiver server on the reference machine:

```bash
QUIVER_DATA_DIR=/tmp/quiver-ref QUIVER_API_KEY=ref-bench-key QUIVER_INSECURE=true \
    ./target/release/quiver serve &
```

Then run the full comparison:

```bash
# In-process competitors (FAISS, LanceDB, Chroma, Milvus Lite) + Docker ones:
just bench-compare --dataset sift1m --competitors all \
    --quiver-url http://127.0.0.1:6333 --quiver-key ref-bench-key \
    --out /path/to/results

# Deep10M (the memory headline — 10M vectors):
just bench-compare --dataset deep10m --competitors faiss,lancedb,qdrant,quiver \
    --quiver-url http://127.0.0.1:6333 --quiver-key ref-bench-key \
    --out /path/to/results
```

Generate the committed report:

```bash
just bench-report
# Output: docs/benchmarks/results/comparison-v0.17.0/comparison-v0.17.0.md
```

Commit the CSVs, manifest.json, and the .md report to the repo. Mark every
figure with the reference-hardware block from §0. Any figure not produced on
dedicated, idle reference hardware **must** carry a `[dev-box]` or
`[reference-hardware-pending]` label in the report — never remove these labels
to make numbers look more authoritative.
