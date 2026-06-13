// SPDX-License-Identifier: AGPL-3.0-only
//! Fixed-size pages: the unit of checksum, encryption, and buffered I/O.
//!
//! Every paged file (the manifest, payload heaps, secondary indexes, index
//! artifacts) is a sequence of 16 KiB pages. A page carries a fixed 32-byte
//! header and a CRC32C over its plaintext, so corruption is *detected* on read
//! and never served (ADR-0004). When encryption-at-rest is enabled the whole
//! plaintext page is additionally sealed with an AEAD by a [`PageCodec`]; the
//! inner CRC still guards the plaintext path and the unencrypted mode.
//!
//! Plaintext layout (little-endian, header 8-byte aligned):
//!
//! ```text
//! 0   magic:u32        4  format_ver:u16   6  page_type:u8   7  _pad:u8
//! 8   page_id:u64
//! 16  lsn:u64
//! 24  payload_len:u32                       28 crc32c:u32
//! 32  ...body: payload_len bytes, then zero padding to 16 KiB...
//! ```
//!
//! The CRC covers header bytes `0..28` (everything but the CRC field itself)
//! followed by the live body bytes `0..payload_len`; trailing padding and the
//! CRC field are excluded so the checksum is stable regardless of padding.

use crate::error::{CoreError, Result};

/// Size of a page in bytes — the fixed I/O, checksum, and encryption unit.
pub const PAGE_SIZE: usize = 16 * 1024;
/// Size of the fixed page header in bytes.
pub const PAGE_HEADER_SIZE: usize = 32;
/// Maximum payload bytes that fit in a single page body.
pub const PAGE_BODY_CAP: usize = PAGE_SIZE - PAGE_HEADER_SIZE;
/// Magic identifying a Quiver page (`b"QVPG"`, little-endian).
pub const PAGE_MAGIC: u32 = u32::from_le_bytes(*b"QVPG");
/// Current page format version. Unknown versions are refused on read.
pub const PAGE_FORMAT_VERSION: u16 = 1;

// Field offsets within the 32-byte header.
const OFF_MAGIC: usize = 0;
const OFF_FORMAT_VER: usize = 4;
const OFF_PAGE_TYPE: usize = 6;
const OFF_PAGE_ID: usize = 8;
const OFF_LSN: usize = 16;
const OFF_PAYLOAD_LEN: usize = 24;
const OFF_CRC: usize = 28;
// The CRC covers the header up to (but not including) the CRC field, then body.
const CRC_HEADER_BYTES: usize = OFF_CRC; // 28

/// The kind of data a page holds. It is stored in the header and validated on
/// read, so a page can never be silently misread as the wrong type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum PageType {
    /// A manifest (catalog) page.
    Manifest = 1,
    /// A sealed-segment page.
    Segment = 2,
    /// An index-artifact page (e.g. a disk-resident graph block, ADR-0019).
    IndexBlock = 3,
}

impl PageType {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Self::Manifest),
            2 => Ok(Self::Segment),
            3 => Ok(Self::IndexBlock),
            other => Err(CoreError::MalformedPage(format!(
                "unknown page type {other}"
            ))),
        }
    }
}

#[inline]
fn rd_u16(p: &[u8; PAGE_SIZE], off: usize) -> u16 {
    u16::from_le_bytes([p[off], p[off + 1]])
}

#[inline]
fn rd_u32(p: &[u8; PAGE_SIZE], off: usize) -> u32 {
    u32::from_le_bytes([p[off], p[off + 1], p[off + 2], p[off + 3]])
}

#[inline]
fn rd_u64(p: &[u8; PAGE_SIZE], off: usize) -> u64 {
    u64::from_le_bytes([
        p[off],
        p[off + 1],
        p[off + 2],
        p[off + 3],
        p[off + 4],
        p[off + 5],
        p[off + 6],
        p[off + 7],
    ])
}

// CRC32C over the header (minus the CRC field) and the live body bytes.
fn page_crc(page: &[u8; PAGE_SIZE], payload_len: usize) -> u32 {
    let crc = crc32c::crc32c(&page[..CRC_HEADER_BYTES]);
    crc32c::crc32c_append(crc, &page[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + payload_len])
}

/// Serialize a page into a fresh 16 KiB plaintext buffer with a valid header and
/// CRC. `body` must fit within [`PAGE_BODY_CAP`].
pub fn build_page(
    page_type: PageType,
    page_id: u64,
    lsn: u64,
    body: &[u8],
) -> Result<[u8; PAGE_SIZE]> {
    if body.len() > PAGE_BODY_CAP {
        return Err(CoreError::TooLarge(format!(
            "page body {} exceeds capacity {PAGE_BODY_CAP}",
            body.len()
        )));
    }
    let mut page = [0u8; PAGE_SIZE];
    page[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&PAGE_MAGIC.to_le_bytes());
    page[OFF_FORMAT_VER..OFF_FORMAT_VER + 2].copy_from_slice(&PAGE_FORMAT_VERSION.to_le_bytes());
    page[OFF_PAGE_TYPE] = page_type as u8;
    // Byte at OFF_PAGE_TYPE + 1 is reserved padding; left zero.
    page[OFF_PAGE_ID..OFF_PAGE_ID + 8].copy_from_slice(&page_id.to_le_bytes());
    page[OFF_LSN..OFF_LSN + 8].copy_from_slice(&lsn.to_le_bytes());
    let payload_len = body.len() as u32;
    page[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4].copy_from_slice(&payload_len.to_le_bytes());
    page[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + body.len()].copy_from_slice(body);
    let crc = page_crc(&page, body.len());
    page[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(page)
}

/// A parsed, validated page header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    /// The page kind.
    pub page_type: PageType,
    /// Monotonic page identifier within its file.
    pub page_id: u64,
    /// Last LSN that modified this page.
    pub lsn: u64,
    /// Number of live payload bytes in the body.
    pub payload_len: u32,
}

/// Validate a 16 KiB plaintext page and return its header plus a borrow of the
/// live body. Fails on bad magic, unknown version, wrong type, an impossible
/// length, or a CRC mismatch — corruption is reported, never served.
pub fn parse_page(page: &[u8; PAGE_SIZE], expected: PageType) -> Result<(PageHeader, &[u8])> {
    let magic = rd_u32(page, OFF_MAGIC);
    if magic != PAGE_MAGIC {
        return Err(CoreError::BadMagic {
            expected: PAGE_MAGIC,
            found: magic,
        });
    }
    let format_ver = rd_u16(page, OFF_FORMAT_VER);
    if format_ver != PAGE_FORMAT_VERSION {
        return Err(CoreError::UnsupportedVersion {
            found: format_ver,
            supported: PAGE_FORMAT_VERSION,
        });
    }
    let page_type = PageType::from_u8(page[OFF_PAGE_TYPE])?;
    if page_type != expected {
        return Err(CoreError::MalformedPage(format!(
            "page type {page_type:?} does not match expected {expected:?}"
        )));
    }
    let page_id = rd_u64(page, OFF_PAGE_ID);
    let lsn = rd_u64(page, OFF_LSN);
    let payload_len = rd_u32(page, OFF_PAYLOAD_LEN);
    // Bound the length before it is used to slice, so a corrupt header cannot
    // index out of bounds; an in-range but wrong length is caught by the CRC.
    if payload_len as usize > PAGE_BODY_CAP {
        return Err(CoreError::MalformedPage(format!(
            "payload_len {payload_len} exceeds body capacity {PAGE_BODY_CAP}"
        )));
    }
    let stored_crc = rd_u32(page, OFF_CRC);
    let computed = page_crc(page, payload_len as usize);
    if stored_crc != computed {
        return Err(CoreError::PageCorrupt {
            page_id,
            expected: stored_crc,
            computed,
        });
    }
    let body = &page[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + payload_len as usize];
    Ok((
        PageHeader {
            page_type,
            page_id,
            lsn,
            payload_len,
        },
        body,
    ))
}

/// Transforms Quiver's durable bytes — fixed-size pages and variable-length
/// records — to and from their on-disk representation.
///
/// The plaintext codec ([`PlainCodec`]) is the identity transform; integrity
/// then comes from the page's inner CRC (and, for records, the WAL frame CRC).
/// The encryption-at-rest codec (added with `quiver-crypto`) seals each page with
/// an AEAD into a `[nonce][ciphertext][tag]` block of [`PageCodec::block_size`]
/// bytes, deriving a unique nonce per page so reuse is impossible by
/// construction; the inner CRC still protects the plaintext.
///
/// The WAL is record-framed rather than paged, so the AEAD codec must also seal
/// each WAL record via [`PageCodec::seal_record`]; otherwise an
/// encrypted-at-rest store would still leak its log in plaintext. The default
/// record methods are the identity transform, so [`PlainCodec`] needs no change
/// and a non-encrypting codec writes records verbatim.
pub trait PageCodec: Send + Sync {
    /// On-disk size, in bytes, of one sealed page.
    fn block_size(&self) -> usize;

    /// Seal a plaintext page into its on-disk block. `out` must be exactly
    /// [`PageCodec::block_size`] bytes. `page_id` lets an AEAD codec bind the
    /// page to its position (nonce derivation).
    fn seal(&self, page_id: u64, plaintext: &[u8; PAGE_SIZE], out: &mut [u8]) -> Result<()>;

    /// Open an on-disk block back into a plaintext page. `block` must be exactly
    /// [`PageCodec::block_size`] bytes.
    fn open(&self, page_id: u64, block: &[u8], out: &mut [u8; PAGE_SIZE]) -> Result<()>;

    /// Clone this codec into a new boxed instance. A codec holds only key
    /// material (or nothing), so a clone shares the same keys — this lets a
    /// component that needs its own handle, such as a disk-resident index sealing
    /// its own files, reuse the store's codec (ADR-0019).
    fn clone_box(&self) -> Box<dyn PageCodec>;

    /// Seal a variable-length record — a WAL frame payload — into a
    /// self-describing on-disk blob. The default is the identity transform used
    /// by [`PlainCodec`]; an AEAD codec overrides it to return
    /// `[nonce][ciphertext+tag]`, so no plaintext record ever reaches the disk.
    fn seal_record(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        Ok(plaintext.to_vec())
    }

    /// Open a record produced by [`PageCodec::seal_record`]. The default is the
    /// identity transform; an AEAD codec authenticates and decrypts, returning an
    /// error on a wrong key or any tampering.
    fn open_record(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        Ok(sealed.to_vec())
    }
}

/// The identity codec used when encryption-at-rest is disabled. On-disk bytes
/// equal the plaintext page; integrity is provided by the page's inner CRC.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainCodec;

impl PageCodec for PlainCodec {
    fn block_size(&self) -> usize {
        PAGE_SIZE
    }

    fn seal(&self, _page_id: u64, plaintext: &[u8; PAGE_SIZE], out: &mut [u8]) -> Result<()> {
        if out.len() != PAGE_SIZE {
            return Err(CoreError::MalformedPage(format!(
                "seal output buffer is {} bytes, expected {PAGE_SIZE}",
                out.len()
            )));
        }
        out.copy_from_slice(plaintext);
        Ok(())
    }

    fn open(&self, _page_id: u64, block: &[u8], out: &mut [u8; PAGE_SIZE]) -> Result<()> {
        if block.len() != PAGE_SIZE {
            return Err(CoreError::MalformedPage(format!(
                "page block is {} bytes, expected {PAGE_SIZE}",
                block.len()
            )));
        }
        out.copy_from_slice(block);
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn PageCodec> {
        Box::new(*self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn header_offsets_are_consistent() {
        assert_eq!(PAGE_HEADER_SIZE, 32);
        assert_eq!(PAGE_BODY_CAP, PAGE_SIZE - PAGE_HEADER_SIZE);
        assert_eq!(OFF_CRC + 4, PAGE_HEADER_SIZE);
    }

    #[test]
    fn build_then_parse_roundtrips() {
        for len in [0usize, 1, 32, 1000, PAGE_BODY_CAP] {
            let body: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let page = build_page(PageType::Manifest, 7, 99, &body).unwrap();
            let (hdr, got) = parse_page(&page, PageType::Manifest).unwrap();
            assert_eq!(hdr.page_type, PageType::Manifest);
            assert_eq!(hdr.page_id, 7);
            assert_eq!(hdr.lsn, 99);
            assert_eq!(hdr.payload_len as usize, len);
            assert_eq!(got, &body[..]);
        }
    }

    #[test]
    fn oversized_body_is_rejected() {
        let body = vec![0u8; PAGE_BODY_CAP + 1];
        assert!(matches!(
            build_page(PageType::Manifest, 0, 0, &body),
            Err(CoreError::TooLarge(_))
        ));
    }

    #[test]
    fn corrupt_body_byte_is_detected() {
        let body = vec![0xABu8; 512];
        let mut page = build_page(PageType::Manifest, 1, 1, &body).unwrap();
        page[PAGE_HEADER_SIZE + 10] ^= 0xFF;
        assert!(matches!(
            parse_page(&page, PageType::Manifest),
            Err(CoreError::PageCorrupt { .. })
        ));
    }

    #[test]
    fn corrupt_header_field_is_detected() {
        let body = vec![1u8; 64];
        let mut page = build_page(PageType::Manifest, 1, 1, &body).unwrap();
        // Flip a bit in the LSN field: covered by the CRC, so it must be caught.
        page[OFF_LSN] ^= 0x01;
        assert!(matches!(
            parse_page(&page, PageType::Manifest),
            Err(CoreError::PageCorrupt { .. })
        ));
    }

    #[test]
    fn bad_magic_is_detected() {
        let mut page = build_page(PageType::Manifest, 1, 1, b"hi").unwrap();
        page[OFF_MAGIC] ^= 0xFF;
        assert!(matches!(
            parse_page(&page, PageType::Manifest),
            Err(CoreError::BadMagic { .. })
        ));
    }

    #[test]
    fn unknown_version_is_refused() {
        let mut page = build_page(PageType::Manifest, 1, 1, b"hi").unwrap();
        page[OFF_FORMAT_VER..OFF_FORMAT_VER + 2].copy_from_slice(&9u16.to_le_bytes());
        // Recompute CRC so we exercise the version check, not the CRC check.
        let crc = page_crc(&page, 2);
        page[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            parse_page(&page, PageType::Manifest),
            Err(CoreError::UnsupportedVersion { found: 9, .. })
        ));
    }

    #[test]
    fn impossible_length_is_rejected_without_panicking() {
        let mut page = build_page(PageType::Manifest, 1, 1, b"hi").unwrap();
        page[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4]
            .copy_from_slice(&(PAGE_BODY_CAP as u32 + 1).to_le_bytes());
        assert!(matches!(
            parse_page(&page, PageType::Manifest),
            Err(CoreError::MalformedPage(_))
        ));
    }

    #[test]
    fn plain_codec_roundtrips() {
        let codec = PlainCodec;
        assert_eq!(codec.block_size(), PAGE_SIZE);
        let page = build_page(PageType::Manifest, 3, 3, b"payload").unwrap();
        let mut block = vec![0u8; codec.block_size()];
        codec.seal(3, &page, &mut block).unwrap();
        let mut back = [0u8; PAGE_SIZE];
        codec.open(3, &block, &mut back).unwrap();
        assert_eq!(page, back);
    }

    #[test]
    fn plain_codec_rejects_wrong_buffer_sizes() {
        let codec = PlainCodec;
        let page = [0u8; PAGE_SIZE];
        let mut small = vec![0u8; PAGE_SIZE - 1];
        assert!(codec.seal(0, &page, &mut small).is_err());
        let mut back = [0u8; PAGE_SIZE];
        assert!(codec.open(0, &small, &mut back).is_err());
    }

    proptest! {
        #[test]
        fn any_body_roundtrips(body in proptest::collection::vec(any::<u8>(), 0..PAGE_BODY_CAP)) {
            let page = build_page(PageType::Manifest, 42, 7, &body).unwrap();
            let (hdr, got) = parse_page(&page, PageType::Manifest).unwrap();
            prop_assert_eq!(hdr.payload_len as usize, body.len());
            prop_assert_eq!(got, &body[..]);
        }

        #[test]
        fn any_single_byte_flip_in_live_region_is_detected(
            body in proptest::collection::vec(any::<u8>(), 1..2048usize),
            flip in 0usize..(PAGE_HEADER_SIZE + 2048),
        ) {
            let page = build_page(PageType::Manifest, 1, 1, &body).unwrap();
            let live = PAGE_HEADER_SIZE + body.len();
            // Only positions within the header or live body are CRC-protected
            // (the CRC field at 28..32 excepted). Flipping padding is invisible.
            prop_assume!(flip < live);
            prop_assume!(!(OFF_CRC..PAGE_HEADER_SIZE).contains(&flip));
            let mut corrupt = page;
            corrupt[flip] ^= 0x80;
            // A flip in the CRC-protected region must surface as *some* error.
            prop_assert!(parse_page(&corrupt, PageType::Manifest).is_err());
        }
    }
}
