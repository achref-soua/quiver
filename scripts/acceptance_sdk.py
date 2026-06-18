# SPDX-License-Identifier: AGPL-3.0-only
"""Real-user acceptance pass over the Python SDK against a live Quiver server.

Drives the full lifecycle (create -> upsert -> filtered search -> get -> delete ->
drop) across every server-ranked index kind, exercises product quantization where
the index supports it, and covers both client-side encryption modes (DCPE and
opaque AEAD) plus multi-vector / ColBERT documents.

Run by ``scripts/acceptance.sh`` against a server booted with encryption-at-rest
ON; can also be run by hand:

    QUIVER_URL=http://127.0.0.1:7333 QUIVER_API_KEY=... \\
      uv run --project sdks/python python scripts/acceptance_sdk.py

Exits non-zero on the first failed check. This is a correctness acceptance test,
not a benchmark — published performance numbers come from the documented
reference hardware (docs/benchmarks/), never from this path.
"""

from __future__ import annotations

import os
import sys

from quiver import Client, DcpeCipher, Document, FilterableField, Point, VectorCipher

URL = os.environ.get("QUIVER_URL", "http://127.0.0.1:7333")
API_KEY = os.environ.get("QUIVER_API_KEY")

# A 256-bit key, hex, reused for both client-side cipher modes in this smoke run.
KEY_HEX = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"

# Eight 8-d points in three recognizable clusters keyed by a filterable `topic`.
CLUSTERS = {
    "search": [
        ("s1", [0.9, 0.1, 0.0, 0.0, 0.1, 0.0, 0.0, 0.0]),
        ("s2", [0.8, 0.2, 0.1, 0.0, 0.0, 0.1, 0.0, 0.0]),
        ("s3", [0.85, 0.15, 0.0, 0.1, 0.0, 0.0, 0.1, 0.0]),
    ],
    "storage": [
        ("d1", [0.0, 0.1, 0.9, 0.1, 0.0, 0.0, 0.0, 0.0]),
        ("d2", [0.1, 0.0, 0.8, 0.2, 0.0, 0.0, 0.0, 0.1]),
        ("d3", [0.0, 0.0, 0.85, 0.15, 0.1, 0.0, 0.0, 0.0]),
    ],
    "ops": [
        ("o1", [0.0, 0.0, 0.0, 0.0, 0.9, 0.1, 0.0, 0.1]),
        ("o2", [0.1, 0.0, 0.0, 0.0, 0.8, 0.2, 0.1, 0.0]),
    ],
}


def _points() -> list[Point]:
    out: list[Point] = []
    for topic, items in CLUSTERS.items():
        for pid, vec in items:
            out.append(Point(pid, vec, {"topic": topic}))
    return out


_PASSES = 0


def check(cond: bool, msg: str) -> None:
    global _PASSES
    if not cond:
        print(f"  FAIL: {msg}", file=sys.stderr)
        raise SystemExit(1)
    _PASSES += 1
    print(f"  ok: {msg}")


def lifecycle(q: Client, index: str, metric: str, pq_subspaces: int | None) -> None:
    """Full CRUD + filtered search over one server-ranked index kind."""
    name = f"acc_{index}"
    label = f"[{index}/{metric}{f'/pq{pq_subspaces}' if pq_subspaces else ''}]"
    print(f"{label} lifecycle")
    try:
        q.delete_collection(name)
    except Exception:  # noqa: BLE001 - best-effort reset
        pass
    q.create_collection(
        name,
        dim=8,
        metric=metric,
        index=index,
        pq_subspaces=pq_subspaces,
        filterable=[FilterableField("topic", "keyword")],
    )
    info = q.get_collection(name)
    check(info.index == index, f"{label} created with index={index}")

    n = q.upsert(name, _points())
    check(n == 8, f"{label} upserted 8 points")

    # Unfiltered search near the 'search' cluster centroid -> a search-cluster id.
    hits = q.search(name, [0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], k=3)
    check(len(hits) == 3, f"{label} search returned k=3")
    check(hits[0].id.startswith("s"), f"{label} nearest is a 'search' point ({hits[0].id})")

    # Hybrid (pre-filtered) search: restrict to topic=storage.
    f = q.search(
        name,
        [0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        k=5,
        filter={"eq": {"field": "topic", "value": "storage"}},
    )
    check(len(f) == 3, f"{label} filter returned only the 3 storage points")
    check(
        all(h.payload and h.payload["topic"] == "storage" for h in f),
        f"{label} every filtered hit has topic=storage",
    )

    got = q.get_point(name, "o1")
    check(got is not None and got.id == "o1", f"{label} get_point round-trips")

    deleted = q.delete_points(name, ["o1", "o2"])
    check(deleted == 2, f"{label} deleted 2 points")
    remaining = q.fetch(name, limit=100)
    check(len(remaining) == 6, f"{label} 6 points remain after delete")

    check(q.delete_collection(name) is True, f"{label} dropped")


def dcpe_mode(q: Client) -> None:
    """DCPE: the server ranks ciphertexts; the key holder encrypts data + query."""
    print("[dcpe] property-preserving encrypted search")
    name = "acc_dcpe"
    cipher = DcpeCipher.from_hex(KEY_HEX, 0.05)
    try:
        q.delete_collection(name)
    except Exception:  # noqa: BLE001
        pass
    q.create_collection(name, dim=8, metric="l2", vector_encryption="dcpe")
    pts = [Point(pid, cipher.encrypt(vec).ciphertext, {"topic": topic})
           for topic, items in CLUSTERS.items() for pid, vec in items]
    check(q.upsert(name, pts) == 8, "[dcpe] upserted 8 ciphertext points")

    enc_q = cipher.encrypt_query([0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    hits = q.search(name, enc_q, k=3)
    check(hits[0].id.startswith("s"), f"[dcpe] nearest under encrypted ranking is 'search' ({hits[0].id})")
    q.delete_collection(name)


def client_side_mode(q: Client) -> None:
    """Opaque AEAD: the server stores blobs it cannot rank; we fetch + rank locally."""
    print("[client_side] semantically secure opaque vectors")
    name = "acc_client_side"
    cipher = VectorCipher.from_hex(KEY_HEX)
    try:
        q.delete_collection(name)
    except Exception:  # noqa: BLE001
        pass
    q.create_collection(name, dim=8, metric="l2", vector_encryption="client_side")
    pts = [
        Point(pid, [0.0] * 8, {"topic": topic, **cipher.seal(vec)})
        for topic, items in CLUSTERS.items()
        for pid, vec in items
    ]
    check(q.upsert(name, pts) == 8, "[client_side] upserted 8 sealed points")

    # The server must not be able to rank these (it never sees the key).
    try:
        q.search(name, [0.0] * 8, k=3)
        check(False, "[client_side] server refused a ranked query")
    except Exception:  # noqa: BLE001 - expected: server cannot rank
        check(True, "[client_side] server refuses to rank an opaque collection")

    hits = q.search_client_side(
        name, [0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], cipher, k=3, metric="l2"
    )
    check(hits[0].id.startswith("s"), f"[client_side] locally-ranked nearest is 'search' ({hits[0].id})")
    q.delete_collection(name)


def multivector_mode(q: Client) -> None:
    """ColBERT-style late interaction: multi-token documents ranked by MaxSim."""
    print("[colbert] multi-vector / late-interaction documents")
    name = "acc_colbert"
    try:
        q.delete_collection(name)
    except Exception:  # noqa: BLE001
        pass
    q.create_collection(name, dim=8, metric="cosine", index="colbert", multivector=True)
    docs = [
        Document("doc-search", [v for _, v in CLUSTERS["search"]], {"topic": "search"}),
        Document("doc-storage", [v for _, v in CLUSTERS["storage"]], {"topic": "storage"}),
        Document("doc-ops", [v for _, v in CLUSTERS["ops"]], {"topic": "ops"}),
    ]
    check(q.upsert_documents(name, docs) == 3, "[colbert] upserted 3 documents")

    query_tokens = [
        [0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        [0.8, 0.2, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0],
    ]
    hits = q.search_multi_vector(name, query_tokens, k=3)
    check(len(hits) >= 1, "[colbert] search_multi_vector returned matches")
    check(hits[0].id == "doc-search", f"[colbert] top document is doc-search ({hits[0].id})")

    check(q.delete_documents(name, ["doc-ops"]) == 1, "[colbert] deleted a document")
    q.delete_collection(name)


def main() -> int:
    print(f"== Quiver SDK acceptance against {URL} ==")
    with Client(URL, api_key=API_KEY) as q:
        check(q.healthz(), "server is healthy")
        # Server-ranked index kinds; PQ where the kind supports it.
        lifecycle(q, "hnsw", "cosine", None)
        lifecycle(q, "ivf", "l2", 4)
        lifecycle(q, "vamana", "l2", None)
        lifecycle(q, "disk_vamana", "l2", 4)
        # Client-side encryption modes.
        dcpe_mode(q)
        client_side_mode(q)
        # Multi-vector / ColBERT.
        multivector_mode(q)
    print(f"\n== SDK acceptance PASSED: {_PASSES} checks ==")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
