// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side DCPE vector encryption (ADR-0031, hardened in ADR-0035) for the
//! Quiver TypeScript SDK.
//
// A faithful port of the reference cipher `quiver_crypto::dcpe` (and the Python
// `quiver.dcpe`): the **Scale-And-Perturb (SAP)** distance-comparison-preserving
// scheme of Fuchsbauer, Ghosal, Hauke & O'Neill (ePrint 2021/1666, SCN 2022). It
// encrypts embedding vectors so a Quiver server can answer approximate
// nearest-neighbour queries over the ciphertexts **without ever holding the
// plaintext vectors or the key** — Euclidean distance comparison is preserved up
// to a tunable margin.
//
// This is **cipher v2** (ADR-0035): it adds the paper's two hardening steps — a
// key-derived component **shuffle** (an exact L2 isometry, so zero recall cost)
// and an optional ordering-preserving global affine **normalisation**
// (`Normalization`). v2 is a breaking change from v1 (v1 ciphertexts are not
// v2-decryptable); the cipher is client-side, so there is no on-disk format change.
//
// This module lives at the `quiver-client/dcpe` subpath so the core client stays
// dependency-free. The primitives come from audited `@stablelib` packages, installed
// as optional peer dependencies only to use these helpers:
//
//     pnpm add @stablelib/chacha @stablelib/hkdf @stablelib/hmac @stablelib/sha256
//
//     import { DcpeCipher } from "quiver-client/dcpe";
//     const cipher = DcpeCipher.fromHex("…64 hex chars…", 0.02);
//     // encrypt vectors before upsert, and queries before search, with the same cipher:
//     const sealed = cipher.encrypt([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
//     // upsert sealed.ciphertext; later: cipher.encryptQuery(myQuery) for search.
//
// **DCPE is experimental and is _not_ semantically secure.** It leaks the
// approximate distance-comparison relation by design (that is what makes encrypted
// search work), is L2-only, and is broken by known-plaintext or strong-prior
// adversaries. It complements — does not replace — encryption at rest. Use a
// dedicated key, and encrypt and query from the same client. See ADR-0031, ADR-0035,
// and docs/security/dcpe.md.
//
// Because the ciphertext is float-valued and uses transcendental functions,
// bit-exact reproduction against the Rust reference is not guaranteed (libm ULP
// differences); interop is validated within a tolerance. The Rust module is canonical.

import { stream } from "@stablelib/chacha";
import { HKDF } from "@stablelib/hkdf";
import { hmac } from "@stablelib/hmac";
import { SHA256 } from "@stablelib/sha256";

/** DCPE initialisation-vector length in bytes (a 96-bit ChaCha20 nonce). */
export const IV_LEN = 12;
/** DCPE integrity-tag length in bytes (full HMAC-SHA256 output). */
export const TAG_LEN = 32;

const TE = new TextEncoder();
// HKDF-SHA256 `info` strings: distinct sub-keys from one master secret. The
// scale/prf/auth derivations are unchanged from v1; `shuffle` is new in v2, and
// the tag domain is bumped to v2 so a v1 ciphertext fails a v2 integrity check.
const INFO_SCALE = TE.encode("quiver/dcpe/v1/scale");
const INFO_PRF = TE.encode("quiver/dcpe/v1/prf");
const INFO_SHUFFLE = TE.encode("quiver/dcpe/v2/shuffle");
const INFO_AUTH = TE.encode("quiver/dcpe/v1/auth");
const AUTH_DOMAIN = TE.encode("quiver/dcpe/v2/tag");
const TWO_POW_53 = 2 ** 53;

/** An error from DCPE encryption, decryption, or construction. */
export class DcpeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "DcpeError";
  }
}

/** A DCPE-encrypted vector: the ciphertext (upserted and searched like any
 * vector), the IV seeding its perturbation, and an HMAC-SHA256 integrity tag. */
export interface EncryptedVector {
  ciphertext: number[];
  iv: Uint8Array;
  tag: Uint8Array;
}

/** A fixed, ordering-preserving global affine normalisation (ADR-0035).
 *
 * Maps a plaintext `m` to `(m - shift) * scale` before encryption, where `shift`
 * is a per-dimension translation and `scale` is a **single** positive scalar. Both
 * steps preserve the L2 distance-comparison ordering (a uniform shift cancels in
 * any difference; a single positive scalar scales every distance by the same
 * factor) and are invertible. Supply it once from a one-time measurement of your
 * corpus and reuse it for the data *and* the queries.
 *
 * Per-axis variance whitening (a different scale per dimension) is anisotropic,
 * re-weights the L2 distance, and so breaks the ordering — it is intentionally not
 * expressible here. See ADR-0035. */
export class Normalization {
  readonly shift: number[];
  readonly scale: number;

  private constructor(shift: number[], scale: number) {
    this.shift = shift;
    this.scale = scale;
  }

  /** Build a normalisation from a per-dimension shift and a single positive scale. */
  static create(shift: ArrayLike<number>, scale: number): Normalization {
    const shiftArr = Array.from(shift, Number);
    if (!Number.isFinite(scale) || scale <= 0 || shiftArr.some((x) => !Number.isFinite(x))) {
      throw new DcpeError("invalid normalisation: scale must be finite and > 0 and shifts finite");
    }
    return new Normalization(shiftArr, scale);
  }
}

/** A client-held DCPE key bound to one approximation factor (ADR-0031/0035).
 *
 * Construct one cipher per `(key, approximationFactor[, normalization])` and reuse
 * it; the same factor (and normalisation) must be used for the data and the queries
 * searched against it. */
export class DcpeCipher {
  readonly #scale: number;
  readonly #prfKey: Uint8Array;
  readonly #shuffleKey: Uint8Array;
  readonly #authKey: Uint8Array;
  readonly #beta: number;
  readonly #normalization: Normalization | null;

  private constructor(key: Uint8Array, approximationFactor: number, normalization: Normalization | null) {
    if (!Number.isFinite(approximationFactor) || approximationFactor < 0) {
      throw new DcpeError("approximation factor must be finite and >= 0");
    }
    // Match the Rust f32 approximation factor exactly (it is bound into the tag).
    this.#beta = Math.fround(approximationFactor);
    const salt = new Uint8Array(32);
    const scaleBytes = new HKDF(SHA256, key, salt, INFO_SCALE).expand(8);
    this.#scale = 1 + Number(leU64(scaleBytes, 0) >> 11n) / TWO_POW_53;
    this.#prfKey = new HKDF(SHA256, key, salt, INFO_PRF).expand(32);
    this.#shuffleKey = new HKDF(SHA256, key, salt, INFO_SHUFFLE).expand(32);
    this.#authKey = new HKDF(SHA256, key, salt, INFO_AUTH).expand(32);
    this.#normalization = normalization;
  }

  /** Build a cipher from a raw 256-bit (32-byte) key. */
  static fromBytes(
    key: Uint8Array,
    approximationFactor: number,
    normalization: Normalization | null = null,
  ): DcpeCipher {
    if (key.length !== 32) {
      throw new DcpeError(`DCPE key must be 32 bytes, got ${key.length}`);
    }
    return new DcpeCipher(Uint8Array.from(key), approximationFactor, normalization);
  }

  /** Build a cipher from a 64-character hex-encoded 256-bit key. */
  static fromHex(
    hex: string,
    approximationFactor: number,
    normalization: Normalization | null = null,
  ): DcpeCipher {
    const clean = hex.trim();
    if (!/^[0-9a-fA-F]{64}$/.test(clean)) {
      throw new DcpeError(`DCPE key must be 64 hex characters, got ${clean.length}`);
    }
    const key = new Uint8Array(32);
    for (let i = 0; i < 32; i++) {
      key[i] = Number.parseInt(clean.slice(i * 2, i * 2 + 2), 16);
    }
    return new DcpeCipher(key, approximationFactor, normalization);
  }

  /** The secret, key-derived scaling factor `s ∈ [1, 2)`. Part of the key. */
  get scale(): number {
    return this.#scale;
  }

  /** The approximation factor this cipher was built with. */
  get approximationFactor(): number {
    return this.#beta;
  }

  /** Encrypt a vector for storage with a fresh random IV. */
  encrypt(vector: ArrayLike<number>): EncryptedVector {
    if (vector.length === 0) {
      throw new DcpeError("empty vector: DCPE needs at least one dimension");
    }
    const pre = this.#pretransform(vector);
    const iv = randomBytes(IV_LEN);
    const ciphertext = this.#scaleAndPerturb(pre, iv);
    return { ciphertext, iv, tag: this.#tag(iv, ciphertext) };
  }

  /** Encrypt a query vector for searching against DCPE-encrypted data. */
  encryptQuery(vector: ArrayLike<number>): number[] {
    if (vector.length === 0) {
      throw new DcpeError("empty vector: DCPE needs at least one dimension");
    }
    return this.#scaleAndPerturb(this.#pretransform(vector), randomBytes(IV_LEN));
  }

  /** Verify the integrity tag (constant-time) and recover the plaintext. */
  decrypt(sealed: EncryptedVector): number[] {
    if (sealed.ciphertext.length === 0) {
      throw new DcpeError("empty vector: DCPE needs at least one dimension");
    }
    const expected = this.#tag(sealed.iv, sealed.ciphertext);
    if (!constantTimeEqual(expected, sealed.tag)) {
      throw new DcpeError("integrity check failed: wrong key or tampered ciphertext");
    }
    const lambda = this.#perturbation(sealed.iv, sealed.ciphertext.length);
    // Recover the shuffled, normalised vector (c - lambda)/s, then reverse the
    // pipeline: un-shuffle, then un-normalise.
    const shuffled = sealed.ciphertext.map((c, i) => (c - lambda[i]!) / this.#scale);
    return this.#denormalize(this.#unshuffle(shuffled));
  }

  // --- internals (match the Rust/Python references byte-for-byte) ---

  #pretransform(vector: ArrayLike<number>): number[] {
    const normalized = this.#normalize(vector);
    const perm = this.#permutation(vector.length);
    return perm.map((p) => normalized[p]!);
  }

  #normalize(vector: ArrayLike<number>): number[] {
    const n = this.#normalization;
    if (n === null) {
      return Array.from(vector, Number);
    }
    if (n.shift.length !== vector.length) {
      throw new DcpeError(
        `dimension mismatch: vector has ${vector.length} dims, normalisation has ${n.shift.length}`,
      );
    }
    return Array.from(vector, (m, i) => (Number(m) - n.shift[i]!) * n.scale);
  }

  #unshuffle(shuffled: number[]): number[] {
    const perm = this.#permutation(shuffled.length);
    const out = new Array<number>(shuffled.length).fill(0);
    for (let i = 0; i < perm.length; i++) {
      out[perm[i]!] = shuffled[i]!;
    }
    return out;
  }

  #denormalize(normalized: number[]): number[] {
    const n = this.#normalization;
    if (n === null) {
      return normalized.map((x) => Math.fround(x));
    }
    if (n.shift.length !== normalized.length) {
      throw new DcpeError(
        `dimension mismatch: vector has ${normalized.length} dims, normalisation has ${n.shift.length}`,
      );
    }
    return normalized.map((x, i) => Math.fround(x / n.scale + n.shift[i]!));
  }

  // The key-derived permutation of [0, d) (Fisher-Yates from the top over the
  // shuffle keystream with a fixed zero IV), identical for every vector and query
  // so all pairwise L2 distances are preserved. The `% (i + 1)` reduction's modulo
  // bias is cryptographically negligible and is fixed for cross-language parity.
  #permutation(d: number): number[] {
    const perm = Array.from({ length: d }, (_, i) => i);
    if (d <= 1) {
      return perm;
    }
    const ks = new KeyStream(this.#shuffleKey, new Uint8Array(IV_LEN), d * 8);
    for (let i = d - 1; i >= 1; i--) {
      const j = Number(ks.nextU64() % BigInt(i + 1));
      const tmp = perm[i]!;
      perm[i] = perm[j]!;
      perm[j] = tmp;
    }
    return perm;
  }

  // Compute c = s·x + λ (f64), stored as f32. `x` is the normalised+shuffled vector.
  #scaleAndPerturb(x: number[], iv: Uint8Array): number[] {
    const lambda = this.#perturbation(iv, x.length);
    return x.map((m, i) => Math.fround(this.#scale * m + lambda[i]!));
  }

  // The perturbation λ: a uniform point in the d-ball of radius (s/4)·β. The CSPRNG
  // draws the d normal components first, then one uniform for the radius.
  #perturbation(iv: Uint8Array, d: number): number[] {
    const ks = new KeyStream(this.#prfKey, iv, (d + 4) * 8);
    const direction = Array.from({ length: d }, () => ks.nextNormal());
    let norm = 0;
    for (const v of direction) {
      norm += v * v;
    }
    norm = Math.sqrt(norm);
    const u = ks.nextUnit();
    const radius = (this.#scale / 4) * this.#beta * Math.pow(u, 1 / d);
    if (norm === 0) {
      return new Array<number>(d).fill(0);
    }
    return direction.map((v) => (v / norm) * radius);
  }

  // HMAC-SHA256 over (domain ‖ β as f32 LE ‖ iv ‖ ciphertext as f32 LE).
  #tag(iv: Uint8Array, ciphertext: ArrayLike<number>): Uint8Array {
    const d = ciphertext.length;
    const msg = new Uint8Array(AUTH_DOMAIN.length + 4 + IV_LEN + 4 * d);
    const dv = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
    let off = 0;
    msg.set(AUTH_DOMAIN, off);
    off += AUTH_DOMAIN.length;
    dv.setFloat32(off, this.#beta, true);
    off += 4;
    msg.set(iv, off);
    off += IV_LEN;
    for (let i = 0; i < d; i++) {
      dv.setFloat32(off, ciphertext[i]!, true);
      off += 4;
    }
    return hmac(SHA256, this.#authKey, msg);
  }
}

/** A deterministic CSPRNG: the raw ChaCha20 keystream from `(key, iv)`, read as
 * little-endian u64s. Standard normals come from Box-Muller, caching the paired
 * value. The layout matches the Rust/Python references byte-for-byte. The keystream
 * is materialised up front to `capacity` bytes (sized to the caller's exact need). */
class KeyStream {
  readonly #buf: Uint8Array;
  #pos = 0;
  #spare: number | null = null;

  constructor(key: Uint8Array, iv: Uint8Array, capacity: number) {
    this.#buf = new Uint8Array(capacity);
    stream(key, iv, this.#buf);
  }

  nextU64(): bigint {
    if (this.#pos + 8 > this.#buf.length) {
      throw new DcpeError("DCPE keystream exhausted");
    }
    const w = leU64(this.#buf, this.#pos);
    this.#pos += 8;
    return w;
  }

  // A uniform in [0, 1) with 53-bit resolution (the f64 mantissa width).
  nextUnit(): number {
    return Number(this.nextU64() >> 11n) / TWO_POW_53;
  }

  // A standard normal via Box-Muller; u1 ∈ (0, 1] so log is finite.
  nextNormal(): number {
    if (this.#spare !== null) {
      const z = this.#spare;
      this.#spare = null;
      return z;
    }
    const u1 = 1 - this.nextUnit();
    const u2 = this.nextUnit();
    const r = Math.sqrt(-2 * Math.log(u1));
    const theta = 2 * Math.PI * u2;
    this.#spare = r * Math.sin(theta);
    return r * Math.cos(theta);
  }
}

/** Read 8 bytes of `buf` at `off` as a little-endian u64. */
function leU64(buf: Uint8Array, off: number): bigint {
  let w = 0n;
  for (let i = 7; i >= 0; i--) {
    w = (w << 8n) | BigInt(buf[off + i]!);
  }
  return w;
}

/** A constant-time byte-string comparison (not a crypto primitive; a comparison). */
function constantTimeEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) {
    return false;
  }
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a[i]! ^ b[i]!;
  }
  return diff === 0;
}

function randomBytes(length: number): Uint8Array {
  const webcrypto = globalThis.crypto;
  if (!webcrypto?.getRandomValues) {
    throw new DcpeError("a Web Crypto getRandomValues implementation is required");
  }
  return webcrypto.getRandomValues(new Uint8Array(length));
}
