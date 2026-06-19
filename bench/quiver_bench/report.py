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


def _load_results(directory: Path) -> dict[str, list[dict]]:
    """Return {competitor_name: [row, ...]} from CSV files in *directory*."""
    data: dict[str, list[dict]] = {}
    for csv_path in sorted(directory.glob("**/*.csv")):
        rows = []
        with csv_path.open() as f:
            rows = list(csv.DictReader(f))
        if rows:
            name = rows[0].get("competitor", csv_path.stem)
            data.setdefault(name, []).extend(rows)
    return data


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
            f"| | |",
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
                "| Competitor | Version | recall@10 | QPS (1T) | QPS (NT) | RSS (MB) | Build (s) | Index (MB) | ef/nprobe | Notes |",
                "|---|---|---|---|---|---|---|---|---|---|",
            ]
            for comp_name, row in sorted(best_rows.items()):
                if row is None:
                    lines.append(f"| {comp_name} | — | error | — | — | — | — | — | — | failed |")
                    continue
                # Find the adapter version from the CSV (not stored — use a known map)
                versions = {
                    "quiver": "v0.18.0-dev",
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
                    f"| {_fmt_or(row.get('recall_at_10'), '.4f')} "
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
            lines += [f"### Full ef/nprobe sweep", ""]
            for comp_name, rows in sorted(data_by_comp.items()):
                lines += [
                    f"<details><summary>{comp_name}</summary>",
                    "",
                    "| ef/nprobe | recall@10 | QPS (1T) | p50 (ms) | p95 (ms) | p99 (ms) | RSS (MB) |",
                    "|---|---|---|---|---|---|---|",
                ]
                for row in rows:
                    param_val = row.get("param_value", "?")
                    lines.append(
                        f"| {param_val} "
                        f"| {_fmt_or(row.get('recall_at_10'), '.4f')} "
                        f"| {_fmt_or(row.get('qps_1t'), '.0f')} "
                        f"| {_fmt_or(row.get('p50_ms'), '.2f')} "
                        f"| {_fmt_or(row.get('p95_ms'), '.2f')} "
                        f"| {_fmt_or(row.get('p99_ms'), '.2f')} "
                        f"| {_fmt_or(row.get('rss_mb'), '.0f')} |"
                    )
                lines += ["", "</details>", ""]

            # Wins / ties / losses matrix
            lines += ["### Wins / ties / losses (Quiver vs field)", ""]
            lines.append(_wins_matrix(best_rows))

    # Reference-hardware callout
    lines += [
        "---",
        "",
        "## How to read these numbers (honesty)",
        "",
        "This run is on a **resource-shared WSL2 dev box** (specs in the manifest above). Per the "
        "risk register: comparisons run on the *identical* box under identical conditions are a fair, "
        "real result (R6) — so the **recall, QPS, and latency standings above stand**. Two things a VM "
        "distorts (R5) are **not** to be read as official headlines:",
        "",
        "- **Absolute RSS.** Only the *isolated* systems are comparable: Quiver, Qdrant, Weaviate, and "
        "Milvus **server** report the DB process/container RSS. FAISS, LanceDB, and Chroma run "
        "in-process, so their RSS includes the Python harness **and the resident 512 MB dataset** — "
        "inflated, not directly comparable. This SIFT1M table is an **in-memory HNSW** comparison for "
        "every system; Quiver's memory-frugality wedge is its **disk-resident DiskVamana path** "
        "(holds only PQ codes in RAM), measured separately in "
        "[`docs/benchmarks/results/disk-path.md`](./disk-path.md) — not this table.",
        "- **Build time.** Quiver's build is the **REST-upload** path (1M points in batched POSTs); "
        "competitors using in-process or bulk insert are faster. A bulk-ingest endpoint is a known "
        "follow-up; it does not reflect engine speed.",
        "",
        "Pending on dedicated, otherwise-idle reference hardware (runbook "
        "[`§9`](../reference-hardware-runbook.md)): **GIST1M** (960-d), **Deep10M** (the disk-path "
        "memory headline), and the official absolute-RSS table. Milvus is benchmarked as the **server** "
        "(Docker), not the in-process Lite build, which is not performance-representative.",
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
