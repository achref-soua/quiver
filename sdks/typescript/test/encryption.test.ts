// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from "vitest";

import { ENVELOPE_KEY, PayloadCipher, PayloadError, isSealed } from "../src/encryption.js";

const KEY_HEX = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

function cipher(): PayloadCipher {
  return PayloadCipher.fromHex(KEY_HEX);
}

describe("PayloadCipher", () => {
  it("round-trips seal then open", () => {
    const c = cipher();
    const plaintext = { ssn: "078-05-1120", notes: ["a", "b"], n: 42 };
    const sealed = c.seal(plaintext);
    expect(isSealed(sealed)).toBe(true);
    const env = (sealed[ENVELOPE_KEY] as Record<string, unknown>);
    expect(env.v).toBe(1);
    expect(env.alg).toBe("xchacha20poly1305");
    expect(typeof env.n).toBe("string");
    expect(typeof env.ct).toBe("string");
    expect(c.open(sealed)).toEqual(plaintext);
  });

  it("uses a fresh nonce per seal", () => {
    const c = cipher();
    const a = c.seal({ x: 1 })[ENVELOPE_KEY] as Record<string, unknown>;
    const b = c.seal({ x: 1 })[ENVELOPE_KEY] as Record<string, unknown>;
    expect(a.n).not.toBe(b.n);
    expect(a.ct).not.toBe(b.ct);
  });

  it("fails to open with the wrong key", () => {
    const sealed = cipher().seal({ secret: true });
    const wrong = PayloadCipher.fromHex("ff".repeat(32));
    expect(() => wrong.open(sealed)).toThrow(PayloadError);
  });

  it("rejects a tampered ciphertext", () => {
    const c = cipher();
    const sealed = c.seal({ secret: "value" });
    const env = sealed[ENVELOPE_KEY] as Record<string, unknown>;
    const bytes = Uint8Array.from(atob(env.ct as string), (ch) => ch.charCodeAt(0));
    const last = bytes.length - 1;
    bytes[last] = (bytes[last] ?? 0) ^ 0x01;
    env.ct = btoa(String.fromCharCode(...bytes));
    expect(() => c.open(sealed)).toThrow(PayloadError);
  });

  it("reports a cleartext value as not encrypted", () => {
    const c = cipher();
    expect(isSealed({ tier: "gold" })).toBe(false);
    expect(() => c.open({ tier: "gold" })).toThrow(PayloadError);
  });

  it("reads only the envelope, ignoring cleartext siblings", () => {
    const c = cipher();
    const payload = { tier: "gold", ...c.seal({ ssn: "078-05-1120" }) };
    expect(payload.tier).toBe("gold");
    expect(isSealed(payload)).toBe(true);
    expect(c.open(payload)).toEqual({ ssn: "078-05-1120" });
  });

  it("rejects an unknown version or algorithm", () => {
    const c = cipher();
    const sealed = c.seal({ x: 1 });
    const env = sealed[ENVELOPE_KEY] as Record<string, unknown>;

    const badVersion = { [ENVELOPE_KEY]: { ...env, v: 999 } };
    expect(() => c.open(badVersion)).toThrow(PayloadError);

    const badAlg = { [ENVELOPE_KEY]: { ...env, alg: "aes-256-gcm" } };
    expect(() => c.open(badAlg)).toThrow(PayloadError);
  });

  it("rejects bad hex keys", () => {
    expect(() => PayloadCipher.fromHex("abcd")).toThrow(PayloadError);
    expect(() => PayloadCipher.fromHex("zz".repeat(32))).toThrow(PayloadError);
  });

  it("opens an envelope sealed by the Rust reference implementation", () => {
    // Cross-language known-answer test: this envelope was produced by the Rust
    // reference (`quiver_crypto::payload`) for the key and plaintext below. The
    // TypeScript SDK must decrypt it, proving the implementations share one wire
    // format (XChaCha20-Poly1305, base64, AAD `quiver/payload/v1`).
    const keyHex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const rustEnvelope = {
      [ENVELOPE_KEY]: {
        alg: "xchacha20poly1305",
        ct: "d0Jeuk4qoE1EnGO3IxUPhD1Ewefs+IqcON9+xMNJlYxEUVvr5NpXmv65gCDGT4aTaeQB7iRgDkyRT+Dh",
        n: "JL/mMdJuHHTw+enUuS2z9cvV2BOpznfm",
        v: 1,
      },
    };
    expect(PayloadCipher.fromHex(keyHex).open(rustEnvelope)).toEqual({
      ssn: "078-05-1120",
      msg: "cross-language",
    });
  });
});
