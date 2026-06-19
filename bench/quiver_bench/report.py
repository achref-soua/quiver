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

    # Header
    lines += [
        "# Quiver v0.17.0 — Multi-DB Benchmark Comparison",
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
                    "qdrant": "1.13.4",
                    "pgvector": "0.7/pg16",
                    "weaviate": "1.27.0",
                }
                ver = versions.get(comp_name, "?")
                note = "[reference-hardware-pending]" if not is_smoke else "smoke only"
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
        "## Reference-hardware-pending figures",
        "",
        "The following results require reproduction on dedicated, otherwise-idle hardware "
        "(see [`docs/benchmarks/reference-hardware-runbook.md`](../reference-hardware-runbook.md), "
        "§9 for the full multi-DB procedure):",
        "",
        "- **Quiver SIFT1M** — uploading 1M vectors through the REST API takes ~12 minutes "
        "on this shared dev box; the comparison harness therefore skipped the Quiver SIFT1M run. "
        "Real measured numbers (recall@10 vs QPS at `ef_search` 16–256, HNSW, L2) are in "
        "[`docs/benchmarks/results/sift1m.md`](./sift1m.md) (single-DB harness, same methodology). "
        "A full cross-competitor Quiver run at SIFT1M requires the reference hardware setup.",
        "- **SIFT1M Docker competitors** (Qdrant, pgvector, Weaviate) — Docker API overhead "
        "at 1M-vector scale requires a dedicated machine and is not run here.",
        "- **GloVe-100** (cosine metric, ~1.2M vectors).",
        "- **Deep10M** (disk-path, 10M vectors — the memory-frugality headline).",
        "",
    ]

    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="quiver_bench.report")
    p.add_argument("result_dir", type=Path, nargs="?",
                   default=Path("docs/benchmarks/results/comparison-v0.17.0"))
    args = p.parse_args(sys.argv[1:] if argv is None else argv)

    if not args.result_dir.exists():
        print(f"ERROR: {args.result_dir} does not exist. Run `just bench-compare` first.", file=sys.stderr)
        return 1

    report = generate(args.result_dir)
    out = args.result_dir / "comparison-v0.17.0.md"
    out.write_text(report)
    print(f"Report written to {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
