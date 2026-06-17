# SPDX-License-Identifier: AGPL-3.0-only
"""DCPE client-side vector encryption (ADR-0031), including a cross-language
known-answer test against the Rust reference (``quiver_crypto::dcpe``)."""

from __future__ import annotations

import math
import random

import pytest

from quiver.dcpe import DcpeCipher, DcpeError, EncryptedVector, Normalization

# A known-answer vector produced by the Rust reference implementation. Decrypting
# it here exercises the whole construction — HKDF (the scale and sub-keys), the
# ChaCha20 CSPRNG, Box-Muller, and HMAC — and proves the Python port matches Rust.
KAT_KEY = "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f"
KAT_BETA = 0.05
KAT_SCALE = 1.95453267099551331
KAT_IV = "112233445566778899aabbcc"
KAT_PLAIN = [0.1, -0.2, 0.3, -0.4, 0.5, 0.6, -0.7, 0.8]
# Cipher v2 (ADR-0035): the ciphertext components are the *shuffled* plaintext
# scale-and-perturbed; the key-derived permutation for this key at d=8 is
# [2, 6, 1, 5, 7, 0, 4, 3]. Decrypt un-shuffles to recover KAT_PLAIN.
KAT_CT = [
    0.5790184,
    -1.3649843,
    -0.3800147,
    1.1816978,
    1.5671049,
    0.18977723,
    0.98995024,
    -0.7886901,
]
KAT_TAG = "0e37dacb37dd8b1bc6f2f2eced612fc66e9dd2ca1efe859817328680454ba176"


def test_kat_matches_the_rust_reference() -> None:
    cipher = DcpeCipher.from_hex(KAT_KEY, KAT_BETA)
    # The key-derived scaling factor matches Rust exactly (HKDF is byte-exact).
    assert abs(cipher.scale - KAT_SCALE) < 1e-12
    sealed = EncryptedVector(
        ciphertext=KAT_CT, iv=bytes.fromhex(KAT_IV), tag=bytes.fromhex(KAT_TAG)
    )
    # The tag must verify (an exact HKDF + HMAC match) and the plaintext must come
    # back (the ChaCha20 + Box-Muller perturbation must match within float ULPs).
    recovered = cipher.decrypt(sealed)
    assert len(recovered) == len(KAT_PLAIN)
    for got, want in zip(recovered, KAT_PLAIN):
        assert abs(got - want) < 1e-3, f"{got} vs {want}"


def test_round_trip_recovers_the_plaintext() -> None:
    cipher = DcpeCipher.from_hex("11" * 32, 0.1)
    plain = [0.5, -0.25, 0.125, 0.0, 0.9, -0.9, 0.33, -0.66]
    sealed = cipher.encrypt(plain)
    recovered = cipher.decrypt(sealed)
    for got, want in zip(recovered, plain):
        assert abs(got - want) < 1e-3


def test_each_encryption_uses_a_fresh_iv() -> None:
    cipher = DcpeCipher.from_hex("22" * 32, 0.1)
    a = cipher.encrypt([0.1, 0.2, 0.3, 0.4])
    b = cipher.encrypt([0.1, 0.2, 0.3, 0.4])
    assert a.iv != b.iv
    assert a.ciphertext != b.ciphertext


def test_wrong_key_and_tamper_fail_integrity() -> None:
    cipher = DcpeCipher.from_hex("33" * 32, 0.1)
    sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4])
    other = DcpeCipher.from_hex("44" * 32, 0.1)
    with pytest.raises(DcpeError):
        other.decrypt(sealed)
    tampered = EncryptedVector(
        ciphertext=[c + 0.5 for c in sealed.ciphertext], iv=sealed.iv, tag=sealed.tag
    )
    with pytest.raises(DcpeError):
        cipher.decrypt(tampered)


def test_preserves_nearest_neighbours_at_small_beta() -> None:
    rng = random.Random(1)
    data = [[rng.uniform(-0.5, 0.5) for _ in range(32)] for _ in range(300)]
    queries = [[rng.uniform(-0.5, 0.5) for _ in range(32)] for _ in range(15)]
    cipher = DcpeCipher.from_hex("55" * 32, 0.02)
    enc = [cipher.encrypt(v).ciphertext for v in data]

    def l2(a: list[float], b: list[float]) -> float:
        return sum((x - y) ** 2 for x, y in zip(a, b))

    def top_k(q: list[float], pts: list[list[float]], k: int) -> set[int]:
        return set(sorted(range(len(pts)), key=lambda i: l2(q, pts[i]))[:k])

    k = 10
    hits = 0
    for q in queries:
        truth = top_k(q, data, k)
        got = top_k(cipher.encrypt_query(q), enc, k)
        hits += len(truth & got)
    recall = hits / (len(queries) * k)
    assert recall > 0.9, f"recall {recall:.3f}"


def test_rejects_invalid_inputs() -> None:
    for bad in (-0.1, math.nan, math.inf):
        with pytest.raises(DcpeError):
            DcpeCipher.from_hex("66" * 32, bad)
    with pytest.raises(DcpeError):
        DcpeCipher.from_hex("not-hex", 0.1)
    cipher = DcpeCipher.from_hex("77" * 32, 0.1)
    with pytest.raises(DcpeError):
        cipher.encrypt([])


def test_shuffle_matches_the_rust_reference() -> None:
    # The key-derived permutation for the KAT key at d=8 (cipher v2).
    cipher = DcpeCipher.from_hex(KAT_KEY, KAT_BETA)
    assert cipher._permutation(8) == [2, 6, 1, 5, 7, 0, 4, 3]
    # It is a valid permutation, deterministic, and key-dependent.
    perm = cipher._permutation(16)
    assert sorted(perm) == list(range(16))
    assert perm == cipher._permutation(16)
    assert perm != DcpeCipher.from_hex("88" * 32, 0.1)._permutation(16)


def test_normalization_round_trips() -> None:
    norm = Normalization(shift=[0.5, -0.5, 1.0, 0.0, 2.0, -1.0, 0.25, -0.25], scale=3.0)
    cipher = DcpeCipher.from_hex("99" * 32, 0.1, norm)
    plain = [0.1, -0.2, 0.3, -0.4, 0.5, 0.6, -0.7, 0.8]
    recovered = cipher.decrypt(cipher.encrypt(plain))
    for got, want in zip(recovered, plain):
        assert abs(got - want) < 1e-3, f"{got} vs {want}"


def test_normalization_preserves_nearest_neighbours() -> None:
    rng = random.Random(7)
    data = [[rng.uniform(-0.5, 0.5) for _ in range(16)] for _ in range(200)]
    queries = [[rng.uniform(-0.5, 0.5) for _ in range(16)] for _ in range(10)]
    norm = Normalization(shift=[0.1 * i for i in range(16)], scale=5.0)
    cipher = DcpeCipher.from_hex("aa" * 32, 0.02, norm)
    enc = [cipher.encrypt(v).ciphertext for v in data]

    def l2(a: list[float], b: list[float]) -> float:
        return sum((x - y) ** 2 for x, y in zip(a, b))

    def top_k(q: list[float], pts: list[list[float]], k: int) -> set[int]:
        return set(sorted(range(len(pts)), key=lambda i: l2(q, pts[i]))[:k])

    k = 10
    hits = sum(len(top_k(q, data, k) & top_k(cipher.encrypt_query(q), enc, k)) for q in queries)
    assert hits / (len(queries) * k) > 0.9


def test_normalization_rejects_bad_params() -> None:
    for bad in (0.0, -1.0, math.nan, math.inf):
        with pytest.raises(DcpeError):
            Normalization(shift=[0.0] * 4, scale=bad)
    with pytest.raises(DcpeError):
        Normalization(shift=[math.nan] * 4, scale=1.0)


def test_normalization_dimension_mismatch_errors() -> None:
    cipher = DcpeCipher.from_hex("bb" * 32, 0.1, Normalization(shift=[0.0] * 4, scale=1.0))
    with pytest.raises(DcpeError):
        cipher.encrypt([1.0, 2.0, 3.0])
