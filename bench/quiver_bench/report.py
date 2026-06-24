# SPDX-License-Identifier: AGPL-3.0-only
"""Auto-generate the comparison report from result CSVs.

Usage:
    uv run --project bench python -m quiver_bench.report \\
        docs/benchmarks/results/comparison-v0.17.0

Reads all ``*.csv`` files under the given directory and generates
``comparison-v0.17.0.md`` alongside ``manifest.json``.
"""

from __future__ import annotations

import argparse
import csv
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

RECALL_TARGET = 0.95

# Quiver-only sweep CSVs that are NOT part of the competitor matrix — they get
# their own report sections (the memory wedge and the filtered-selectivity sweep).
SWEEP_FILES = {"quant_sweep.csv", "filter_sweep.csv"}


def _load_results(directory: Path) -> dict[str, list[dict]]:
    """Return {competitor_name: [row, ...]} from competitor CSVs in *directory*.

    The Quiver-only sweep files are excluded — they would otherwise merge into the
    ``quiver`` competitor rows and pollute the ef/nprobe sweep tables.
    """
    data: dict[str, list[dict]] = {}
    for csv_path in sorted(directory.glob("**/*.csv")):
        if csv_path.name in SWEEP_FILES:
            continue
        rows = []
        with csv_path.open() as f:
            rows = list(csv.DictReader(f))
        if rows:
            name = rows[0].get("competitor", csv_path.stem)
            data.setdefault(name, []).extend(rows)
    return data


def _read_csv(path: Path) -> list[dict]:
    """Read one CSV into a list of row dicts ([] if absent)."""
    if not path.exists():
        return []
    with path.open() as f:
        return list(csv.DictReader(f))


def _memory_wedge_section(ds_dir: Path) -> list[str]:
    """Render the quantization memory-wedge table from ``quant_sweep.csv``."""
    rows = _read_csv(ds_dir / "quant_sweep.csv")
    if not rows:
        return []
    # Best operating point per config (highest recall@10).
    by_cfg: dict[str, dict] = {}
    for r in rows:
        cfg = r.get("notes") or "?"
        try:
            recall = float(r.get("recall_at_10") or 0)
        except ValueError:
            recall = 0.0
        if cfg not in by_cfg or recall > float(by_cfg[cfg].get("recall_at_10") or 0):
            by_cfg[cfg] = r
    out = [
        "### Memory wedge — quantization tradeoff (Quiver)",
        "",
        "> Same dataset, best operating point per index/quantization config, **each built in its own "
        "fresh server process**. The recall/build/throughput tradeoff is what is published here: the "
        "disk-resident Vamana graph (PQ codes in RAM, full vectors on SSD) holds recall@10 close to "
        "exact in-memory HNSW while PQ trades the *deep* tail — note recall@100 falls off. The "
        "**absolute serving-RAM wedge is `[reference-hardware-pending]`**: post-build RSS on this box "
        "reflects the build's allocator high-water mark (the disk-Vamana build pages in every vector "
        "to construct the graph, and the allocator keeps those pages), not the cold-reload serving "
        "footprint where only PQ codes stay resident — so RSS is deliberately omitted rather than "
        "shown misleadingly. IVF+PQ is also omitted: its default parameters were mistuned on this run "
        "(slow build, poor recall), so a fair IVF point is reference-hardware-pending too.",
        "",
        "| Config | recall@1 | recall@10 | recall@100 | Build (s) | QPS (1T) | ef |",
        "|---|---|---|---|---|---|---|",
    ]
    # Stable wedge order: exact graph → IVF+PQ → disk graph.
    order = {"hnsw": 0, "ivf": 1, "disk_vamana": 2}
    for cfg in sorted(by_cfg, key=lambda c: order.get(c.split("+")[0], 9)):
        r = by_cfg[cfg]
        out.append(
            f"| {cfg} "
            f"| {_fmt_or(r.get('recall_at_1'), '.4f')} "
            f"| {_fmt_or(r.get('recall_at_10'), '.4f')} "
            f"| {_fmt_or(r.get('recall_at_100'), '.4f')} "
            f"| {_fmt_or(r.get('build_s'), '.1f')} "
            f"| {_fmt_or(r.get('qps_1t'), '.0f')} "
            f"| {r.get('param_value', '?')} |"
        )
    return out + [""]


def _filter_sweep_section(ds_dir: Path) -> list[str]:
    """Render the filtered-selectivity sweep table from ``filter_sweep.csv``."""
    rows = _read_csv(ds_dir / "filter_sweep.csv")
    if not rows:
        return []
    out = [
        "### Filtered-selectivity sweep (Quiver)",
        "",
        "> Recall and throughput as a payload pre-filter (`bucket < s`) keeps `s`% of the "
        "collection. Recall is measured against the *filtered* exact ground truth (brute force "
        "over the matching subset), so it reflects correctness under filtering, not the unfiltered "
        "neighbours. The selectivity planner crosses over between regimes: very selective filters "
        "pre-filter to an exact scan (recall ≈ 1.0, but the scan to find the subset is the latency "
        "cost), while looser filters post-filter an ANN result — which has a recall valley at "
        "mid-selectivity before recovering as more candidates survive the filter.",
        "",
        "| Selectivity | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) |",
        "|---|---|---|---|---|---|",
    ]
    for r in sorted(rows, key=lambda r: int(r.get("param_value") or 0)):
        out.append(
            f"| {r.get('param_value', '?')}% "
            f"| {_fmt_or(r.get('recall_at_10'), '.4f')} "
            f"| {_fmt_or(r.get('qps_1t'), '.0f')} "
            f"| {_fmt_or(r.get('p50_ms'), '.2f')} "
            f"| {_fmt_or(r.get('p95_ms'), '.2f')} "
            f"| {_fmt_or(r.get('p99_ms'), '.2f')} |"
        )
    return out + [""]


def _best_at_recall(rows: list[dict], target: float) -> dict | None:
    """Return the row closest to (and ≥) *target* recall@10, or the best available."""
    typed = []
    for r in rows:
        try:
            typed.append({**r, "_r": float(r.get("recall_at_10") or 0)})
        except ValueError:
            pass
    if not typed:
        return None
    # Rows meeting the target
    meeting = [r for r in typed if r["_r"] >= target]
    if meeting:
        # Lowest recall that meets the target (most efficient operating point)
        return min(meeting, key=lambda r: r["_r"])
    # Fall back to the best available
    return max(typed, key=lambda r: r["_r"])


def _fmt_or(val: str | None, fmt: str = ".1f") -> str:
    if val is None or val == "" or val == "None":
        return "—"
    try:
        return format(float(val), fmt)
    except (ValueError, TypeError):
        return str(val)


def _wins_matrix(data: dict[str, dict | None]) -> str:
    """Build a wins/ties/losses matrix: Quiver vs each other competitor."""
    quiver = data.get("quiver")
    if quiver is None:
        return "_Quiver results not available — matrix requires a Quiver run._\n"

    metrics = {
        "recall@10": ("recall_at_10", True, ".4f"),   # higher is better
        "QPS (1T)": ("qps_1t", True, ".0f"),          # higher is better
        "RSS (MB)": ("rss_mb", False, ".0f"),          # lower is better
        "Build (s)": ("build_s", False, ".1f"),        # lower is better
    }

    lines = ["| Metric | vs competitor | Quiver | Competitor | Verdict |", "|---|---|---|---|---|"]
    for label, (field, higher_wins, fmt) in metrics.items():
        for comp_name, comp_row in data.items():
            if comp_name == "quiver":
                continue
            try:
                q_val = float(quiver.get(field) or 0)
                c_val = float((comp_row or {}).get(field) or 0) if comp_row else None
            except (ValueError, TypeError):
                continue
            if c_val is None or c_val == 0:
                verdict = "n/a"
            elif higher_wins:
                if q_val > c_val * 1.02:
                    verdict = "✅ win"
                elif q_val < c_val * 0.98:
                    verdict = "❌ loss"
                else:
                    verdict = "≈ tie"
            else:
                if q_val < c_val * 0.98:
                    verdict = "✅ win"
                elif q_val > c_val * 1.02:
                    verdict = "❌ loss"
                else:
                    verdict = "≈ tie"

            lines.append(
                f"| {label} | {comp_name} "
                f"| {format(q_val, fmt)} "
                f"| {format(c_val, fmt) if c_val else '—'} "
                f"| {verdict} |"
            )
    return "\n".join(lines) + "\n"


def generate(result_dir: Path) -> str:
    """Generate and return the full report as a Markdown string."""
    # Load manifest
    manifest_path = result_dir / "manifest.json"
    hw: dict = {}
    if manifest_path.exists():
        hw = json.loads(manifest_path.read_text())

    # Load all CSVs from sub-directories
    dataset_dirs: list[tuple[str, Path]] = []
    for sub in sorted(result_dir.iterdir()):
        if sub.is_dir() and any(sub.glob("*.csv")):
            dataset_dirs.append((sub.name, sub))

    lines: list[str] = []

    # Version label derived from the result directory (e.g. comparison-v0.18.0).
    version = result_dir.name.replace("comparison-", "") or "dev"

    # Header
    lines += [
        f"# Quiver {version} — Multi-DB Benchmark Comparison",
        "",
        f"_Generated: {datetime.now(tz=timezone.utc).strftime('%Y-%m-%d %H:%M UTC')}_",
        "",
        "> **Methodology:** [docs/benchmarks/methodology.md](../methodology.md) · "
        "[ADR-0037](../../adr/0037-scientific-multi-db-benchmark-suite.md)",
        "",
        "> **Honesty note:** Every number below is real and measured. Where Quiver wins, "
        "numbers are shown; where it loses or ties, that is stated plainly. "
        "`[reference-hardware-pending]` marks figures that require reproduction on "
        "dedicated, otherwise-idle hardware to carry weight as official headlines.",
        "",
    ]

    # Hardware block
    if hw:
        lines += [
            "## Hardware manifest",
            "",
            "| | |",
            "|---|---|",
            f"| OS | {hw.get('os', '?')} {hw.get('os_release', '')} |",
            f"| Processor | {hw.get('processor', '?')} |",
            f"| Logical CPUs | {hw.get('cpu_count_logical', '?')} |",
            f"| RAM total | {int((hw.get('ram_total_mb') or 0) / 1024)} GB |",
            f"| Rust | {hw.get('rust_version', '?')} |",
            f"| Docker | {hw.get('docker_version', '?')} |",
            f"| Python | {hw.get('python', '?')} |",
            "",
            f"> {hw.get('note', '')}",
            "",
        ]

    # Per-dataset sections
    if not dataset_dirs:
        lines.append("_No result CSVs found. Run `just bench-compare` first._\n")
    else:
        # Union of every competitor seen across all datasets, so a system that
        # ran on one dataset but is missing on another (e.g. an OOM/DNF that left
        # no CSV) is reported honestly rather than silently dropped.
        all_comps: set[str] = set()
        for _, d in dataset_dirs:
            all_comps |= set(_load_results(d).keys())

        for ds_name, ds_dir in dataset_dirs:
            data_by_comp = _load_results(ds_dir)
            best_rows: dict[str, dict | None] = {
                name: _best_at_recall(rows, RECALL_TARGET)
                for name, rows in data_by_comp.items()
            }

            is_smoke = ds_name == "smoke"
            label = "SIFTSMALL (10k, 128-d, L2) — smoke validation" if is_smoke else ds_name.upper()
            pending = " `[reference-hardware-pending]`" if not is_smoke else ""

            lines += [
                f"## Dataset: {label}{pending}",
                "",
            ]

            if is_smoke:
                lines += [
                    "> **Purpose:** Validates every competitor adapter end-to-end on 10k vectors. "
                    "QPS and RSS on 10k vectors are not representative of production scale.",
                    "",
                ]

            # Comparison table at recall ≥ 0.95 (or best available)
            lines += [
                f"### Operating point: recall@10 ≥ {RECALL_TARGET} (or best achieved)",
                "",
                "| Competitor | Version | recall@1 | recall@10 | recall@100 | QPS (1T) | QPS (NT) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |",
                "|---|---|---|---|---|---|---|---|---|---|---|---|",
            ]
            for comp_name, row in sorted(best_rows.items()):
                if row is None:
                    lines.append(f"| {comp_name} | — | error | — | — | — | — | — | — | — | — | failed |")
                    continue
                # Find the adapter version from the CSV (not stored — use a known map)
                versions = {
                    "quiver": "v0.22.0-dev",
                    "faiss": "1.14.3",
                    "lancedb": "0.33.0",
                    "chroma": "1.5.9",
                    "milvus_lite": "3.0.0",
                    "milvus_server": "v2.5.4 (server)",
                    "qdrant": "1.13.4",
                    "pgvector": "0.7/pg16",
                    "weaviate": "1.27.0",
                }
                ver = versions.get(comp_name, "?")
                # Comparative numbers on the identical box are real (R6); only
                # absolute RSS and the 10M disk path are VM-distorted (R5).
                note = "smoke only" if is_smoke else "dev-box · indicative"
                param = f"{row.get('param_name','ef')}={row.get('param_value','?')}"
                lines.append(
                    f"| {comp_name} | {ver} "
                    f"| {_fmt_or(row.get('recall_at_1'), '.4f')} "
                    f"| {_fmt_or(row.get('recall_at_10'), '.4f')} "
                    f"| {_fmt_or(row.get('recall_at_100'), '.4f')} "
                    f"| {_fmt_or(row.get('qps_1t'), '.0f')} "
                    f"| {_fmt_or(row.get('qps_nt'), '.0f')} "
                    f"| {_fmt_or(row.get('rss_mb'), '.0f')} "
                    f"| {_fmt_or(row.get('build_s'), '.1f')} "
                    f"| {_fmt_or(row.get('index_disk_mb'), '.1f')} "
                    f"| {param} "
                    f"| {note} |"
                )
            lines += [""]

            # Full sweep tables per competitor
            lines += ["### Full ef/nprobe sweep", ""]
            for comp_name, rows in sorted(data_by_comp.items()):
                lines += [
                    f"<details><summary>{comp_name}</summary>",
                    "",
                    "| ef/nprobe | recall@10 | QPS (1T) | QPS (NT) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |",
                    "|---|---|---|---|---|---|---|---|",
                ]
                for row in rows:
                    param_val = row.get("param_value", "?")
                    lines.append(
                        f"| {param_val} "
                        f"| {_fmt_or(row.get('recall_at_10'), '.4f')} "
                        f"| {_fmt_or(row.get('qps_1t'), '.0f')} "
                        f"| {_fmt_or(row.get('qps_nt'), '.0f')} "
                        f"| {_fmt_or(row.get('p50_ms'), '.2f')} "
                        f"| {_fmt_or(row.get('p95_ms'), '.2f')} "
                        f"| {_fmt_or(row.get('p99_ms'), '.2f')} "
                        f"| {_fmt_or(row.get('rss_mb'), '.0f')} |"
                    )
                lines += ["", "</details>", ""]

            # Quiver-only sweeps: the memory wedge and the filtered-selectivity sweep.
            lines += _memory_wedge_section(ds_dir)
            lines += _filter_sweep_section(ds_dir)

            # Wins / ties / losses matrix
            lines += ["### Wins / ties / losses (Quiver vs field)", ""]
            lines.append(_wins_matrix(best_rows))

            # Did-not-complete note: competitors that ran on another dataset but
            # left no CSV here (an OOM/DNF), so absence is explained, not hidden.
            if not is_smoke:
                missing = sorted(all_comps - set(data_by_comp))
                if missing:
                    lines += [
                        f"> **Did not complete on {ds_name.upper()}:** "
                        f"{', '.join(missing)} — ran on another dataset but failed or "
                        "ran out of memory here on this box (recorded honestly, never estimated).",
                        "",
                    ]

    # Reference-hardware callout. A quiver-only result set (the v0.22.0
    # dimensions) gets a focused footer — the multi-DB note below talks about
    # competitors that did not run here, which would mislead.
    competitors_present = {
        c for _, d in dataset_dirs for c in _load_results(d) if c != "quiver"
    }
    lines += ["---", "", "## How to read these numbers (honesty)", ""]
    if dataset_dirs and not competitors_present:
        lines += [
            "This is a **Quiver-only** result set (the v0.22.0 dimensions, ADR-0061) on a "
            "**resource-shared WSL2 dev box** (specs in the manifest above). The full multi-DB "
            "standings live in the `comparison-v0.20.0` set; here every number is Quiver against its "
            "own exact ground truth, labelled *dev-box · indicative*.",
            "",
            "- **QPS (NT)** is the saturated multi-thread throughput from the concurrent driver "
            "(`--concurrency`) — the showcase for the v0.21.0 concurrent-reads work. Read it honestly: "
            "a single-process Python client (GIL + HTTP round-trip) is itself a concurrency ceiling, so "
            "for *light* queries (low `ef`, sub-2 ms) the client saturates first and NT sits at or "
            "below 1T; the server-side win shows on *heavier* queries (higher `ef`, higher recall), "
            "where NT pulls ahead of 1T.",
            "- **Memory wedge.** The recall/build/throughput tradeoff across index/quantization configs "
            "is real and published; the **absolute serving-RAM** figure is omitted, not estimated — "
            "post-build RSS on this box is the build's allocator high-water mark, not the cold-reload "
            "serving footprint, so it stays `[reference-hardware-pending]`.",
            "- **Build time** is *time-until-queryable* via the bulk-ingest path (`POST …/points:bulk`, "
            "ADR-0045): one WAL fsync per request and a single deferred index pass, with the first "
            "query forcing the rebuild inside the timer.",
            "",
            "What stays pending on dedicated, otherwise-idle reference hardware (runbook "
            "[`§9`](../reference-hardware-runbook.md)): the **full-field saturated QPS** across every "
            "competitor, the **official absolute-RSS table** (and the serving-RAM wedge), and "
            "**Deep10M**. IVF+PQ is omitted from the wedge — its default parameters were mistuned on "
            "this run — so a fair IVF point is reference-hardware-pending too. Never fabricated.",
            "",
        ]
    else:
        lines += [
            "This run is on a **resource-shared WSL2 dev box** (specs in the manifest above). Per the "
            "risk register: comparisons run on the *identical* box under identical conditions are a fair, "
            "real result (R6) — so the **recall, QPS, and latency standings above stand**. Two things a VM "
            "distorts (R5) are **not** to be read as official headlines:",
            "",
            "- **Absolute RSS.** Only the *isolated* systems are comparable: Quiver, Qdrant, Weaviate, and "
            "Milvus **server** report the DB process/container RSS. FAISS, LanceDB, and Chroma run "
            "in-process, so their RSS includes the Python harness **and the resident dataset** (~512 MB "
            "for SIFT1M, ~3.7 GB for GIST1M) — inflated, not directly comparable. These are **in-memory "
            "HNSW** comparisons for every system; Quiver's memory-frugality wedge is its **disk-resident "
            "DiskVamana path** (holds only PQ codes in RAM), measured separately in "
            "[`docs/benchmarks/results/disk-path.md`](./disk-path.md) — not these tables.",
            "- **Build time.** As of v0.20.0 Quiver's build uses the **bulk-ingest** path "
            "(`POST …/points:bulk`, ADR-0045): one WAL fsync per request and a single deferred index "
            "pass, with the first query forcing the rebuild so the reported number is the honest "
            "*time-until-queryable* (the same thing every competitor's build column measures). This "
            "replaces the v0.18.0 REST-upload path (1M points in 500-point POSTs, each doing incremental "
            "index maintenance) — compare the two `comparison-*` result sets for the improvement. "
            "In-process libraries (FAISS) still build fastest because they skip the network and "
            "serialization entirely.",
            "",
            "The **SIFT1M and GIST1M comparative standings above are dev-box but real** (R6 — identical "
            "box, identical conditions). The **QPS (NT)** column is the saturated multi-thread throughput "
            "from the concurrent driver (`--concurrency`); it is populated where a run drove more than one "
            "client thread and is the showcase for the v0.21.0 concurrent-reads work. Read it honestly: a "
            "single-process Python client (GIL + HTTP round-trip) is itself a concurrency ceiling, so for "
            "*light* queries (low `ef`, sub-2 ms) the client saturates first and NT can sit at or below 1T; "
            "the server-side concurrent-reads win shows on *heavier* queries (higher `ef`, higher recall), "
            "where NT pulls ahead of 1T. What stays pending on "
            "dedicated, otherwise-idle reference hardware (runbook "
            "[`§9`](../reference-hardware-runbook.md)): the **official absolute-RSS table**, the "
            "**full-field saturated QPS** across every competitor, and **Deep10M** (the disk-path memory "
            "headline). Milvus is benchmarked as the **server** (Docker), not the in-process Lite build, "
            "which is not performance-representative.",
            "",
        ]

    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="quiver_bench.report")
    p.add_argument("result_dir", type=Path, nargs="?",
                   default=Path("docs/benchmarks/results/comparison-v0.18.0"))
    args = p.parse_args(sys.argv[1:] if argv is None else argv)

    if not args.result_dir.exists():
        print(f"ERROR: {args.result_dir} does not exist. Run `just bench-compare` first.", file=sys.stderr)
        return 1

    report = generate(args.result_dir)
    # The report filename mirrors the result directory (e.g. comparison-v0.18.0.md).
    out = args.result_dir / f"{args.result_dir.name}.md"
    out.write_text(report)
    print(f"Report written to {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
