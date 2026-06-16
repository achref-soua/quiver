# SPDX-License-Identifier: AGPL-3.0-only
"""Quiver — Python client for the security-first vector database.

Example::

    from quiver import Client, Point

    with Client(api_key="…") as q:
        q.create_collection("items", dim=3, metric="cosine")
        q.upsert("items", [Point("a", [0.1, 0.2, 0.3], {"tag": "x"})])
        hits = q.search("items", [0.1, 0.2, 0.3], k=5)
"""

from .client import (
    Client,
    CollectionInfo,
    Document,
    DocumentMatch,
    FilterableField,
    Match,
    Point,
    QuiverError,
)
from .dcpe import DcpeCipher, DcpeError, EncryptedVector
from .encryption import ENVELOPE_KEY, PayloadCipher, PayloadError, is_sealed
from .vector import (
    VECTOR_ENVELOPE_KEY,
    MalformedVectorEnvelopeError,
    NotEncryptedVectorError,
    VectorCipher,
    VectorDecryptError,
    VectorError,
    is_sealed_vector,
)

__all__ = [
    "Client",
    "Point",
    "Match",
    "Document",
    "DocumentMatch",
    "CollectionInfo",
    "FilterableField",
    "QuiverError",
    "PayloadCipher",
    "PayloadError",
    "is_sealed",
    "ENVELOPE_KEY",
    "DcpeCipher",
    "DcpeError",
    "EncryptedVector",
    "VectorCipher",
    "VectorError",
    "VectorDecryptError",
    "NotEncryptedVectorError",
    "MalformedVectorEnvelopeError",
    "is_sealed_vector",
    "VECTOR_ENVELOPE_KEY",
]
__version__ = "0.12.0"
