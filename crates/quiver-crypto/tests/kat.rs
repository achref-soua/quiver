// SPDX-License-Identifier: AGPL-3.0-only
//! Cross-language known-answer tests (F-13): the client ciphers must reproduce the
//! canonical vectors in `kat/client-ciphers.json`. The Python and TypeScript SDK
//! suites assert the SAME file, so a cipher change not mirrored across all three
//! languages fails the build. This Rust side is the reference: if the file drifts
//! from what the Rust cipher produces, this test fails.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use quiver_crypto::{DcpeCipher, EncryptedVector, VectorCipher};
use serde_json::Value;

// Embedded at compile time; path is relative to this source file (repo-root `kat/`).
const KAT: &str = include_str!("../../../kat/client-ciphers.json");

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn f32s(v: &Value) -> Vec<f32> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap() as f32)
        .collect()
}

#[test]
fn dcpe_kat_matches_the_reference() {
    let kat: Value = serde_json::from_str(KAT).unwrap();
    let d = &kat["dcpe"];
    let cipher =
        DcpeCipher::from_hex(d["key_hex"].as_str().unwrap(), d["beta"].as_f64().unwrap() as f32)
            .unwrap();
    // The HKDF-derived scale is byte-exact across languages.
    assert!((cipher.scale() - d["scale"].as_f64().unwrap()).abs() < 1e-12);

    let sealed = EncryptedVector {
        ciphertext: f32s(&d["ciphertext"]),
        iv: hex_to_bytes(d["iv_hex"].as_str().unwrap())
            .try_into()
            .unwrap(),
        tag: hex_to_bytes(d["tag_hex"].as_str().unwrap())
            .try_into()
            .unwrap(),
    };
    // The tag must verify exactly (HKDF + HMAC), and the plaintext must come back
    // within the documented perturbation tolerance.
    let recovered = cipher.decrypt(&sealed).expect("tag verifies and decrypts");
    let plain = d["plaintext"].as_array().unwrap();
    let tol = d["plaintext_tolerance"].as_f64().unwrap();
    assert_eq!(recovered.len(), plain.len());
    for (got, want) in recovered.iter().zip(plain) {
        assert!(
            (f64::from(*got) - want.as_f64().unwrap()).abs() < tol,
            "{got} vs {want}"
        );
    }
}

#[test]
fn opaque_vector_kat_matches_the_reference() {
    let kat: Value = serde_json::from_str(KAT).unwrap();
    let o = &kat["opaque_vector"];
    let cipher = VectorCipher::from_hex(o["key_hex"].as_str().unwrap()).unwrap();
    // Exact interop: the sealed message is raw f32 LE bytes, so decrypt is byte-exact.
    assert_eq!(cipher.open(&o["envelope"]).expect("open kat"), f32s(&o["plaintext"]));
}
