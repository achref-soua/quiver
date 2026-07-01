// SPDX-License-Identifier: AGPL-3.0-only
//! Experimental property-preserving vector encryption (ADR-0031): the
//! **reference implementation** of Quiver's Distance-Comparison-Preserving
//! Encryption (DCPE).
//!
//! DCPE encrypts embedding vectors so that an **untrusted server can still
//! answer approximate-nearest-neighbour queries** over the ciphertexts —
//! because the *ordering* of Euclidean distances is preserved, up to a tunable
//! margin — **without ever holding the plaintext vectors or the key**. The
//! scheme is the published **Scale-And-Perturb (SAP)** construction of
//! Fuchsbauer, Ghosal, Hauke & O'Neill, *"Approximate
//! Distance-Comparison-Preserving Symmetric Encryption"* (IACR ePrint 2021/1666;
//! SCN 2022), which underlies IronCore Labs' Cloaked AI.
//!
//! # ⚠ Security: read this before using DCPE
//!
//! **DCPE is experimental, is _not_ semantically secure (not IND-CPA), and leaks
//! information by design.** It is a *different, weaker* tool than
//! encryption-at-rest ([`crate::AeadCodec`], ADR-0010) or payload encryption
//! ([`crate::PayloadCipher`], ADR-0012), for a *different* problem (search over
//! encrypted vectors on an untrusted server). Specifically:
//!
//! - **Leaks by design:** the approximate Euclidean **distance-comparison
//!   relation** among ciphertexts — hence approximate pairwise distances (up to a
//!   secret scale and the margin), cluster structure, and dataset geometry.
//!   Anyone holding the ciphertexts can run the same nearest-neighbour search and
//!   clustering you can; that is the whole point.
//! - **Broken by:** an adversary with **known plaintext/ciphertext pairs** (the
//!   low-entropy secret scale is recoverable), or a **strong prior** on the
//!   embedding distribution or access to the embedding model (embedding-inversion
//!   attacks apply — preserving distance preserves much of what inversion needs).
//!   DCPE assumes a high-entropy message distribution.
//! - **Tunable trade-off:** a larger approximation factor hides exact distances
//!   better but lowers search recall.
//! - **Euclidean (L2) only:** the secret scaling changes norms, so cosine and
//!   inner-product orderings are *not* preserved.
//!
//! Use a **dedicated** DCPE key — never your at-rest or payload key — and prefer
//! to encrypt and query from the same client. The client owns the key; Quiver
//! never sees it, and losing it makes the vectors unrecoverable.
//!
//! # Construction (cipher **v2**, ADR-0035)
//!
//! Key material is derived from one master secret with HKDF-SHA256: a secret
//! scaling factor `s ∈ [1, 2)`, a CSPRNG key, an HMAC key, and (new in v2) a
//! shuffle CSPRNG key. To encrypt `m ∈ ℝ^d` with approximation factor `β ≥ 0`:
//!
//! 1. **Normalise** (optional, ordering-preserving): apply a fixed global affine
//!    transform `m₁ = (m − μ)·α` — a per-dimension shift vector `μ` (default `0`)
//!    and a *single* positive scalar `α` (default `1`). A uniform per-coordinate
//!    shift cancels in any difference and a single scalar scales every distance
//!    by the same `α`, so the distance-comparison ordering is preserved exactly
//!    and the step is invertible (`m = m₁/α + μ`). See [`Normalization`].
//! 2. **Shuffle**: permute the components with a permutation `π` derived from the
//!    key alone (HKDF sub-key `quiver/dcpe/v2/shuffle`, a ChaCha20 CSPRNG with a
//!    fixed zero IV, and a fully-specified Fisher–Yates). L2 distance is invariant
//!    under any permutation of coordinates, so the *same* `π` applied to every
//!    vector and query preserves all pairwise distances exactly (no recall cost);
//!    it hides which ciphertext coordinate is which plaintext coordinate.
//! 3. draw a fresh random 96-bit IV;
//! 4. seed ChaCha20 from `(prfKey, iv)` and sample a perturbation `λ` **uniformly
//!    in the d-ball of radius `(s/4)·β`** (a Gaussian direction normalised and
//!    scaled by `radius = (s/4)·β·U^{1/d}`, `U ~ Uniform[0,1)`);
//! 5. the ciphertext vector is `c = s·π(m₁) + λ` (stored/indexed like any vector);
//! 6. an HMAC-SHA256 tag over `(domain ‖ β ‖ iv ‖ c)` gives tamper-evidence, with
//!    the domain bumped to `quiver/dcpe/v2/tag` so a v1 ciphertext fails a v2
//!    integrity check (fail-closed) instead of decrypting to garbage.
//!
//! Decryption re-derives `λ` from `(prfKey, iv)` — the perturbation is
//! *pseudorandom*, so it cancels — verifies the tag, and reverses the pipeline:
//! `m = T⁻¹(π⁻¹((c − λ)/s))`. Querying encrypts the query the same way; the secret
//! `s`, the permutation `π`, and the normalisation are identical for data and
//! queries, so they cancel in the distance ordering while the bounded per-vector
//! perturbations are the margin.
//!
//! **Honest limit (ADR-0035):** normalisation is restricted to a *global* affine
//! transform (per-axis shift + a single scalar scale) because that is the strongest
//! normalisation that preserves the L2 distance-comparison ordering. Per-axis
//! variance *whitening* (an anisotropic per-dimension scale) re-weights the
//! dimensions in the distance and so is **incompatible** with the untrusted-server
//! search this scheme exists for; it is deliberately not offered.
//!
//! v2 is a breaking change from the v1 cipher of ADR-0031 (which stays immutable):
//! v1 ciphertexts are not v2-decryptable. DCPE is experimental and off by default,
//! and the cipher is client-side — there is no on-disk format change.
//!
//! The byte-level sampling (the ChaCha20 keystream as the CSPRNG, the
//! `u64 → [0,1)` mapping, and Box-Muller normals) is fixed so it can be
//! reproduced in another language. Because the ciphertext is float-valued and its
//! computation uses transcendental functions, bit-exact reproduction across
//! languages is *not* guaranteed (libm ULP differences); cross-language interop
//! is validated within a tolerance. This Rust module is the canonical reference.
//!
//! ```
//! use quiver_crypto::dcpe::DcpeCipher;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // A dedicated key, and a small approximation factor for high recall.
//! let cipher = DcpeCipher::from_hex(
//!     "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
//!     0.05,
//! )?;
//! let sealed = cipher.encrypt(&[0.1, 0.2, 0.3, 0.4])?;
//! // `sealed.ciphertext` is upserted and indexed like any L2 vector; the server
//! // never sees the plaintext. The key holder can recover it:
//! let recovered = cipher.decrypt(&sealed)?;
//! assert_eq!(recovered.len(), 4);
//! # Ok(())
//! # }
//! ```

use std::f64::consts::PI;

use chacha20::ChaCha20;
use chacha20::cipher::generic_array::GenericArray;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::codec::{KEY_LEN, decode_key_hex};

type HmacSha256 = Hmac<Sha256>;

/// DCPE initialisation-vector length in bytes (a 96-bit ChaCha20 nonce).
pub const IV_LEN: usize = 12;
/// DCPE integrity-tag length in bytes (full HMAC-SHA256 output).
pub const TAG_LEN: usize = 32;

// HKDF-SHA256 `info` strings: distinct sub-keys from one master secret. The
// scale/prf/auth derivations are unchanged from v1 (so the secret scale is stable
// across the v1→v2 hardening); `shuffle` is new in v2.
const INFO_SCALE: &[u8] = b"quiver/dcpe/v1/scale";
const INFO_PRF: &[u8] = b"quiver/dcpe/v1/prf";
const INFO_AUTH: &[u8] = b"quiver/dcpe/v1/auth";
const INFO_SHUFFLE: &[u8] = b"quiver/dcpe/v2/shuffle";
// Domain-separation prefix bound into every integrity tag. Bumped to v2 so a v1
// ciphertext fails a v2 integrity check (fail-closed) rather than decrypting to
// garbage under the hardened cipher.
const AUTH_DOMAIN: &[u8] = b"quiver/dcpe/v2/tag";

/// Errors from DCPE encryption, decryption, or construction.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DcpeError {
    /// The supplied master key was not a valid 256-bit key. The message never
    /// echoes the key material.
    #[error("invalid DCPE key: {0}")]
    InvalidKey(String),
    /// The approximation factor was negative, NaN, or infinite. It must be a
    /// finite value `≥ 0` (and `> 0` to hide anything — `0` adds no perturbation).
    #[error("invalid approximation factor: must be finite and >= 0")]
    InvalidApproximationFactor,
    /// An empty vector was supplied; DCPE needs at least one dimension.
    #[error("empty vector: DCPE needs at least one dimension")]
    EmptyVector,
    /// The integrity tag did not verify: the wrong key, or a tampered or
    /// corrupted ciphertext.
    #[error("ciphertext integrity check failed: wrong key or tampered ciphertext")]
    Integrity,
    /// The normalisation parameters were invalid: a non-positive or non-finite
    /// scale, or a non-finite shift component.
    #[error("invalid normalisation: scale must be finite and > 0 and shifts finite")]
    InvalidNormalization,
    /// A vector's dimension did not match the normalisation shift length.
    #[error("dimension mismatch: vector has {vector} dims, normalisation has {shift}")]
    DimensionMismatch {
        /// The vector's dimension.
        vector: usize,
        /// The normalisation shift vector's length.
        shift: usize,
    },
}

/// A fixed, ordering-preserving global affine normalisation for DCPE (ADR-0035).
///
/// Maps a plaintext `m ∈ ℝ^d` to `m₁ = (m − shift)·scale` before encryption,
/// where `shift ∈ ℝ^d` is a per-dimension translation and `scale > 0` is a
/// **single** scalar. Both steps preserve the L2 distance-comparison ordering the
/// untrusted server ranks on (a uniform shift cancels in any difference; a single
/// positive scalar scales every distance by the same factor) and are invertible.
///
/// Supply it once from a one-time measurement of your corpus — its per-dimension
/// mean as `shift` and a global RMS radius as `1/scale`, say — and reuse it for
/// the data *and* the queries. It is **fixed at construction**, never recomputed
/// per batch.
///
/// # Honest limit
///
/// This is the strongest normalisation compatible with searchable DCPE. Per-axis
/// variance *whitening* (a different scale per dimension) is anisotropic, re-weights
/// the dimensions in the L2 distance, and so breaks the distance-comparison ordering
/// — it is intentionally not expressible here. See ADR-0035 and `docs/security/dcpe.md`.
#[derive(Clone, Debug)]
pub struct Normalization {
    shift: Vec<f32>,
    scale: f32,
}

impl Normalization {
    /// Build a normalisation from a per-dimension shift and a single positive scale.
    ///
    /// # Errors
    /// [`DcpeError::InvalidNormalization`] if `scale` is not finite and `> 0`, or
    /// any `shift` component is not finite.
    pub fn new(shift: Vec<f32>, scale: f32) -> Result<Self, DcpeError> {
        if !scale.is_finite() || scale <= 0.0 || shift.iter().any(|x| !x.is_finite()) {
            return Err(DcpeError::InvalidNormalization);
        }
        Ok(Self { shift, scale })
    }

    /// The per-dimension shift vector.
    #[must_use]
    pub fn shift(&self) -> &[f32] {
        &self.shift
    }

    /// The single scalar scale.
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }
}

/// A DCPE-encrypted vector: the ciphertext (upserted and indexed like any
/// vector), the IV that seeds its perturbation, and an HMAC-SHA256 integrity tag.
///
/// The caller stows `iv` and `tag` (e.g. in the point payload) if it wants to
/// [`decrypt`](DcpeCipher::decrypt) later; queries do not need them.
#[derive(Clone, Debug, PartialEq)]
pub struct EncryptedVector {
    /// The encrypted vector `c = s·m + λ`, stored and searched like a plaintext
    /// L2 vector.
    pub ciphertext: Vec<f32>,
    /// The per-encryption initialisation vector seeding the perturbation.
    pub iv: [u8; IV_LEN],
    /// HMAC-SHA256 tag over `(domain ‖ β ‖ iv ‖ ciphertext)`.
    pub tag: [u8; TAG_LEN],
}

/// A client-held DCPE key bound to one approximation factor (ADR-0031).
///
/// One master secret derives all sub-keys, held in zeroizing buffers and wiped on
/// drop. Construct one cipher per `(key, approximation_factor)` and reuse it; the
/// same factor must be used for the data and the queries searched against it.
pub struct DcpeCipher {
    // The secret scaling factor `s ∈ [1, 2)`, derived from the master key.
    scale: f64,
    // ChaCha20 CSPRNG key for the perturbation.
    prf_key: Zeroizing<[u8; 32]>,
    // ChaCha20 CSPRNG key for the key-derived component shuffle (v2).
    shuffle_key: Zeroizing<[u8; 32]>,
    // HMAC-SHA256 key for the integrity tag.
    auth_key: Zeroizing<[u8; 32]>,
    // The approximation factor `β` (the security/accuracy knob).
    approximation_factor: f32,
    // Optional fixed global affine normalisation applied before encryption (v2).
    normalization: Option<Normalization>,
}

impl DcpeCipher {
    /// Build a cipher from a raw 256-bit master key and an approximation factor.
    ///
    /// # Errors
    /// [`DcpeError::InvalidApproximationFactor`] if `approximation_factor` is not
    /// finite and `≥ 0`.
    pub fn new(key: [u8; KEY_LEN], approximation_factor: f32) -> Result<Self, DcpeError> {
        if !approximation_factor.is_finite() || approximation_factor < 0.0 {
            return Err(DcpeError::InvalidApproximationFactor);
        }
        let key = Zeroizing::new(key);
        let hk = Hkdf::<Sha256>::new(None, key.as_slice());

        let mut scale_bytes = Zeroizing::new([0u8; 8]);
        expand(&hk, INFO_SCALE, scale_bytes.as_mut_slice())?;
        // Map 8 key-derived bytes to a scale in [1, 2): take the top 53 bits as a
        // fraction (the f64 mantissa width) and add 1. Deterministic and portable.
        let frac = (u64::from_le_bytes(*scale_bytes) >> 11) as f64 * (1.0 / ((1u64 << 53) as f64));
        let scale = 1.0 + frac;

        let mut prf_key = Zeroizing::new([0u8; 32]);
        expand(&hk, INFO_PRF, prf_key.as_mut_slice())?;
        let mut shuffle_key = Zeroizing::new([0u8; 32]);
        expand(&hk, INFO_SHUFFLE, shuffle_key.as_mut_slice())?;
        let mut auth_key = Zeroizing::new([0u8; 32]);
        expand(&hk, INFO_AUTH, auth_key.as_mut_slice())?;

        Ok(Self {
            scale,
            prf_key,
            shuffle_key,
            auth_key,
            approximation_factor,
            normalization: None,
        })
    }

    /// Attach a fixed global affine [`Normalization`] applied before encryption
    /// (and reversed on decryption). Builder style: `DcpeCipher::new(..)?.with_normalization(n)`.
    /// The same normalisation must be used for the data and the queries searched
    /// against it. Defaults to none.
    #[must_use]
    pub fn with_normalization(mut self, normalization: Normalization) -> Self {
        self.normalization = Some(normalization);
        self
    }

    /// Build a cipher from a 64-character hex-encoded 256-bit master key.
    ///
    /// # Errors
    /// [`DcpeError::InvalidKey`] if the string is not exactly 64 hex digits;
    /// [`DcpeError::InvalidApproximationFactor`] if the factor is invalid.
    pub fn from_hex(hex: &str, approximation_factor: f32) -> Result<Self, DcpeError> {
        let key = decode_key_hex(hex).map_err(|e| DcpeError::InvalidKey(e.to_string()))?;
        Self::new(key, approximation_factor)
    }

    /// The secret scaling factor `s`. Exposed for inspection and testing; it is
    /// part of the key and should be treated as secret.
    #[must_use]
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// The approximation factor `β` this cipher was built with.
    #[must_use]
    pub fn approximation_factor(&self) -> f32 {
        self.approximation_factor
    }

    /// Encrypt a vector for storage. Each call draws a fresh IV, so the same
    /// vector encrypts differently every time (hiding equality).
    ///
    /// # Errors
    /// [`DcpeError::EmptyVector`] if `vector` is empty;
    /// [`DcpeError::DimensionMismatch`] if a normalisation is set and its shift
    /// length differs from `vector.len()`.
    pub fn encrypt(&self, vector: &[f32]) -> Result<EncryptedVector, DcpeError> {
        if vector.is_empty() {
            return Err(DcpeError::EmptyVector);
        }
        let pre = self.pretransform(vector)?;
        let mut iv = [0u8; IV_LEN];
        OsRng.fill_bytes(&mut iv);
        let ciphertext = self.scale_and_perturb(&pre, &iv);
        let tag = self.compute_tag(&iv, &ciphertext)?;
        Ok(EncryptedVector {
            ciphertext,
            iv,
            tag,
        })
    }

    /// Encrypt a query vector for searching against DCPE-encrypted data. The
    /// returned ciphertext is passed straight to the server's L2 search; it is
    /// never decrypted, so no IV or tag is retained.
    ///
    /// # Errors
    /// [`DcpeError::EmptyVector`] if `vector` is empty;
    /// [`DcpeError::DimensionMismatch`] if a normalisation is set and its shift
    /// length differs from `vector.len()`.
    pub fn encrypt_query(&self, vector: &[f32]) -> Result<Vec<f32>, DcpeError> {
        if vector.is_empty() {
            return Err(DcpeError::EmptyVector);
        }
        let pre = self.pretransform(vector)?;
        let mut iv = [0u8; IV_LEN];
        OsRng.fill_bytes(&mut iv);
        Ok(self.scale_and_perturb(&pre, &iv))
    }

    /// Recover the plaintext vector from an [`EncryptedVector`]. The integrity tag
    /// is verified first (constant-time); recovery is approximate (within float
    /// tolerance and the scheme's own perturbation).
    ///
    /// # Errors
    /// [`DcpeError::Integrity`] if the tag does not verify (wrong key or tampered
    /// ciphertext); [`DcpeError::EmptyVector`] if the ciphertext is empty;
    /// [`DcpeError::DimensionMismatch`] if a normalisation is set and its shift
    /// length differs from the ciphertext length.
    pub fn decrypt(&self, sealed: &EncryptedVector) -> Result<Vec<f32>, DcpeError> {
        if sealed.ciphertext.is_empty() {
            return Err(DcpeError::EmptyVector);
        }
        // Constant-time verification before touching the ciphertext.
        self.mac_for(&sealed.iv, &sealed.ciphertext)?
            .verify_slice(&sealed.tag)
            .map_err(|_| DcpeError::Integrity)?;

        let lambda = self.perturbation(&sealed.iv, sealed.ciphertext.len());
        // Recover the shuffled, normalised vector `(c − λ)/s`, then reverse the
        // pipeline: un-shuffle, then un-normalise.
        let shuffled: Vec<f64> = sealed
            .ciphertext
            .iter()
            .zip(&lambda)
            .map(|(&c, &l)| (f64::from(c) - l) / self.scale)
            .collect();
        let normalized = self.unshuffle(&shuffled);
        self.denormalize(&normalized)
    }

    // Compute `c = s·x + λ` in f64, storing f32. `x` is the normalised+shuffled
    // vector from `pretransform`.
    fn scale_and_perturb(&self, x: &[f64], iv: &[u8; IV_LEN]) -> Vec<f32> {
        let lambda = self.perturbation(iv, x.len());
        x.iter()
            .zip(&lambda)
            .map(|(&m, &l)| (self.scale * m + l) as f32)
            .collect()
    }

    // Normalise (optional) then shuffle: produce `π((m − μ)·α)` in f64.
    fn pretransform(&self, vector: &[f32]) -> Result<Vec<f64>, DcpeError> {
        let normalized = self.normalize(vector)?;
        let perm = self.permutation(vector.len());
        Ok(perm.iter().map(|&p| normalized[p]).collect())
    }

    // Apply the optional normalisation `(m − μ)·α` in f64 (identity if none).
    fn normalize(&self, vector: &[f32]) -> Result<Vec<f64>, DcpeError> {
        match &self.normalization {
            None => Ok(vector.iter().map(|&x| f64::from(x)).collect()),
            Some(n) => {
                if n.shift.len() != vector.len() {
                    return Err(DcpeError::DimensionMismatch {
                        vector: vector.len(),
                        shift: n.shift.len(),
                    });
                }
                Ok(vector
                    .iter()
                    .zip(&n.shift)
                    .map(|(&x, &mu)| (f64::from(x) - f64::from(mu)) * f64::from(n.scale))
                    .collect())
            }
        }
    }

    // Invert the shuffle: `out[perm[i]] = shuffled[i]`.
    fn unshuffle(&self, shuffled: &[f64]) -> Vec<f64> {
        let perm = self.permutation(shuffled.len());
        let mut out = vec![0.0f64; shuffled.len()];
        for (i, &p) in perm.iter().enumerate() {
            out[p] = shuffled[i];
        }
        out
    }

    // Invert the normalisation `m = m₁/α + μ` (identity if none), to f32.
    fn denormalize(&self, normalized: &[f64]) -> Result<Vec<f32>, DcpeError> {
        match &self.normalization {
            None => Ok(normalized.iter().map(|&x| x as f32).collect()),
            Some(n) => {
                if n.shift.len() != normalized.len() {
                    return Err(DcpeError::DimensionMismatch {
                        vector: normalized.len(),
                        shift: n.shift.len(),
                    });
                }
                Ok(normalized
                    .iter()
                    .zip(&n.shift)
                    .map(|(&x, &mu)| (x / f64::from(n.scale) + f64::from(mu)) as f32)
                    .collect())
            }
        }
    }

    // The key-derived permutation of `[0, d)`, identical for every vector and
    // query (it depends only on the key and `d`, with a fixed zero IV), so all
    // pairwise L2 distances are preserved. Fisher–Yates from the top using the
    // shuffle keystream; the `% (i + 1)` reduction has cryptographically negligible
    // modulo bias and is fixed for cross-language reproducibility.
    fn permutation(&self, d: usize) -> Vec<usize> {
        let mut perm: Vec<usize> = (0..d).collect();
        if d <= 1 {
            return perm;
        }
        // A fixed (zero) IV is deliberate and not a hardcoded secret: this
        // keystream's secrecy comes entirely from `self.shuffle_key`, and the
        // dimension permutation must be deterministic and reproducible across the
        // Rust/Python/TypeScript ciphers (cross-language KAT). The per-vector
        // encryptions above use a random IV; only this key-derived shuffle is fixed.
        let iv = [0u8; IV_LEN];
        let mut rng = KeyStream::new(&self.shuffle_key, &iv);
        for i in (1..d).rev() {
            let j = (rng.next_u64() % (i as u64 + 1)) as usize;
            perm.swap(i, j);
        }
        perm
    }

    // Derive the perturbation λ: a uniform point in the d-ball of radius
    // `(s/4)·β`. The CSPRNG draws the d normal components of the direction first,
    // then one uniform for the radius — this consumption order is part of the
    // wire-compatible specification.
    fn perturbation(&self, iv: &[u8; IV_LEN], d: usize) -> Vec<f64> {
        let mut rng = KeyStream::new(&self.prf_key, iv);
        let direction: Vec<f64> = (0..d).map(|_| rng.next_normal()).collect();
        let norm = direction.iter().map(|x| x * x).sum::<f64>().sqrt();
        let u = rng.next_unit();
        let radius =
            (self.scale / 4.0) * f64::from(self.approximation_factor) * u.powf(1.0 / d as f64);
        if norm == 0.0 {
            // The all-zero direction has probability ~0; map it to no perturbation.
            return vec![0.0; d];
        }
        direction.iter().map(|x| x / norm * radius).collect()
    }

    // Build the HMAC over (domain ‖ β ‖ iv ‖ ciphertext), ready to finalize or
    // verify. HMAC accepts any key length, so a 32-byte key never fails to init;
    // the error is threaded only to avoid an unwrap on the production path.
    fn mac_for(&self, iv: &[u8; IV_LEN], ciphertext: &[f32]) -> Result<HmacSha256, DcpeError> {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.auth_key[..])
            .map_err(|_| DcpeError::InvalidKey("HMAC key rejected".to_owned()))?;
        mac.update(AUTH_DOMAIN);
        mac.update(&self.approximation_factor.to_le_bytes());
        mac.update(iv);
        for &c in ciphertext {
            mac.update(&c.to_le_bytes());
        }
        // Bind the normalization parameters (when present) so a shift/scale
        // mismatch between the encrypting and decrypting cipher fails the tag
        // (fail-closed) instead of silently denormalizing to a wrong plaintext,
        // matching the fail-closed treatment of `approximation_factor`. Absent
        // normalization updates nothing, preserving the tag for existing
        // ciphertexts (the shipped path carries no normalization).
        if let Some(n) = &self.normalization {
            mac.update(b"quiver/dcpe/v2/norm");
            mac.update(&n.scale.to_le_bytes());
            for &s in &n.shift {
                mac.update(&s.to_le_bytes());
            }
        }
        Ok(mac)
    }

    fn compute_tag(
        &self,
        iv: &[u8; IV_LEN],
        ciphertext: &[f32],
    ) -> Result<[u8; TAG_LEN], DcpeError> {
        let out = self.mac_for(iv, ciphertext)?.finalize().into_bytes();
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&out);
        Ok(tag)
    }
}

// HKDF-expand into `out`, mapping any failure to an invalid-key error.
fn expand(hk: &Hkdf<Sha256>, info: &[u8], out: &mut [u8]) -> Result<(), DcpeError> {
    hk.expand(info, out)
        .map_err(|_| DcpeError::InvalidKey("HKDF expand failed".to_owned()))
}

/// A deterministic CSPRNG: the raw ChaCha20 keystream seeded from `(key, iv)`,
/// read as little-endian `u64`s. Standard normals come from Box-Muller, caching
/// the paired value. The layout is fixed for cross-language reproducibility.
struct KeyStream {
    cipher: ChaCha20,
    block: [u8; 64],
    used: usize,
    spare_normal: Option<f64>,
}

impl KeyStream {
    fn new(key: &[u8; 32], iv: &[u8; IV_LEN]) -> Self {
        let cipher = ChaCha20::new(GenericArray::from_slice(key), GenericArray::from_slice(iv));
        // `used == 64` forces a refill on the first read.
        Self {
            cipher,
            block: [0u8; 64],
            used: 64,
            spare_normal: None,
        }
    }

    fn next_u64(&mut self) -> u64 {
        if self.used + 8 > self.block.len() {
            self.block = [0u8; 64];
            self.cipher.apply_keystream(&mut self.block);
            self.used = 0;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.block[self.used..self.used + 8]);
        self.used += 8;
        u64::from_le_bytes(b)
    }

    // A uniform in [0, 1) with 53-bit resolution (the f64 mantissa width).
    fn next_unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / ((1u64 << 53) as f64))
    }

    // A standard normal via Box-Muller. `u1 ∈ (0, 1]` (so `ln` is finite); the
    // sine partner is cached and returned on the next call.
    fn next_normal(&mut self) -> f64 {
        if let Some(z) = self.spare_normal.take() {
            return z;
        }
        let u1 = 1.0 - self.next_unit();
        let u2 = self.next_unit();
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = 2.0 * PI * u2;
        self.spare_normal = Some(r * theta.sin());
        r * theta.cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn cipher(beta: f32) -> DcpeCipher {
        DcpeCipher::from_hex(KEY_HEX, beta).expect("test key/factor parse")
    }

    // A small deterministic pseudo-random vector generator (SplitMix64), so tests
    // need no `rand` dependency and are reproducible.
    fn dataset(n: usize, d: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut s = seed;
        let mut next = || {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 40) as f32 / (1u32 << 24) as f32
        };
        (0..n)
            .map(|_| (0..d).map(|_| next() - 0.5).collect())
            .collect()
    }

    fn l2(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
    }

    fn top_k(query: &[f32], data: &[Vec<f32>], k: usize) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..data.len()).collect();
        idx.sort_by(|&i, &j| {
            l2(query, &data[i])
                .partial_cmp(&l2(query, &data[j]))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        idx.truncate(k);
        idx
    }

    #[test]
    fn round_trip_recovers_the_plaintext() {
        let cipher = cipher(0.1);
        let plaintext = vec![0.1, -0.2, 0.3, -0.4, 0.5, 0.6, -0.7, 0.8];
        let sealed = cipher.encrypt(&plaintext).expect("encrypt");
        let recovered = cipher.decrypt(&sealed).expect("decrypt");
        assert_eq!(recovered.len(), plaintext.len());
        for (got, want) in recovered.iter().zip(&plaintext) {
            assert!((got - want).abs() < 1e-3, "got {got}, want {want}");
        }
    }

    #[test]
    fn each_encryption_uses_a_fresh_iv() {
        let cipher = cipher(0.1);
        let v = vec![0.1, 0.2, 0.3, 0.4];
        let a = cipher.encrypt(&v).expect("a");
        let b = cipher.encrypt(&v).expect("b");
        assert_ne!(a.iv, b.iv, "fresh IV per call");
        assert_ne!(a.ciphertext, b.ciphertext, "randomised ciphertext");
        // Both still decrypt back to (approximately) the same plaintext.
        for sealed in [&a, &b] {
            let r = cipher.decrypt(sealed).expect("decrypt");
            for (got, want) in r.iter().zip(&v) {
                assert!((got - want).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn wrong_key_fails_integrity() {
        let sealed = cipher(0.1).encrypt(&[0.1, 0.2, 0.3, 0.4]).expect("encrypt");
        let wrong = DcpeCipher::from_hex(&"ff".repeat(KEY_LEN), 0.1).expect("wrong key parses");
        assert!(matches!(wrong.decrypt(&sealed), Err(DcpeError::Integrity)));
    }

    #[test]
    fn tampered_ciphertext_fails_integrity() {
        let cipher = cipher(0.1);
        let mut sealed = cipher.encrypt(&[0.1, 0.2, 0.3, 0.4]).expect("encrypt");
        sealed.ciphertext[0] += 0.5;
        assert!(matches!(cipher.decrypt(&sealed), Err(DcpeError::Integrity)));
    }

    #[test]
    fn normalization_mismatch_fails_integrity() {
        // The tag binds the normalization params, so decrypting a normalized
        // ciphertext with a cipher that lacks or changes the normalization
        // fails closed rather than returning silently wrong plaintext.
        let norm = Normalization::new(vec![0.1, 0.2, 0.3, 0.4], 3.0).expect("normalization");
        let enc = cipher(0.1)
            .with_normalization(norm)
            .encrypt(&[0.5, 0.6, 0.7, 0.8])
            .expect("encrypt");
        // Same key and beta, but no normalization → tag mismatch.
        assert!(matches!(cipher(0.1).decrypt(&enc), Err(DcpeError::Integrity)));
        // A different normalization scale also fails closed.
        let other = Normalization::new(vec![0.1, 0.2, 0.3, 0.4], 4.0).expect("normalization");
        assert!(matches!(
            cipher(0.1).with_normalization(other).decrypt(&enc),
            Err(DcpeError::Integrity)
        ));
    }

    // THE core property: top-k nearest neighbours over ciphertexts match the
    // plaintext top-k at a small approximation factor.
    #[test]
    fn preserves_nearest_neighbours_at_small_beta() {
        let data = dataset(400, 32, 1);
        let queries = dataset(20, 32, 999);
        let cipher = cipher(0.02);
        let enc: Vec<Vec<f32>> = data
            .iter()
            .map(|v| cipher.encrypt(v).expect("enc").ciphertext)
            .collect();

        let k = 10;
        let mut hits = 0usize;
        let mut total = 0usize;
        for q in &queries {
            let truth = top_k(q, &data, k);
            let eq = cipher.encrypt_query(q).expect("enc query");
            let got = top_k(&eq, &enc, k);
            let truth_set: std::collections::HashSet<_> = truth.iter().collect();
            hits += got.iter().filter(|i| truth_set.contains(i)).count();
            total += k;
        }
        let recall = hits as f64 / total as f64;
        assert!(recall > 0.9, "recall@{k} = {recall:.3}, expected > 0.9");
    }

    // The security/accuracy trade-off, demonstrated rather than merely claimed: a
    // larger approximation factor lowers recall.
    #[test]
    fn recall_degrades_as_beta_grows() {
        let data = dataset(400, 32, 7);
        let queries = dataset(20, 32, 13);
        let k = 10;
        let recall_at = |beta: f32| {
            let cipher = cipher(beta);
            let enc: Vec<Vec<f32>> = data
                .iter()
                .map(|v| cipher.encrypt(v).expect("enc").ciphertext)
                .collect();
            let mut hits = 0usize;
            for q in &queries {
                let truth: std::collections::HashSet<_> = top_k(q, &data, k).into_iter().collect();
                let eq = cipher.encrypt_query(q).expect("query");
                hits += top_k(&eq, &enc, k)
                    .iter()
                    .filter(|i| truth.contains(i))
                    .count();
            }
            hits as f64 / (queries.len() * k) as f64
        };
        let small = recall_at(0.02);
        let large = recall_at(1.0);
        assert!(small > 0.85, "small-beta recall {small:.3} should be high");
        assert!(
            large < small,
            "recall should degrade: small {small:.3} vs large {large:.3}"
        );
    }

    // Honest positive control: the distance-comparison relation IS recoverable
    // from ciphertexts alone — we are not overclaiming secrecy. Most random
    // triples have their distance ordering preserved.
    #[test]
    fn the_distance_comparison_leak_is_real() {
        let data = dataset(120, 32, 42);
        let cipher = cipher(0.05);
        let enc: Vec<Vec<f32>> = data
            .iter()
            .map(|v| cipher.encrypt(v).expect("enc").ciphertext)
            .collect();
        let mut preserved = 0usize;
        let mut total = 0usize;
        for a in 0..40 {
            for b in (a + 1)..60 {
                for c in (b + 1)..80 {
                    let pt_ab = l2(&data[a], &data[b]) < l2(&data[a], &data[c]);
                    let ct_ab = l2(&enc[a], &enc[b]) < l2(&enc[a], &enc[c]);
                    if pt_ab == ct_ab {
                        preserved += 1;
                    }
                    total += 1;
                }
            }
        }
        let rate = preserved as f64 / total as f64;
        assert!(
            rate > 0.9,
            "ciphertext distance ordering leaks the plaintext ordering: {rate:.3}"
        );
    }

    #[test]
    fn beta_zero_is_exactly_distance_preserving_but_hides_nothing() {
        // With no perturbation the ciphertext is s·m: distances scale by s², so
        // the ordering is preserved exactly — and nothing is hidden (documented).
        let cipher = cipher(0.0);
        let data = dataset(50, 16, 5);
        let enc: Vec<Vec<f32>> = data
            .iter()
            .map(|v| cipher.encrypt(v).expect("enc").ciphertext)
            .collect();
        let q = vec![0.1f32; 16];
        let eq = cipher.encrypt_query(&q).expect("query");
        assert_eq!(top_k(&q, &data, 10), top_k(&eq, &enc, 10));
    }

    #[test]
    fn rejects_invalid_approximation_factor() {
        for bad in [-0.1f32, f32::NAN, f32::INFINITY] {
            assert!(matches!(
                DcpeCipher::from_hex(KEY_HEX, bad),
                Err(DcpeError::InvalidApproximationFactor)
            ));
        }
    }

    #[test]
    fn rejects_empty_vectors() {
        let cipher = cipher(0.1);
        assert!(matches!(cipher.encrypt(&[]), Err(DcpeError::EmptyVector)));
        assert!(matches!(
            cipher.encrypt_query(&[]),
            Err(DcpeError::EmptyVector)
        ));
    }

    #[test]
    fn from_hex_rejects_bad_keys() {
        assert!(matches!(
            DcpeCipher::from_hex("abcd", 0.1),
            Err(DcpeError::InvalidKey(_))
        ));
        assert!(matches!(
            DcpeCipher::from_hex(&"zz".repeat(KEY_LEN), 0.1),
            Err(DcpeError::InvalidKey(_))
        ));
    }

    #[test]
    fn scale_is_key_derived_and_in_range() {
        let c = cipher(0.1);
        assert!(
            (1.0..2.0).contains(&c.scale()),
            "scale {} in [1,2)",
            c.scale()
        );
        // A different key gives a different scale (overwhelmingly).
        let other = DcpeCipher::from_hex(&"11".repeat(KEY_LEN), 0.1).expect("key");
        assert_ne!(c.scale(), other.scale());
    }

    // Decryption is deterministic given the IV: re-deriving the perturbation from
    // a fixed (key, iv) reproduces the same plaintext. This determinism is what a
    // cross-language port must match (validated by the SDK known-answer tests).
    #[test]
    fn perturbation_is_deterministic_for_a_fixed_iv() {
        let cipher = cipher(0.1);
        let iv = [7u8; IV_LEN];
        let a = cipher.perturbation(&iv, 8);
        let b = cipher.perturbation(&iv, 8);
        assert_eq!(a, b);
    }

    // --- v2 hardening: the key-derived component shuffle ---

    #[test]
    fn shuffle_is_a_valid_deterministic_permutation() {
        let c = cipher(0.1);
        let p1 = c.permutation(16);
        let p2 = c.permutation(16);
        assert_eq!(p1, p2, "the permutation is deterministic for a key");
        let mut sorted = p1.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            (0..16).collect::<Vec<_>>(),
            "is a permutation of 0..d"
        );
        assert_ne!(
            p1,
            (0..16).collect::<Vec<_>>(),
            "the shuffle actually permutes at d = 16"
        );
    }

    #[test]
    fn shuffle_is_key_dependent() {
        let a = cipher(0.1).permutation(32);
        let b = DcpeCipher::from_hex(&"11".repeat(KEY_LEN), 0.1)
            .expect("key")
            .permutation(32);
        assert_ne!(a, b, "different keys give different shuffles");
    }

    #[test]
    fn shuffle_hides_axis_alignment_but_round_trips() {
        // At β = 0 there is no perturbation, so the ciphertext is exactly s·π(m):
        // its component order differs from the un-shuffled s·m (the shuffle has an
        // effect), yet decrypt recovers the original ordering.
        let c = cipher(0.0);
        let plaintext: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let sealed = c.encrypt(&plaintext).expect("encrypt");
        let naive: Vec<f32> = plaintext
            .iter()
            .map(|&m| (c.scale() * f64::from(m)) as f32)
            .collect();
        assert_ne!(sealed.ciphertext, naive, "components are shuffled");
        let recovered = c.decrypt(&sealed).expect("decrypt");
        for (got, want) in recovered.iter().zip(&plaintext) {
            assert!((got - want).abs() < 1e-3, "got {got} want {want}");
        }
    }

    // --- v2 hardening: ordering-preserving global normalisation ---

    #[test]
    fn normalization_round_trips() {
        let shift = vec![0.5f32, -0.5, 1.0, 0.0, 2.0, -1.0, 0.25, -0.25];
        let norm = Normalization::new(shift, 3.0).expect("normalization");
        let c = cipher(0.1).with_normalization(norm);
        let plaintext = vec![0.1, -0.2, 0.3, -0.4, 0.5, 0.6, -0.7, 0.8];
        let sealed = c.encrypt(&plaintext).expect("encrypt");
        let recovered = c.decrypt(&sealed).expect("decrypt");
        for (got, want) in recovered.iter().zip(&plaintext) {
            assert!((got - want).abs() < 1e-3, "got {got} want {want}");
        }
    }

    #[test]
    fn normalization_preserves_nearest_neighbours() {
        // A global affine (per-axis shift + a single positive scale) is an L2
        // isometry up to a positive scalar, so search recall is unchanged.
        let data = dataset(300, 16, 3);
        let queries = dataset(15, 16, 99);
        let shift: Vec<f32> = (0..16).map(|i| 0.1 * i as f32).collect();
        let norm = Normalization::new(shift, 5.0).expect("normalization");
        let c = cipher(0.02).with_normalization(norm);
        let enc: Vec<Vec<f32>> = data
            .iter()
            .map(|v| c.encrypt(v).expect("enc").ciphertext)
            .collect();
        let k = 10;
        let mut hits = 0usize;
        for q in &queries {
            let truth: std::collections::HashSet<_> = top_k(q, &data, k).into_iter().collect();
            let eq = c.encrypt_query(q).expect("query");
            hits += top_k(&eq, &enc, k)
                .iter()
                .filter(|i| truth.contains(i))
                .count();
        }
        let recall = hits as f64 / (queries.len() * k) as f64;
        assert!(
            recall > 0.9,
            "recall {recall:.3} should stay high with normalisation"
        );
    }

    #[test]
    fn normalization_rejects_bad_params() {
        for bad in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            assert!(matches!(
                Normalization::new(vec![0.0; 4], bad),
                Err(DcpeError::InvalidNormalization)
            ));
        }
        assert!(matches!(
            Normalization::new(vec![f32::NAN; 4], 1.0),
            Err(DcpeError::InvalidNormalization)
        ));
    }

    #[test]
    fn normalization_dimension_mismatch_errors() {
        let norm = Normalization::new(vec![0.0f32; 4], 1.0).expect("normalization");
        let c = cipher(0.1).with_normalization(norm);
        assert!(matches!(
            c.encrypt(&[1.0, 2.0, 3.0]),
            Err(DcpeError::DimensionMismatch {
                vector: 3,
                shift: 4
            })
        ));
    }
}
