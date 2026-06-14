// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side payload encryption (ADR-0012) for the Quiver TypeScript SDK.
//
// Mirrors the reference envelope in `quiver_crypto::payload` byte-for-byte: a
// caller seals a JSON payload with a 256-bit key Quiver never sees, and the
// server stores and returns it as an opaque blob it cannot read. Sealing uses
// XChaCha20-Poly1305 (the audited `@stablelib/xchacha20poly1305`, which
// interoperates with the Rust and Python implementations) with a fresh random
// 192-bit nonce.
//
// This module lives at the `quiver-client/encryption` subpath so the core
// client stays dependency-free; `@stablelib/xchacha20poly1305` is an optional
// peer dependency you install only to use these helpers:
//
//     pnpm add @stablelib/xchacha20poly1305
//
// Keep fields server-filterable by leaving them in cleartext and merging the
// sealed envelope alongside them — `open` reads only the reserved key:
//
//     import { PayloadCipher } from "quiver-client/encryption";
//     const cipher = PayloadCipher.fromHex("…64 hex chars…");
//     const payload = { tier: "gold", ...cipher.seal({ ssn: "078-05-1120" }) };
//     // ... upsert `payload`; the server only ever sees ciphertext for `ssn`.
//     const secret = cipher.open(payload); // -> { ssn: "078-05-1120" }
//
// Use a dedicated key for payload encryption; never reuse your at-rest
// `QUIVER_ENCRYPTION_KEY`. The client owns the key — losing it means the data
// is unrecoverable.

import { XChaCha20Poly1305 } from "@stablelib/xchacha20poly1305";

/** The reserved payload key under which a sealed envelope is stored. */
export const ENVELOPE_KEY = "__quiver_enc__";

const VERSION = 1;
const ALG = "xchacha20poly1305";
const NONCE_LEN = 24;
const TAG_LEN = 16;
const AAD = new TextEncoder().encode("quiver/payload/v1");

/** An error sealing or opening a client-side payload envelope. */
export class PayloadError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PayloadError";
  }
}

/** A client-held key for sealing and opening payload envelopes (ADR-0012). */
export class PayloadCipher {
  readonly #key: Uint8Array;

  private constructor(key: Uint8Array) {
    this.#key = key;
  }

  /** Build a cipher from a raw 256-bit (32-byte) key. */
  static fromBytes(key: Uint8Array): PayloadCipher {
    if (key.length !== 32) {
      throw new PayloadError(`payload key must be 32 bytes, got ${key.length}`);
    }
    return new PayloadCipher(Uint8Array.from(key));
  }

  /** Build a cipher from a 64-character hex-encoded 256-bit key. */
  static fromHex(hex: string): PayloadCipher {
    const clean = hex.trim();
    if (!/^[0-9a-fA-F]{64}$/.test(clean)) {
      throw new PayloadError(
        `payload key must be 64 hex characters, got ${clean.length}`,
      );
    }
    const key = new Uint8Array(32);
    for (let i = 0; i < 32; i++) {
      key[i] = Number.parseInt(clean.slice(i * 2, i * 2 + 2), 16);
    }
    return new PayloadCipher(key);
  }

  /** Seal `plaintext` into a one-key envelope `{ [ENVELOPE_KEY]: { … } }`. Each
   * call uses a fresh random nonce, so the same value seals to different
   * ciphertext. */
  seal(plaintext: unknown): Record<string, unknown> {
    const json = JSON.stringify(plaintext);
    if (json === undefined) {
      throw new PayloadError("cannot seal a value that is not JSON-serializable");
    }
    const nonce = randomBytes(NONCE_LEN);
    const ciphertext = new XChaCha20Poly1305(this.#key).seal(
      nonce,
      new TextEncoder().encode(json),
      AAD,
    );
    return {
      [ENVELOPE_KEY]: {
        v: VERSION,
        alg: ALG,
        n: toBase64(nonce),
        ct: toBase64(ciphertext),
      },
    };
  }

  /** Open an envelope sealed by {@link seal}, returning the plaintext. `sealed`
   * may carry cleartext sibling fields; only the reserved key is read. A wrong
   * key or any tampering throws {@link PayloadError}. */
  open(sealed: unknown): unknown {
    if (!isSealed(sealed)) {
      throw new PayloadError("payload is not a quiver-encrypted envelope");
    }
    const envelope = (sealed as Record<string, unknown>)[ENVELOPE_KEY];
    if (typeof envelope !== "object" || envelope === null) {
      throw new PayloadError("envelope is not an object");
    }
    const env = envelope as Record<string, unknown>;
    if (env.v !== VERSION) {
      throw new PayloadError(`unsupported envelope version: ${String(env.v)}`);
    }
    if (env.alg !== ALG) {
      throw new PayloadError(`unsupported envelope algorithm: ${String(env.alg)}`);
    }
    const nonce = decodeField(env, "n");
    if (nonce.length !== NONCE_LEN) {
      throw new PayloadError(`nonce is ${nonce.length} bytes, expected ${NONCE_LEN}`);
    }
    const ciphertext = decodeField(env, "ct");
    if (ciphertext.length < TAG_LEN) {
      throw new PayloadError(
        `ciphertext is ${ciphertext.length} bytes, shorter than the ${TAG_LEN}-byte tag`,
      );
    }
    const message = new XChaCha20Poly1305(this.#key).open(nonce, ciphertext, AAD);
    if (message === null) {
      throw new PayloadError("wrong key or tampered ciphertext");
    }
    return JSON.parse(new TextDecoder().decode(message));
  }
}

/** Whether `value` carries a Quiver payload envelope. */
export function isSealed(value: unknown): boolean {
  return typeof value === "object" && value !== null && ENVELOPE_KEY in value;
}

function decodeField(envelope: Record<string, unknown>, field: string): Uint8Array {
  const raw = envelope[field];
  if (typeof raw !== "string") {
    throw new PayloadError(`missing envelope field '${field}'`);
  }
  return fromBase64(raw);
}

function randomBytes(length: number): Uint8Array {
  const webcrypto = globalThis.crypto;
  if (!webcrypto?.getRandomValues) {
    throw new PayloadError("a Web Crypto getRandomValues implementation is required");
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
