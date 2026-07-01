# SPDX-License-Identifier: AGPL-3.0-only
"""DCPE client-side vector encryption (ADR-0031), including a cross-language
known-answer test against the Rust reference (``quiver_crypto::dcpe``)."""

from __future__ import annotations

import json
import math
import random
from pathlib import Path

import pytest

from quiver.dcpe import DcpeCipher, DcpeError, EncryptedVector, Normalization

# The single canonical cross-language KAT (F-13), generated from the Rust reference
# and asserted identically by the Rust, Python, and TypeScript suites — so drift in
# any one cipher fails the build. `parents[3]` is the repo root from tests/.
_KAT = json.loads((Path(__file__).parents[3] / "kat" / "client-ciphers.json").read_text())
KAT = _KAT["dcpe"]


def test_kat_matches_the_rust_reference() -> None:
    # Decrypting the reference vector exercises the whole construction — HKDF (the
    # scale and sub-keys), the ChaCha20 CSPRNG, Box-Muller, and HMAC — proving the
    # Python port matches Rust byte-for-byte where it must (tag, scale) and within
    # the perturbation tolerance for the recovered plaintext.
    cipher = DcpeCipher.from_hex(KAT["key_hex"], KAT["beta"])
    assert abs(cipher.scale - KAT["scale"]) < 1e-12
    sealed = EncryptedVector(
        ciphertext=KAT["ciphertext"],
        iv=bytes.fromhex(KAT["iv_hex"]),
        tag=bytes.fromhex(KAT["tag_hex"]),
    )
    recovered = cipher.decrypt(sealed)
    assert len(recovered) == len(KAT["plaintext"])
    for got, want in zip(recovered, KAT["plaintext"]):
        assert abs(got - want) < KAT["plaintext_tolerance"], f"{got} vs {want}"


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
    cipher = DcpeCipher.from_hex(KAT["key_hex"], KAT["beta"])
    assert cipher._permutation(8) == KAT["permutation_d8"]
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
