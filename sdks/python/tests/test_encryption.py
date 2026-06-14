# SPDX-License-Identifier: AGPL-3.0-only
"""Tests for client-side payload encryption (quiver.encryption)."""

import pytest

from quiver.encryption import (
    ENVELOPE_KEY,
    DecryptError,
    MalformedEnvelopeError,
    NotEncryptedError,
    PayloadCipher,
    is_sealed,
)

KEY_HEX = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"


def cipher() -> PayloadCipher:
    return PayloadCipher.from_hex(KEY_HEX)


def test_seal_then_open_round_trips():
    c = cipher()
    plaintext = {"ssn": "078-05-1120", "notes": ["a", "b"], "n": 42}
    sealed = c.seal(plaintext)
    assert is_sealed(sealed)
    env = sealed[ENVELOPE_KEY]
    assert env["v"] == 1 and env["alg"] == "xchacha20poly1305"
    assert isinstance(env["n"], str) and isinstance(env["ct"], str)
    assert c.open(sealed) == plaintext


def test_each_seal_uses_a_fresh_nonce():
    c = cipher()
    a = c.seal({"x": 1})
    b = c.seal({"x": 1})
    assert a[ENVELOPE_KEY]["n"] != b[ENVELOPE_KEY]["n"]
    assert a[ENVELOPE_KEY]["ct"] != b[ENVELOPE_KEY]["ct"]


def test_open_with_wrong_key_fails():
    sealed = cipher().seal({"secret": True})
    wrong = PayloadCipher.from_hex("ff" * 32)
    with pytest.raises(DecryptError):
        wrong.open(sealed)


def test_open_rejects_tampered_ciphertext():
    import base64

    c = cipher()
    sealed = c.seal({"secret": "value"})
    raw = bytearray(base64.b64decode(sealed[ENVELOPE_KEY]["ct"]))
    raw[-1] ^= 0x01
    sealed[ENVELOPE_KEY]["ct"] = base64.b64encode(bytes(raw)).decode("ascii")
    with pytest.raises(DecryptError):
        c.open(sealed)


def test_open_cleartext_value_reports_not_encrypted():
    c = cipher()
    assert not is_sealed({"tier": "gold"})
    with pytest.raises(NotEncryptedError):
        c.open({"tier": "gold"})


def test_open_reads_only_the_envelope_ignoring_cleartext_siblings():
    c = cipher()
    payload = {"tier": "gold", **c.seal({"ssn": "078-05-1120"})}
    assert payload["tier"] == "gold"
    assert is_sealed(payload)
    assert c.open(payload) == {"ssn": "078-05-1120"}


def test_open_rejects_unknown_version_and_algorithm():
    c = cipher()
    sealed = c.seal({"x": 1})
    bad_version = {ENVELOPE_KEY: {**sealed[ENVELOPE_KEY], "v": 999}}
    with pytest.raises(MalformedEnvelopeError):
        c.open(bad_version)
    bad_alg = {ENVELOPE_KEY: {**sealed[ENVELOPE_KEY], "alg": "aes-256-gcm"}}
    with pytest.raises(MalformedEnvelopeError):
        c.open(bad_alg)


def test_from_hex_rejects_bad_keys():
    with pytest.raises(ValueError):
        PayloadCipher.from_hex("abcd")
    with pytest.raises(ValueError):
        PayloadCipher.from_hex("zz" * 32)


def test_opens_envelope_sealed_by_the_rust_reference_impl():
    """Cross-language known-answer test: this envelope was produced by the Rust
    reference (`quiver_crypto::payload`) for the key and plaintext below. The
    Python SDK must decrypt it, proving the two implementations share one wire
    format (XChaCha20-Poly1305, base64, AAD `quiver/payload/v1`)."""
    key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    rust_envelope = {
        ENVELOPE_KEY: {
            "alg": "xchacha20poly1305",
            "ct": "d0Jeuk4qoE1EnGO3IxUPhD1Ewefs+IqcON9+xMNJlYxEUVvr5NpXmv65gCDGT4aTaeQB7iRgDkyRT+Dh",
            "n": "JL/mMdJuHHTw+enUuS2z9cvV2BOpznfm",
            "v": 1,
        }
    }
    recovered = PayloadCipher.from_hex(key_hex).open(rust_envelope)
    assert recovered == {"ssn": "078-05-1120", "msg": "cross-language"}
