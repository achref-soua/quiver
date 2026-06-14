# SPDX-License-Identifier: AGPL-3.0-only
"""Client-side payload encryption (ADR-0012) for the Quiver Python SDK.

This mirrors the reference envelope in ``quiver_crypto::payload`` byte-for-byte:
a caller seals a JSON payload with a 256-bit key Quiver never sees, and the
server stores and returns it as an opaque blob it cannot read. Sealing uses
**XChaCha20-Poly1305** (the libsodium ``xchacha20poly1305_ietf`` construction,
which interoperates with the Rust reference) with a fresh random 192-bit nonce.

Requires the optional ``[encryption]`` extra (PyNaCl)::

    pip install quiver-client[encryption]

Keep fields server-filterable by leaving them in cleartext and merging the
sealed envelope alongside them — :meth:`PayloadCipher.open` reads only the
reserved key and ignores cleartext siblings::

    from quiver.encryption import PayloadCipher

    cipher = PayloadCipher.from_hex("…64 hex chars…")
    payload = {"tier": "gold", **cipher.seal({"ssn": "078-05-1120"})}
    # ... upsert `payload`; the server only ever sees ciphertext for `ssn`.
    secret = cipher.open(payload)          # -> {"ssn": "078-05-1120"}

**Use a dedicated key for payload encryption; never reuse your at-rest
``QUIVER_ENCRYPTION_KEY``.** The client owns the key — losing it means the data
is unrecoverable.
"""

from __future__ import annotations

import base64
import json
import os
from typing import Any

#: The reserved payload key under which a sealed envelope is stored.
ENVELOPE_KEY = "__quiver_enc__"

_VERSION = 1
_ALG = "xchacha20poly1305"
_NONCE_LEN = 24
_TAG_LEN = 16
_AAD = b"quiver/payload/v1"


class PayloadError(Exception):
    """Base class for client-side payload encryption errors."""


class NotEncryptedError(PayloadError):
    """The value carries no Quiver envelope (the reserved key is absent)."""


class MalformedEnvelopeError(PayloadError):
    """The envelope is structurally invalid or uses an unsupported version."""


class DecryptError(PayloadError):
    """Decryption failed: the wrong key or a tampered ciphertext."""


def _bindings() -> Any:
    """Import PyNaCl's libsodium bindings, with a clear error if it is missing."""
    try:
        from nacl import bindings
    except ImportError as exc:  # pragma: no cover - exercised only without the extra
        raise PayloadError(
            "client-side payload encryption requires the optional dependency PyNaCl; "
            "install it with: pip install quiver-client[encryption]"
        ) from exc
    return bindings


class PayloadCipher:
    """A client-held key for sealing and opening payload envelopes (ADR-0012)."""

    __slots__ = ("_key",)

    def __init__(self, key: bytes) -> None:
        if len(key) != 32:
            raise ValueError(f"payload key must be 32 bytes, got {len(key)}")
        self._key = bytes(key)

    @classmethod
    def from_hex(cls, hex_key: str) -> PayloadCipher:
        """Build a cipher from a 64-character hex-encoded 256-bit key."""
        try:
            key = bytes.fromhex(hex_key.strip())
        except ValueError as exc:
            raise ValueError(f"invalid payload key: {exc}") from exc
        return cls(key)

    def seal(self, plaintext: Any) -> dict[str, Any]:
        """Seal ``plaintext`` into a one-key envelope ``{ENVELOPE_KEY: {...}}``.

        Each call uses a fresh random nonce, so sealing the same value twice
        yields different ciphertext.
        """
        bindings = _bindings()
        message = json.dumps(
            plaintext, separators=(",", ":"), ensure_ascii=False
        ).encode("utf-8")
        nonce = os.urandom(_NONCE_LEN)
        ciphertext = bindings.crypto_aead_xchacha20poly1305_ietf_encrypt(
            message, _AAD, nonce, self._key
        )
        return {
            ENVELOPE_KEY: {
                "v": _VERSION,
                "alg": _ALG,
                "n": base64.b64encode(nonce).decode("ascii"),
                "ct": base64.b64encode(ciphertext).decode("ascii"),
            }
        }

    def open(self, sealed: Any) -> Any:
        """Open an envelope sealed by :meth:`seal`, returning the plaintext.

        ``sealed`` may carry cleartext sibling fields; only the reserved key is
        read. A wrong key or any tampering raises :class:`DecryptError`.
        """
        if not is_sealed(sealed):
            raise NotEncryptedError("payload is not a quiver-encrypted envelope")
        envelope = sealed[ENVELOPE_KEY]
        if not isinstance(envelope, dict):
            raise MalformedEnvelopeError("envelope is not an object")
        if envelope.get("v") != _VERSION:
            raise MalformedEnvelopeError(
                f"unsupported envelope version: {envelope.get('v')!r}"
            )
        if envelope.get("alg") != _ALG:
            raise MalformedEnvelopeError(
                f"unsupported envelope algorithm: {envelope.get('alg')!r}"
            )
        nonce = _decode_field(envelope, "n")
        if len(nonce) != _NONCE_LEN:
            raise MalformedEnvelopeError(
                f"nonce is {len(nonce)} bytes, expected {_NONCE_LEN}"
            )
        ciphertext = _decode_field(envelope, "ct")
        if len(ciphertext) < _TAG_LEN:
            raise MalformedEnvelopeError(
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
            raise DecryptError("wrong key or tampered ciphertext") from exc
        return json.loads(message)


def is_sealed(value: Any) -> bool:
    """Whether ``value`` carries a Quiver payload envelope."""
    return isinstance(value, dict) and ENVELOPE_KEY in value


def _decode_field(envelope: dict[str, Any], field: str) -> bytes:
    raw = envelope.get(field)
    if not isinstance(raw, str):
        raise MalformedEnvelopeError(f"missing envelope field {field!r}")
    try:
        return base64.b64decode(raw, validate=True)
    except (ValueError, TypeError) as exc:
        raise MalformedEnvelopeError(
            f"envelope field {field!r} is not base64: {exc}"
        ) from exc
