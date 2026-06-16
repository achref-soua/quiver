# SPDX-License-Identifier: AGPL-3.0-only
"""Client-side opaque vector encryption (ADR-0032) for the Quiver Python SDK.

This mirrors the reference envelope in ``quiver_crypto::vector`` byte-for-byte: a
caller seals a vector's raw little-endian ``f32`` bytes with **XChaCha20-Poly1305**
(the libsodium ``xchacha20poly1305_ietf`` construction) under the reserved
``__quiver_vec__`` key. The server stores the blob (in the payload) plus a zero
placeholder vector, does **no** distance math, and never sees the key; the client
:meth:`~quiver.client.Client.fetch` es the entitled set, decrypts, and ranks
locally. It is the semantically secure end of Quiver's encrypted-search spectrum.

Unlike DCPE (:mod:`quiver.dcpe`), which lets the server rank ciphertexts and leaks
the distance-comparison relation by design, this mode leaks nothing about the
vectors (the server sees only ciphertext, the collection size/dimension, the
cleartext fields you leave filterable, and access patterns). Because the sealed
message is raw bytes, interop with the Rust reference is **bit-exact**.

Requires the optional ``[encryption]`` extra (PyNaCl)::

    pip install quiver-client[encryption]

Usage::

    from quiver.vector import VectorCipher

    cipher = VectorCipher.from_hex("…64 hex chars…")
    payload = {"tier": "gold", **cipher.seal([0.1, -0.2, 0.3, 0.4])}
    # ... upsert with a zero placeholder vector + this payload; the server stores
    # opaque ciphertext. Later, after Client.fetch, recover it:
    vector = cipher.open(payload)          # -> [0.1, -0.2, 0.3, 0.4]

**Use a dedicated key for vector encryption; never reuse your at-rest
``QUIVER_ENCRYPTION_KEY``.** The client owns the key — losing it means the vectors
are unrecoverable.
"""

from __future__ import annotations

import base64
import os
import struct
from typing import Any, Sequence

#: The reserved payload key under which a sealed vector envelope is stored.
VECTOR_ENVELOPE_KEY = "__quiver_vec__"

_VERSION = 1
_ALG = "xchacha20poly1305"
_NONCE_LEN = 24
_TAG_LEN = 16
_AAD = b"quiver/vector/v1"


class VectorError(Exception):
    """Base class for client-side vector encryption errors."""


class NotEncryptedVectorError(VectorError):
    """The value carries no Quiver vector envelope (the reserved key is absent)."""


class MalformedVectorEnvelopeError(VectorError):
    """The envelope is structurally invalid, unsupported, or its length/dim disagree."""


class VectorDecryptError(VectorError):
    """Decryption failed: the wrong key or a tampered ciphertext."""


def _bindings() -> Any:
    """Import PyNaCl's libsodium bindings, with a clear error if it is missing."""
    try:
        from nacl import bindings
    except ImportError as exc:  # pragma: no cover - exercised only without the extra
        raise VectorError(
            "client-side vector encryption requires the optional dependency PyNaCl; "
            "install it with: pip install quiver-client[encryption]"
        ) from exc
    return bindings


class VectorCipher:
    """A client-held key for sealing and opening vector envelopes (ADR-0032)."""

    __slots__ = ("_key",)

    def __init__(self, key: bytes) -> None:
        if len(key) != 32:
            raise ValueError(f"vector key must be 32 bytes, got {len(key)}")
        self._key = bytes(key)

    @classmethod
    def from_hex(cls, hex_key: str) -> VectorCipher:
        """Build a cipher from a 64-character hex-encoded 256-bit key."""
        try:
            key = bytes.fromhex(hex_key.strip())
        except ValueError as exc:
            raise ValueError(f"invalid vector key: {exc}") from exc
        return cls(key)

    def seal(self, vector: Sequence[float]) -> dict[str, Any]:
        """Seal ``vector`` into a one-key envelope ``{VECTOR_ENVELOPE_KEY: {...}}``.

        Each call uses a fresh random nonce, so sealing the same vector twice
        yields different ciphertext.
        """
        bindings = _bindings()
        values = [float(x) for x in vector]
        message = struct.pack(f"<{len(values)}f", *values)
        nonce = os.urandom(_NONCE_LEN)
        ciphertext = bindings.crypto_aead_xchacha20poly1305_ietf_encrypt(
            message, _AAD, nonce, self._key
        )
        return {
            VECTOR_ENVELOPE_KEY: {
                "v": _VERSION,
                "alg": _ALG,
                "dim": len(values),
                "n": base64.b64encode(nonce).decode("ascii"),
                "ct": base64.b64encode(ciphertext).decode("ascii"),
            }
        }

    def open(self, sealed: Any) -> list[float]:
        """Open an envelope sealed by :meth:`seal`, returning the vector.

        ``sealed`` may carry cleartext sibling fields; only the reserved key is
        read. A wrong key or any tampering raises :class:`VectorDecryptError`.
        """
        if not is_sealed_vector(sealed):
            raise NotEncryptedVectorError("value is not a quiver-encrypted vector envelope")
        envelope = sealed[VECTOR_ENVELOPE_KEY]
        if not isinstance(envelope, dict):
            raise MalformedVectorEnvelopeError("envelope is not an object")
        if envelope.get("v") != _VERSION:
            raise MalformedVectorEnvelopeError(
                f"unsupported envelope version: {envelope.get('v')!r}"
            )
        if envelope.get("alg") != _ALG:
            raise MalformedVectorEnvelopeError(
                f"unsupported envelope algorithm: {envelope.get('alg')!r}"
            )
        dim = envelope.get("dim")
        if not isinstance(dim, int) or isinstance(dim, bool) or dim < 0:
            raise MalformedVectorEnvelopeError(f"missing or invalid dim: {dim!r}")
        nonce = _decode_field(envelope, "n")
        if len(nonce) != _NONCE_LEN:
            raise MalformedVectorEnvelopeError(
                f"nonce is {len(nonce)} bytes, expected {_NONCE_LEN}"
            )
        ciphertext = _decode_field(envelope, "ct")
        if len(ciphertext) < _TAG_LEN:
            raise MalformedVectorEnvelopeError(
                f"ciphertext is {len(ciphertext)} bytes, shorter than the "
                f"{_TAG_LEN}-byte tag"
            )
        bindings = _bindings()
        from nacl.exceptions import CryptoError as NaClCryptoError

        try:
            message = bindings.crypto_aead_xchacha20poly1305_ietf_decrypt(
                ciphertext, _AAD, nonce, self._key
            )
        except NaClCryptoError as exc:
            raise VectorDecryptError("wrong key or tampered ciphertext") from exc
        if len(message) != dim * 4:
            raise MalformedVectorEnvelopeError(
                f"decrypted {len(message)} bytes, expected {dim * 4} for dim {dim}"
            )
        return list(struct.unpack(f"<{dim}f", message))


def is_sealed_vector(value: Any) -> bool:
    """Whether ``value`` carries a Quiver vector envelope."""
    return isinstance(value, dict) and VECTOR_ENVELOPE_KEY in value


def _decode_field(envelope: dict[str, Any], field: str) -> bytes:
    raw = envelope.get(field)
    if not isinstance(raw, str):
        raise MalformedVectorEnvelopeError(f"missing envelope field {field!r}")
    try:
        return base64.b64decode(raw, validate=True)
    except (ValueError, TypeError) as exc:
        raise MalformedVectorEnvelopeError(
            f"envelope field {field!r} is not base64: {exc}"
        ) from exc
