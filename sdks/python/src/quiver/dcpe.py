# SPDX-License-Identifier: AGPL-3.0-only
"""Client-side DCPE vector encryption (ADR-0031) for the Quiver Python SDK.

This is a faithful port of the reference implementation in ``quiver_crypto::dcpe``:
the **Scale-And-Perturb (SAP)** distance-comparison-preserving scheme (Fuchsbauer,
Ghosal, Hauke & O'Neill, *ePrint 2021/1666*, SCN 2022). It lets you encrypt
embedding vectors so a Quiver server can answer approximate-nearest-neighbour
queries over the ciphertexts **without ever holding the plaintext vectors or the
key** — Euclidean distance comparison is preserved, up to a tunable margin.

Requires the optional ``[dcpe]`` extra (the ``cryptography`` package, for
ChaCha20)::

    pip install quiver-client[dcpe]

Usage — encrypt vectors before upserting, and queries before searching, with the
*same* cipher::

    from quiver import Client
    from quiver.dcpe import DcpeCipher

    cipher = DcpeCipher.from_hex("…64 hex chars…", approximation_factor=0.02)
    q = Client("http://localhost:4000", api_key="…")
    q.create_collection("vault", dim=8, metric="l2", encrypted_vectors=True)
    sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8])
    q.upsert("vault", [{"id": "a", "vector": sealed.ciphertext}])
    hits = q.search("vault", cipher.encrypt_query(my_query_vector), k=10)

.. warning::

   **DCPE is experimental and is _not_ semantically secure.** It leaks the
   approximate distance-comparison relation by design (that is what makes the
   encrypted search work), is **L2-only**, and is broken by known-plaintext or
   strong-prior adversaries. It complements — does not replace — encryption at
   rest. Use a **dedicated** key, and prefer to encrypt and query from the same
   client. See ADR-0031 and ``docs/security/dcpe.md``.

Because the ciphertext is float-valued and uses transcendental functions,
bit-exact reproduction against the Rust reference is not guaranteed (libm ULP
differences); interop is validated within a tolerance. The Rust module is the
canonical reference.
"""

from __future__ import annotations

import hashlib
import hmac
import math
import os
import struct
from dataclasses import dataclass

#: DCPE initialisation-vector length in bytes (a 96-bit ChaCha20 nonce).
IV_LEN = 12
#: DCPE integrity-tag length in bytes (full HMAC-SHA256 output).
TAG_LEN = 32

_INFO_SCALE = b"quiver/dcpe/v1/scale"
_INFO_PRF = b"quiver/dcpe/v1/prf"
_INFO_AUTH = b"quiver/dcpe/v1/auth"
_AUTH_DOMAIN = b"quiver/dcpe/v1/tag"

_TWO_POW_53 = float(1 << 53)


class DcpeError(Exception):
    """A DCPE encryption, decryption, or construction error."""


def _hkdf_sha256(ikm: bytes, info: bytes, length: int) -> bytes:
    """RFC 5869 HKDF-SHA256 with a zero salt, matching the Rust ``hkdf`` crate's
    ``Hkdf::new(None, ikm)`` followed by ``expand``."""
    prk = hmac.new(b"\x00" * 32, ikm, hashlib.sha256).digest()
    okm = b""
    block = b""
    counter = 1
    while len(okm) < length:
        block = hmac.new(prk, block + info + bytes([counter]), hashlib.sha256).digest()
        okm += block
        counter += 1
    return okm[:length]


class _KeyStream:
    """The raw ChaCha20 keystream seeded from ``(key, iv)``, read as little-endian
    ``u64``s, with Box-Muller standard normals (the sine partner cached). The
    layout matches ``quiver_crypto::dcpe`` byte-for-byte."""

    def __init__(self, key: bytes, iv: bytes) -> None:
        # `cryptography`'s ChaCha20 takes a 16-byte nonce = 4-byte little-endian
        # counter (start 0) followed by the 12-byte nonce, which is exactly the
        # RustCrypto ChaCha20 initial state.
        from cryptography.hazmat.primitives.ciphers import Cipher, algorithms

        nonce16 = (0).to_bytes(4, "little") + iv
        self._enc = Cipher(algorithms.ChaCha20(key, nonce16), mode=None).encryptor()
        self._buf = b""
        self._pos = 0
        self._spare: float | None = None

    def _next_u64(self) -> int:
        if self._pos + 8 > len(self._buf):
            # Pull a fresh chunk of keystream (encrypting zeros).
            self._buf = self._enc.update(b"\x00" * 4096)
            self._pos = 0
        word = int.from_bytes(self._buf[self._pos : self._pos + 8], "little")
        self._pos += 8
        return word

    def next_unit(self) -> float:
        """A uniform in ``[0, 1)`` with 53-bit resolution."""
        return (self._next_u64() >> 11) / _TWO_POW_53

    def next_normal(self) -> float:
        """A standard normal via Box-Muller; ``u1 in (0, 1]`` so ``log`` is finite."""
        if self._spare is not None:
            z = self._spare
            self._spare = None
            return z
        u1 = 1.0 - self.next_unit()
        u2 = self.next_unit()
        r = math.sqrt(-2.0 * math.log(u1))
        theta = 2.0 * math.pi * u2
        self._spare = r * math.sin(theta)
        return r * math.cos(theta)


def _f32(x: float) -> float:
    """Round a Python float to f32 precision, matching the engine's storage."""
    return struct.unpack("<f", struct.pack("<f", x))[0]


@dataclass
class EncryptedVector:
    """A DCPE-encrypted vector: the ciphertext (upserted and searched like any
    vector), the IV seeding its perturbation, and an HMAC-SHA256 integrity tag."""

    ciphertext: list[float]
    iv: bytes
    tag: bytes


class DcpeCipher:
    """A client-held DCPE key bound to one approximation factor (ADR-0031).

    Construct one cipher per ``(key, approximation_factor)`` and reuse it; the
    same factor must be used for the data and the queries searched against it.
    """

    __slots__ = ("_scale", "_prf_key", "_auth_key", "_beta")

    def __init__(self, key: bytes, approximation_factor: float) -> None:
        if len(key) != 32:
            raise DcpeError("DCPE key must be exactly 32 bytes (256 bits)")
        if not math.isfinite(approximation_factor) or approximation_factor < 0.0:
            raise DcpeError("approximation factor must be finite and >= 0")
        # Match the Rust f32 approximation factor exactly (it is bound into the tag).
        self._beta = _f32(approximation_factor)
        scale_bytes = _hkdf_sha256(key, _INFO_SCALE, 8)
        frac = (int.from_bytes(scale_bytes, "little") >> 11) / _TWO_POW_53
        self._scale = 1.0 + frac
        self._prf_key = _hkdf_sha256(key, _INFO_PRF, 32)
        self._auth_key = _hkdf_sha256(key, _INFO_AUTH, 32)

    @classmethod
    def from_hex(cls, hex_key: str, approximation_factor: float) -> "DcpeCipher":
        """Build a cipher from a 64-character hex-encoded 256-bit key."""
        try:
            key = bytes.fromhex(hex_key)
        except ValueError as exc:
            raise DcpeError(f"invalid DCPE key: {exc}") from exc
        return cls(key, approximation_factor)

    @property
    def scale(self) -> float:
        """The secret, key-derived scaling factor ``s in [1, 2)``."""
        return self._scale

    @property
    def approximation_factor(self) -> float:
        """The approximation factor this cipher was built with."""
        return self._beta

    def encrypt(self, vector: list[float]) -> EncryptedVector:
        """Encrypt a vector for storage with a fresh random IV."""
        if not vector:
            raise DcpeError("empty vector: DCPE needs at least one dimension")
        iv = os.urandom(IV_LEN)
        ciphertext = self._scale_and_perturb(vector, iv)
        tag = self._tag(iv, ciphertext)
        return EncryptedVector(ciphertext=ciphertext, iv=iv, tag=tag)

    def encrypt_query(self, vector: list[float]) -> list[float]:
        """Encrypt a query vector for searching against DCPE-encrypted data."""
        if not vector:
            raise DcpeError("empty vector: DCPE needs at least one dimension")
        return self._scale_and_perturb(vector, os.urandom(IV_LEN))

    def decrypt(self, sealed: EncryptedVector) -> list[float]:
        """Verify the integrity tag (constant-time) and recover the plaintext."""
        if not sealed.ciphertext:
            raise DcpeError("empty vector: DCPE needs at least one dimension")
        expected = self._tag(sealed.iv, sealed.ciphertext)
        if not hmac.compare_digest(expected, sealed.tag):
            raise DcpeError("integrity check failed: wrong key or tampered ciphertext")
        lam = self._perturbation(sealed.iv, len(sealed.ciphertext))
        return [_f32((c - l) / self._scale) for c, l in zip(sealed.ciphertext, lam)]

    def _scale_and_perturb(self, vector: list[float], iv: bytes) -> list[float]:
        lam = self._perturbation(iv, len(vector))
        return [_f32(self._scale * m + l) for m, l in zip(vector, lam)]

    def _perturbation(self, iv: bytes, d: int) -> list[float]:
        rng = _KeyStream(self._prf_key, iv)
        direction = [rng.next_normal() for _ in range(d)]
        norm = math.sqrt(sum(x * x for x in direction))
        u = rng.next_unit()
        radius = (self._scale / 4.0) * self._beta * (u ** (1.0 / d))
        if norm == 0.0:
            return [0.0] * d
        return [x / norm * radius for x in direction]

    def _tag(self, iv: bytes, ciphertext: list[float]) -> bytes:
        mac = hmac.new(self._auth_key, digestmod=hashlib.sha256)
        mac.update(_AUTH_DOMAIN)
        mac.update(struct.pack("<f", self._beta))
        mac.update(iv)
        for c in ciphertext:
            mac.update(struct.pack("<f", c))
        return mac.digest()
