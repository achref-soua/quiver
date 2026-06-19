# SPDX-License-Identifier: AGPL-3.0-only
"""Dataset loading: the SIFT/`.fvecs` family, plus a small synthetic set with
exact ground truth for smoke runs.

`.fvecs`/`.ivecs` are the standard ann-benchmarks layout: each vector is a
little-endian int32 dimension count followed by that many float32 (`.fvecs`) or
int32 (`.ivecs`) values.
"""

from __future__ import annotations

import tarfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path

import numpy as np

# The TEXMEX corpus (http://corpus-texmex.irisa.fr/) publishes SIFT and GIST as
# gzipped tars of `.fvecs`/`.ivecs` files. SIFT1M ships with the repo; GIST1M is
# downloaded on demand because it is ~2.6 GB.
TEXMEX_URLS = {
    "sift": "ftp://ftp.irisa.fr/local/texmex/corpus/sift.tar.gz",
    "gist": "ftp://ftp.irisa.fr/local/texmex/corpus/gist.tar.gz",
}


@dataclass
class Dataset:
    """A benchmark dataset: base vectors, queries, and exact neighbour indices."""

    base: np.ndarray  # (n, dim) float32
    queries: np.ndarray  # (q, dim) float32
    ground_truth: np.ndarray  # (q, k) int32 — indices into `base`

    @property
    def name(self) -> str:
        return f"{self.base.shape[0]}x{self.base.shape[1]}"


def read_fvecs(path: Path) -> np.ndarray:
    """Read an `.fvecs` file into an ``(n, dim)`` float32 array."""
    raw = np.fromfile(path, dtype=np.int32)
    if raw.size == 0:
        return np.zeros((0, 0), dtype=np.float32)
    dim = int(raw[0])
    stride = dim + 1
    rows = raw.reshape(-1, stride)
    return rows[:, 1:].view(np.float32).astype(np.float32)


def read_ivecs(path: Path) -> np.ndarray:
    """Read an `.ivecs` file into an ``(n, dim)`` int32 array."""
    raw = np.fromfile(path, dtype=np.int32)
    if raw.size == 0:
        return np.zeros((0, 0), dtype=np.int32)
    dim = int(raw[0])
    stride = dim + 1
    return raw.reshape(-1, stride)[:, 1:].astype(np.int32)


def brute_force_l2(base: np.ndarray, queries: np.ndarray, k: int) -> np.ndarray:
    """Exact top-``k`` L2 neighbours (indices into ``base``) for each query."""
    # ||b - q||^2 = ||b||^2 - 2 b·q + ||q||^2; the per-query constant ||q||^2 is
    # irrelevant to the ordering, so it is omitted.
    base_sq = np.einsum("ij,ij->i", base, base)
    out = np.empty((queries.shape[0], k), dtype=np.int32)
    for i, q in enumerate(queries):
        dist = base_sq - 2.0 * (base @ q)
        out[i] = np.argpartition(dist, k - 1)[:k][np.argsort(dist[np.argpartition(dist, k - 1)[:k]])]
    return out


def load_sift(directory: Path, *, k: int = 100) -> Dataset:
    """Load SIFT1M-style files from ``directory``.

    Expects ``*_base.fvecs``, ``*_query.fvecs`` and (optionally)
    ``*_groundtruth.ivecs``; ground truth is brute-forced when absent.
    """
    base = read_fvecs(_one(directory, "*_base.fvecs"))
    queries = read_fvecs(_one(directory, "*_query.fvecs"))
    gt_files = sorted(directory.glob("*_groundtruth.ivecs"))
    ground_truth = read_ivecs(gt_files[0]) if gt_files else brute_force_l2(base, queries, k)
    return Dataset(base=base, queries=queries, ground_truth=ground_truth)


def synthetic(*, n: int, dim: int, queries: int, k: int = 10, seed: int = 42) -> Dataset:
    """A small random dataset with exact L2 ground truth, for smoke runs.

    Deterministic given ``seed`` — it exercises the harness end to end without
    downloading SIFT1M, and is never a source of published numbers.
    """
    rng = np.random.default_rng(seed)
    base = rng.standard_normal((n, dim), dtype=np.float32)
    query = rng.standard_normal((queries, dim), dtype=np.float32)
    return Dataset(base=base, queries=query, ground_truth=brute_force_l2(base, query, k))


def ensure_texmex(name: str, datasets_dir: Path) -> Path:
    """Return the directory for a TEXMEX dataset (``sift`` or ``gist``), downloading
    and extracting it under ``datasets_dir`` if the ``*_base.fvecs`` is absent.

    The download is large (GIST1M ≈ 2.6 GB); it is fetched once and cached. We do
    not pin a checksum — TEXMEX does not publish one — so the cache is trusted
    after a successful extract. Never used as a source of *fabricated* numbers.
    """
    if name not in TEXMEX_URLS:
        raise ValueError(f"no TEXMEX URL for {name!r}; known: {sorted(TEXMEX_URLS)}")
    dest = datasets_dir / name
    if sorted(dest.glob("*_base.fvecs")):
        return dest
    datasets_dir.mkdir(parents=True, exist_ok=True)
    archive = datasets_dir / f"{name}.tar.gz"
    if not archive.exists():
        url = TEXMEX_URLS[name]
        print(f"downloading {name} from {url} (large; one-time) ...")
        urllib.request.urlretrieve(url, archive)  # noqa: S310 - trusted TEXMEX host
    print(f"extracting {archive} ...")
    with tarfile.open(archive) as tar:
        tar.extractall(datasets_dir, filter="data")
    if not sorted(dest.glob("*_base.fvecs")):
        raise FileNotFoundError(
            f"{name} extracted to {datasets_dir} but no *_base.fvecs under {dest}"
        )
    return dest


def _one(directory: Path, pattern: str) -> Path:
    matches = sorted(directory.glob(pattern))
    if not matches:
        raise FileNotFoundError(f"no file matching {pattern!r} in {directory}")
    return matches[0]
