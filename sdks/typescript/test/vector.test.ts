// SPDX-License-Identifier: AGPL-3.0-only
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import {
  VECTOR_ENVELOPE_KEY,
  VectorCipher,
  VectorError,
  isSealedVector,
} from "../src/vector.js";

const KEY_HEX = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

function cipher(): VectorCipher {
  return VectorCipher.fromHex(KEY_HEX);
}

describe("VectorCipher", () => {
  it("decrypts the canonical cross-language KAT envelope (F-13)", () => {
    // The single shared KAT, asserted identically by the Rust and Python suites:
    // decrypting the Rust reference envelope proves byte-exact interop (ADR-0032).
    const kat = JSON.parse(
      readFileSync(
        join(dirname(fileURLToPath(import.meta.url)), "../../../kat/client-ciphers.json"),
        "utf8",
      ),
    ).opaque_vector as { key_hex: string; envelope: Record<string, unknown>; plaintext: number[] };
    expect(VectorCipher.fromHex(kat.key_hex).open(kat.envelope)).toEqual(kat.plaintext);
  });

  it("round-trips seal then open bit-exactly", () => {
    const c = cipher();
    const v = [0.0, 1.0, -1.0, 0.5, -0.5, 7.25, 2.5, 42.0];
    const sealed = c.seal(v);
    expect(isSealedVector(sealed)).toBe(true);
    const env = sealed[VECTOR_ENVELOPE_KEY] as Record<string, unknown>;
    expect(env.v).toBe(1);
    expect(env.alg).toBe("xchacha20poly1305");
    expect(env.dim).toBe(8);
    expect(typeof env.n).toBe("string");
    expect(typeof env.ct).toBe("string");
    expect(c.open(sealed)).toEqual(v);
  });

  it("uses a fresh nonce per seal", () => {
    const c = cipher();
    const a = c.seal([1, 2, 3])[VECTOR_ENVELOPE_KEY] as Record<string, unknown>;
    const b = c.seal([1, 2, 3])[VECTOR_ENVELOPE_KEY] as Record<string, unknown>;
    expect(a.n).not.toBe(b.n);
    expect(a.ct).not.toBe(b.ct);
  });

  it("fails to open with the wrong key", () => {
    const sealed = cipher().seal([1, 2]);
    expect(() => VectorCipher.fromHex("ff".repeat(32)).open(sealed)).toThrow(VectorError);
  });

  it("rejects a tampered ciphertext", () => {
    const c = cipher();
    const sealed = c.seal([9, 8, 7]);
    const env = sealed[VECTOR_ENVELOPE_KEY] as Record<string, unknown>;
    const bytes = Uint8Array.from(atob(env.ct as string), (ch) => ch.charCodeAt(0));
    const last = bytes.length - 1;
    bytes[last] = (bytes[last] ?? 0) ^ 0x01;
    env.ct = btoa(String.fromCharCode(...bytes));
    expect(() => c.open(sealed)).toThrow(VectorError);
  });

  it("reports a cleartext value as not encrypted", () => {
    const c = cipher();
    expect(isSealedVector({ tier: "gold" })).toBe(false);
    expect(() => c.open({ tier: "gold" })).toThrow(VectorError);
  });

  it("reads only the envelope, ignoring cleartext siblings", () => {
    const c = cipher();
    const payload = { tier: "gold", ...c.seal([1.5, 2.5]) };
    expect(payload.tier).toBe("gold");
    expect(isSealedVector(payload)).toBe(true);
    expect(c.open(payload)).toEqual([1.5, 2.5]);
  });

  it("rejects a dimension mismatch", () => {
    const c = cipher();
    const sealed = c.seal([1, 2, 3]);
    (sealed[VECTOR_ENVELOPE_KEY] as Record<string, unknown>).dim = 4;
    expect(() => c.open(sealed)).toThrow(VectorError);
  });

  it("rejects an unknown version or algorithm", () => {
    const c = cipher();
    const env = c.seal([1])[VECTOR_ENVELOPE_KEY] as Record<string, unknown>;
    expect(() => c.open({ [VECTOR_ENVELOPE_KEY]: { ...env, v: 999 } })).toThrow(VectorError);
    expect(() => c.open({ [VECTOR_ENVELOPE_KEY]: { ...env, alg: "aes-256-gcm" } })).toThrow(
      VectorError,
    );
  });

  it("rejects bad hex keys", () => {
    expect(() => VectorCipher.fromHex("abcd")).toThrow(VectorError);
    expect(() => VectorCipher.fromHex("zz".repeat(32))).toThrow(VectorError);
  });

  it("opens a vector sealed by the Rust reference implementation", () => {
    // Cross-language known-answer test: this envelope was produced by the Rust
    // reference (`quiver_crypto::vector`). Because the sealed message is raw f32
    // little-endian bytes, the recovery is bit-exact.
    const keyHex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const rustEnvelope = {
      [VECTOR_ENVELOPE_KEY]: {
        alg: "xchacha20poly1305",
        ct: "8zgd/+aSyPbmk1vkIdfaGYBKr45Bv0DsPOGdDFojuCqldB3jGiguWQ==",
        dim: 6,
        n: "1Tt6qe+yyU87VhS4bfOpdtloq2DlFllv",
        v: 1,
      },
    };
    expect(VectorCipher.fromHex(keyHex).open(rustEnvelope)).toEqual([
      0.0, 1.0, -1.0, 0.5, -0.25, 3.5,
    ]);
  });
});
