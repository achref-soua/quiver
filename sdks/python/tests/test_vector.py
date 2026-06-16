# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for client-side opaque vector encryption (quiver.vector)."""

import base64

import pytest

from quiver.vector import (
    VECTOR_ENVELOPE_KEY,
    MalformedVectorEnvelopeError,
    NotEncryptedVectorError,
    VectorCipher,
    VectorDecryptError,
    is_sealed_vector,
)

KEY_HEX = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"


def cipher() -> VectorCipher:
    return VectorCipher.from_hex(KEY_HEX)


def test_seal_then_open_round_trips_bit_exactly():
    c = cipher()
    v = [0.0, 1.0, -1.0, 0.5, -0.5, 7.25, 2.5, 42.0]
    sealed = c.seal(v)
    assert is_sealed_vector(sealed)
    env = sealed[VECTOR_ENVELOPE_KEY]
    assert env["v"] == 1 and env["alg"] == "xchacha20poly1305" and env["dim"] == 8
    assert isinstance(env["n"], str) and isinstance(env["ct"], str)
    # Bit-exact: these values are exact in f32, so equality holds.
    assert c.open(sealed) == v


def test_each_seal_uses_a_fresh_nonce():
    c = cipher()
    a = c.seal([1.0, 2.0, 3.0])
    b = c.seal([1.0, 2.0, 3.0])
    assert a[VECTOR_ENVELOPE_KEY]["n"] != b[VECTOR_ENVELOPE_KEY]["n"]
    assert a[VECTOR_ENVELOPE_KEY]["ct"] != b[VECTOR_ENVELOPE_KEY]["ct"]


def test_open_with_wrong_key_fails():
    sealed = cipher().seal([1.0, 2.0])
    with pytest.raises(VectorDecryptError):
        VectorCipher.from_hex("ff" * 32).open(sealed)


def test_open_rejects_tampered_ciphertext():
    c = cipher()
    sealed = c.seal([9.0, 8.0, 7.0])
    raw = bytearray(base64.b64decode(sealed[VECTOR_ENVELOPE_KEY]["ct"]))
    raw[-1] ^= 0x01
    sealed[VECTOR_ENVELOPE_KEY]["ct"] = base64.b64encode(bytes(raw)).decode("ascii")
    with pytest.raises(VectorDecryptError):
        c.open(sealed)


def test_open_cleartext_value_reports_not_encrypted():
    c = cipher()
    assert not is_sealed_vector({"tier": "gold"})
    with pytest.raises(NotEncryptedVectorError):
        c.open({"tier": "gold"})


def test_open_reads_only_the_envelope_ignoring_cleartext_siblings():
    c = cipher()
    payload = {"tier": "gold", **c.seal([1.5, 2.5])}
    assert payload["tier"] == "gold"
    assert is_sealed_vector(payload)
    assert c.open(payload) == [1.5, 2.5]


def test_open_rejects_dimension_mismatch():
    c = cipher()
    sealed = c.seal([1.0, 2.0, 3.0])
    sealed[VECTOR_ENVELOPE_KEY]["dim"] = 4
    with pytest.raises(MalformedVectorEnvelopeError):
        c.open(sealed)


def test_open_rejects_unknown_version_and_algorithm():
    c = cipher()
    sealed = c.seal([1.0])
    bad_v = {VECTOR_ENVELOPE_KEY: {**sealed[VECTOR_ENVELOPE_KEY], "v": 999}}
    with pytest.raises(MalformedVectorEnvelopeError):
        c.open(bad_v)
    bad_alg = {VECTOR_ENVELOPE_KEY: {**sealed[VECTOR_ENVELOPE_KEY], "alg": "aes-256-gcm"}}
    with pytest.raises(MalformedVectorEnvelopeError):
        c.open(bad_alg)


def test_from_hex_rejects_bad_keys():
    with pytest.raises(ValueError):
        VectorCipher.from_hex("abcd")
    with pytest.raises(ValueError):
        VectorCipher.from_hex("zz" * 32)


def test_opens_vector_sealed_by_the_rust_reference_impl():
    """Cross-language known-answer test: this envelope was produced by the Rust
    reference (`quiver_crypto::vector`) for the key and vector below. Because the
    sealed message is raw f32 little-endian bytes (no transcendental floats), the
    recovery is **bit-exact**, a stronger interop guarantee than DCPE's."""
    key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    rust_envelope = {
        VECTOR_ENVELOPE_KEY: {
            "alg": "xchacha20poly1305",
            "ct": "8zgd/+aSyPbmk1vkIdfaGYBKr45Bv0DsPOGdDFojuCqldB3jGiguWQ==",
            "dim": 6,
            "n": "1Tt6qe+yyU87VhS4bfOpdtloq2DlFllv",
            "v": 1,
        }
    }
    recovered = VectorCipher.from_hex(key_hex).open(rust_envelope)
    assert recovered == [0.0, 1.0, -1.0, 0.5, -0.25, 3.5]
