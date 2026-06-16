// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side opaque vector encryption (ADR-0032) for the Quiver TypeScript SDK.
//
// Mirrors `quiver_crypto::vector` byte-for-byte: seals a vector's raw
// little-endian f32 bytes with XChaCha20-Poly1305 (the audited
// `@stablelib/xchacha20poly1305`, interoperating with the Rust and Python
// implementations) under the reserved `__quiver_vec__` key. The server stores the
// blob (in the payload) plus a zero placeholder vector, does no distance math, and
// never sees the key; the client fetches the entitled set, decrypts, and ranks
// locally. It is the semantically secure end of Quiver's encrypted-search
// spectrum: unlike DCPE it leaks nothing about the vectors. Because the sealed
// message is raw bytes, interop with the Rust/Python impls is bit-exact.
//
// This module lives at the `quiver-client/vector` subpath so the core client
// stays dependency-free; `@stablelib/xchacha20poly1305` is an optional peer
// dependency you install only to use these helpers:
//
//     pnpm add @stablelib/xchacha20poly1305
//
//     import { VectorCipher } from "quiver-client/vector";
//     const cipher = VectorCipher.fromHex("…64 hex chars…");
//     const payload = { tier: "gold", ...cipher.seal([0.1, -0.2, 0.3, 0.4]) };
//     // ... upsert with a zero placeholder vector + this payload; later:
//     const vector = cipher.open(payload); // -> [0.1, -0.2, 0.3, 0.4] (bit-exact)
//
// Use a dedicated key for vector encryption; never reuse your at-rest
// `QUIVER_ENCRYPTION_KEY`. The client owns the key — losing it means the vectors
// are unrecoverable.

import { XChaCha20Poly1305 } from "@stablelib/xchacha20poly1305";

/** The reserved payload key under which a sealed vector envelope is stored. */
export const VECTOR_ENVELOPE_KEY = "__quiver_vec__";

const VERSION = 1;
const ALG = "xchacha20poly1305";
const NONCE_LEN = 24;
const TAG_LEN = 16;
const AAD = new TextEncoder().encode("quiver/vector/v1");

/** An error sealing or opening a client-side vector envelope. */
export class VectorError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "VectorError";
  }
}

/** A client-held key for sealing and opening vector envelopes (ADR-0032). */
export class VectorCipher {
  readonly #key: Uint8Array;

  private constructor(key: Uint8Array) {
    this.#key = key;
  }

  /** Build a cipher from a raw 256-bit (32-byte) key. */
  static fromBytes(key: Uint8Array): VectorCipher {
    if (key.length !== 32) {
      throw new VectorError(`vector key must be 32 bytes, got ${key.length}`);
    }
    return new VectorCipher(Uint8Array.from(key));
  }

  /** Build a cipher from a 64-character hex-encoded 256-bit key. */
  static fromHex(hex: string): VectorCipher {
    const clean = hex.trim();
    if (!/^[0-9a-fA-F]{64}$/.test(clean)) {
      throw new VectorError(`vector key must be 64 hex characters, got ${clean.length}`);
    }
    const key = new Uint8Array(32);
    for (let i = 0; i < 32; i++) {
      key[i] = Number.parseInt(clean.slice(i * 2, i * 2 + 2), 16);
    }
    return new VectorCipher(key);
  }

  /** Seal `vector` into a one-key envelope `{ [VECTOR_ENVELOPE_KEY]: { … } }`. Each
   * call uses a fresh random nonce, so the same vector seals to different
   * ciphertext. The vector's f32 components are written little-endian to match the
   * Rust reference regardless of host endianness. */
  seal(vector: ArrayLike<number>): Record<string, unknown> {
    const dim = vector.length;
    const buffer = new ArrayBuffer(dim * 4);
    const view = new DataView(buffer);
    for (let i = 0; i < dim; i++) {
      view.setFloat32(i * 4, Number(vector[i]), true);
    }
    const nonce = randomBytes(NONCE_LEN);
    const ciphertext = new XChaCha20Poly1305(this.#key).seal(
      nonce,
      new Uint8Array(buffer),
      AAD,
    );
    return {
      [VECTOR_ENVELOPE_KEY]: {
        v: VERSION,
        alg: ALG,
        dim,
        n: toBase64(nonce),
        ct: toBase64(ciphertext),
      },
    };
  }

  /** Open an envelope sealed by {@link seal}, returning the vector. `sealed` may
   * carry cleartext sibling fields; only the reserved key is read. A wrong key or
   * any tampering throws {@link VectorError}. */
  open(sealed: unknown): number[] {
    if (!isSealedVector(sealed)) {
      throw new VectorError("value is not a quiver-encrypted vector envelope");
    }
    const envelope = (sealed as Record<string, unknown>)[VECTOR_ENVELOPE_KEY];
    if (typeof envelope !== "object" || envelope === null) {
      throw new VectorError("envelope is not an object");
    }
    const env = envelope as Record<string, unknown>;
    if (env.v !== VERSION) {
      throw new VectorError(`unsupported envelope version: ${String(env.v)}`);
    }
    if (env.alg !== ALG) {
      throw new VectorError(`unsupported envelope algorithm: ${String(env.alg)}`);
    }
    const dim = env.dim;
    if (typeof dim !== "number" || !Number.isInteger(dim) || dim < 0) {
      throw new VectorError(`missing or invalid dim: ${String(dim)}`);
    }
    const nonce = decodeField(env, "n");
    if (nonce.length !== NONCE_LEN) {
      throw new VectorError(`nonce is ${nonce.length} bytes, expected ${NONCE_LEN}`);
    }
    const ciphertext = decodeField(env, "ct");
    if (ciphertext.length < TAG_LEN) {
      throw new VectorError(
        `ciphertext is ${ciphertext.length} bytes, shorter than the ${TAG_LEN}-byte tag`,
      );
    }
    const message = new XChaCha20Poly1305(this.#key).open(nonce, ciphertext, AAD);
    if (message === null) {
      throw new VectorError("wrong key or tampered ciphertext");
    }
    if (message.length !== dim * 4) {
      throw new VectorError(`decrypted ${message.length} bytes, expected ${dim * 4} for dim ${dim}`);
    }
    const view = new DataView(message.buffer, message.byteOffset, message.byteLength);
    const out: number[] = [];
    for (let i = 0; i < dim; i++) {
      out.push(view.getFloat32(i * 4, true));
    }
    return out;
  }
}

/** Whether `value` carries a Quiver vector envelope. */
export function isSealedVector(value: unknown): boolean {
  return typeof value === "object" && value !== null && VECTOR_ENVELOPE_KEY in value;
}

function decodeField(envelope: Record<string, unknown>, field: string): Uint8Array {
  const raw = envelope[field];
  if (typeof raw !== "string") {
    throw new VectorError(`missing envelope field '${field}'`);
  }
  return fromBase64(raw);
}

function randomBytes(length: number): Uint8Array {
  const webcrypto = globalThis.crypto;
  if (!webcrypto?.getRandomValues) {
    throw new VectorError("a Web Crypto getRandomValues implementation is required");
  }
  return webcrypto.getRandomValues(new Uint8Array(length));
}

function toBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

function fromBase64(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
