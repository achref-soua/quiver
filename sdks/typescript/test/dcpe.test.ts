// SPDX-License-Identifier: AGPL-3.0-only
import { describe, expect, it } from "vitest";

import { DcpeCipher, DcpeError, Normalization } from "../src/dcpe.js";

// A known-answer vector produced by the Rust reference (`quiver_crypto::dcpe`,
// cipher v2). Decrypting it exercises the whole construction — HKDF (the scale and
// sub-keys), the ChaCha20 CSPRNG, the key-derived shuffle, Box-Muller, and HMAC —
// proving the TS port matches Rust. The tag verifies bit-exact (HKDF + HMAC); the
// plaintext is recovered within float tolerance (ChaCha20 + Box-Muller).
const KAT_KEY = "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f";
const KAT_BETA = 0.05;
const KAT_SCALE = 1.95453267099551331;
const KAT_IV = "112233445566778899aabbcc";
const KAT_PLAIN = [0.1, -0.2, 0.3, -0.4, 0.5, 0.6, -0.7, 0.8];
// v2: the ciphertext is the *shuffled* plaintext, scale-and-perturbed; the
// key-derived permutation for this key at d=8 is [2, 6, 1, 5, 7, 0, 4, 3].
const KAT_CT = [
  0.5790184, -1.3649843, -0.3800147, 1.1816978, 1.5671049, 0.18977723, 0.98995024, -0.7886901,
];
const KAT_TAG = "0e37dacb37dd8b1bc6f2f2eced612fc66e9dd2ca1efe859817328680454ba176";

function hexToBytes(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

function l2(a: number[], b: number[]): number {
  return a.reduce((s, x, i) => s + (x - b[i]!) ** 2, 0);
}

function topK(q: number[], pts: number[][], k: number): Set<number> {
  return new Set(
    pts
      .map((_, i) => i)
      .sort((i, j) => l2(q, pts[i]!) - l2(q, pts[j]!))
      .slice(0, k),
  );
}

// A small deterministic PRNG (mulberry32) so tests need no randomness source.
function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function dataset(n: number, d: number, seed: number): number[][] {
  const r = rng(seed);
  return Array.from({ length: n }, () => Array.from({ length: d }, () => r() - 0.5));
}

describe("DcpeCipher", () => {
  it("decrypts a vector sealed by the Rust reference (cross-language KAT)", () => {
    const cipher = DcpeCipher.fromHex(KAT_KEY, KAT_BETA);
    expect(Math.abs(cipher.scale - KAT_SCALE)).toBeLessThan(1e-12);
    const recovered = cipher.decrypt({
      ciphertext: KAT_CT,
      iv: hexToBytes(KAT_IV),
      tag: hexToBytes(KAT_TAG),
    });
    expect(recovered.length).toBe(KAT_PLAIN.length);
    recovered.forEach((got, i) => {
      expect(Math.abs(got - KAT_PLAIN[i]!)).toBeLessThan(1e-3);
    });
  });

  it("round-trips encrypt then decrypt", () => {
    const cipher = DcpeCipher.fromHex("11".repeat(32), 0.1);
    const plain = [0.5, -0.25, 0.125, 0.0, 0.9, -0.9, 0.33, -0.66];
    const recovered = cipher.decrypt(cipher.encrypt(plain));
    recovered.forEach((got, i) => expect(Math.abs(got - plain[i]!)).toBeLessThan(1e-3));
  });

  it("uses a fresh IV per encryption", () => {
    const cipher = DcpeCipher.fromHex("22".repeat(32), 0.1);
    const a = cipher.encrypt([0.1, 0.2, 0.3, 0.4]);
    const b = cipher.encrypt([0.1, 0.2, 0.3, 0.4]);
    expect(a.iv).not.toEqual(b.iv);
    expect(a.ciphertext).not.toEqual(b.ciphertext);
  });

  it("fails integrity on the wrong key or a tampered ciphertext", () => {
    const cipher = DcpeCipher.fromHex("33".repeat(32), 0.1);
    const sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4]);
    expect(() => DcpeCipher.fromHex("44".repeat(32), 0.1).decrypt(sealed)).toThrow(DcpeError);
    const tampered = { ...sealed, ciphertext: sealed.ciphertext.map((c) => c + 0.5) };
    expect(() => cipher.decrypt(tampered)).toThrow(DcpeError);
  });

  it("applies the component shuffle (visible at beta = 0) and still round-trips", () => {
    // At β = 0 there is no perturbation, so the ciphertext is exactly s·π(m): a
    // permutation of the un-shuffled s·m, hence not equal to it — yet it decrypts.
    const cipher = DcpeCipher.fromHex("55".repeat(32), 0);
    const plain = Array.from({ length: 16 }, (_, i) => i);
    const sealed = cipher.encrypt(plain);
    const naive = plain.map((m) => Math.fround(cipher.scale * m));
    expect(sealed.ciphertext).not.toEqual(naive);
    expect([...sealed.ciphertext].sort((a, b) => a - b)).toEqual([...naive].sort((a, b) => a - b));
    cipher.decrypt(sealed).forEach((got, i) => expect(Math.abs(got - plain[i]!)).toBeLessThan(1e-3));
  });

  it("preserves nearest neighbours at a small beta", () => {
    const data = dataset(300, 32, 1);
    const queries = dataset(15, 32, 999);
    const cipher = DcpeCipher.fromHex("66".repeat(32), 0.02);
    const enc = data.map((v) => cipher.encrypt(v).ciphertext);
    const k = 10;
    let hits = 0;
    for (const q of queries) {
      const truth = topK(q, data, k);
      const got = topK(cipher.encryptQuery(q), enc, k);
      hits += [...got].filter((i) => truth.has(i)).length;
    }
    expect(hits / (queries.length * k)).toBeGreaterThan(0.9);
  });

  it("round-trips with a global normalisation", () => {
    const norm = Normalization.create([0.5, -0.5, 1.0, 0.0, 2.0, -1.0, 0.25, -0.25], 3.0);
    const cipher = DcpeCipher.fromHex("77".repeat(32), 0.1, norm);
    const plain = [0.1, -0.2, 0.3, -0.4, 0.5, 0.6, -0.7, 0.8];
    cipher.decrypt(cipher.encrypt(plain)).forEach((got, i) => {
      expect(Math.abs(got - plain[i]!)).toBeLessThan(1e-3);
    });
  });

  it("preserves nearest neighbours under normalisation", () => {
    const data = dataset(200, 16, 3);
    const queries = dataset(10, 16, 99);
    const norm = Normalization.create(
      Array.from({ length: 16 }, (_, i) => 0.1 * i),
      5.0,
    );
    const cipher = DcpeCipher.fromHex("88".repeat(32), 0.02, norm);
    const enc = data.map((v) => cipher.encrypt(v).ciphertext);
    const k = 10;
    let hits = 0;
    for (const q of queries) {
      hits += [...topK(cipher.encryptQuery(q), enc, k)].filter((i) => topK(q, data, k).has(i)).length;
    }
    expect(hits / (queries.length * k)).toBeGreaterThan(0.9);
  });

  it("rejects invalid normalisation parameters", () => {
    for (const bad of [0, -1, NaN, Infinity]) {
      expect(() => Normalization.create([0, 0, 0, 0], bad)).toThrow(DcpeError);
    }
    expect(() => Normalization.create([NaN, 0, 0, 0], 1)).toThrow(DcpeError);
  });

  it("errors on a normalisation dimension mismatch", () => {
    const cipher = DcpeCipher.fromHex("99".repeat(32), 0.1, Normalization.create([0, 0, 0, 0], 1));
    expect(() => cipher.encrypt([1, 2, 3])).toThrow(DcpeError);
  });

  it("rejects bad keys, invalid factors, and empty vectors", () => {
    expect(() => DcpeCipher.fromHex("abcd", 0.1)).toThrow(DcpeError);
    expect(() => DcpeCipher.fromHex("zz".repeat(32), 0.1)).toThrow(DcpeError);
    for (const bad of [-0.1, NaN, Infinity]) {
      expect(() => DcpeCipher.fromHex("aa".repeat(32), bad)).toThrow(DcpeError);
    }
    expect(() => DcpeCipher.fromHex("bb".repeat(32), 0.1).encrypt([])).toThrow(DcpeError);
  });
});
