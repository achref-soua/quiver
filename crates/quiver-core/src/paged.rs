// SPDX-License-Identifier: AGPL-3.0-only
//! Paged blob storage: read and write a length-delimited byte blob across a
//! sequence of CRC'd [`crate::page`] pages, through a [`PageCodec`].
//!
//! Both the manifest and sealed segments persist a `postcard` blob this way.
//! Page 0's body begins with the total blob length (`u64`) so the reader
//! reassembles exactly the original bytes; every page carries the standard
//! header and CRC, so corruption anywhere in the file is detected on read.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::error::{CoreError, Result};
use crate::page::{PAGE_BODY_CAP, PAGE_SIZE, PageCodec, PageType, build_page, parse_page};

const LEN_PREFIX: usize = 8;

/// `fsync` a directory so prior file creations or renames within it are durable.
///
/// On Windows, `File::open` on a directory requires `FILE_FLAG_BACKUP_SEMANTICS`
/// (0x02000000); without it the kernel returns ERROR_ACCESS_DENIED (os error 5).
/// We skip the fsync on Windows because NTFS journals directory-entry changes
/// atomically — the WAL and per-page CRCs already give us crash safety there.
pub(crate) fn fsync_dir(dir: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = dir;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        let f = File::open(dir).map_err(|e| CoreError::io(dir, e))?;
        f.sync_all().map_err(|e| CoreError::io(dir, e))
    }
}

fn paginate(body: &[u8], page_type: PageType, stamp: u64) -> Result<Vec<[u8; PAGE_SIZE]>> {
    let total = body.len() as u64;
    let mut pages = Vec::new();

    let first_cap = PAGE_BODY_CAP - LEN_PREFIX;
    let first_take = body.len().min(first_cap);
    let mut page0 = Vec::with_capacity(LEN_PREFIX + first_take);
    page0.extend_from_slice(&total.to_le_bytes());
    page0.extend_from_slice(&body[..first_take]);
    pages.push(build_page(page_type, 0, stamp, &page0)?);

    let mut cursor = first_take;
    let mut page_id = 1u64;
    while cursor < body.len() {
        let take = (body.len() - cursor).min(PAGE_BODY_CAP);
        pages.push(build_page(
            page_type,
            page_id,
            stamp,
            &body[cursor..cursor + take],
        )?);
        cursor += take;
        page_id += 1;
    }
    Ok(pages)
}

/// Write `body` to `path` as a paged file (overwriting any existing content),
/// sealing each page with `codec` and `fsync`'ing the file. The `stamp` is
/// recorded in each page's `lsn` header field (manifest version / segment id).
/// Does not `fsync` the directory — the caller decides when the file's existence
/// must be durable relative to other operations.
pub(crate) fn write_paged(
    path: &Path,
    codec: &dyn PageCodec,
    page_type: PageType,
    stamp: u64,
    body: &[u8],
) -> Result<()> {
    let pages = paginate(body, page_type, stamp)?;
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|e| CoreError::io(path, e))?;
    let mut block = vec![0u8; codec.block_size()];
    for (i, page) in pages.iter().enumerate() {
        codec.seal(i as u64, page, &mut block)?;
        f.write_all(&block).map_err(|e| CoreError::io(path, e))?;
    }
    f.sync_data().map_err(|e| CoreError::io(path, e))?;
    Ok(())
}

/// Read a paged file written by [`write_paged`], verifying every page and
/// reassembling the original blob. Fails (never silently truncates) on a bad
/// page, an out-of-order page id, or a size that is not a whole number of
/// blocks.
pub(crate) fn read_paged(
    path: &Path,
    codec: &dyn PageCodec,
    page_type: PageType,
) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).map_err(|e| CoreError::io(path, e))?;
    let block = codec.block_size();
    if raw.is_empty() || raw.len() % block != 0 {
        return Err(CoreError::MalformedPage(format!(
            "paged file {} size {} is not a multiple of block size {block}",
            path.display(),
            raw.len()
        )));
    }
    let n_pages = raw.len() / block;
    let mut body = Vec::new();
    let mut total: Option<usize> = None;
    let mut plain = [0u8; PAGE_SIZE];
    for i in 0..n_pages {
        let blk = &raw[i * block..(i + 1) * block];
        codec.open(i as u64, blk, &mut plain)?;
        let (hdr, page_body) = parse_page(&plain, page_type)?;
        if hdr.page_id != i as u64 {
            return Err(CoreError::MalformedPage(format!(
                "paged file {} page {i} carries page_id {}",
                path.display(),
                hdr.page_id
            )));
        }
        if i == 0 {
            if page_body.len() < LEN_PREFIX {
                return Err(CoreError::MalformedPage(
                    "paged file page 0 too small for its length prefix".to_owned(),
                ));
            }
            let len_bytes: [u8; 8] = page_body[0..LEN_PREFIX]
                .try_into()
                .map_err(|_| CoreError::MalformedPage("bad length prefix".to_owned()))?;
            total = Some(u64::from_le_bytes(len_bytes) as usize);
            body.extend_from_slice(&page_body[LEN_PREFIX..]);
        } else {
            body.extend_from_slice(page_body);
        }
    }
    let total =
        total.ok_or_else(|| CoreError::MalformedPage("paged file has no pages".to_owned()))?;
    if body.len() < total {
        return Err(CoreError::MalformedPage(format!(
            "paged body {} shorter than declared length {total}",
            body.len()
        )));
    }
    body.truncate(total);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PlainCodec;

    #[test]
    fn small_and_large_blobs_roundtrip() {
        for len in [0usize, 1, PAGE_BODY_CAP, PAGE_BODY_CAP * 3 + 7] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("blob");
            let body: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
            write_paged(&path, &PlainCodec, PageType::Segment, 1, &body).unwrap();
            let back = read_paged(&path, &PlainCodec, PageType::Segment).unwrap();
            assert_eq!(back, body);
        }
    }

    #[test]
    fn wrong_page_type_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        write_paged(&path, &PlainCodec, PageType::Manifest, 1, b"hello").unwrap();
        assert!(read_paged(&path, &PlainCodec, PageType::Segment).is_err());
    }

    #[test]
    fn corruption_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        write_paged(&path, &PlainCodec, PageType::Segment, 1, &[7u8; 200]).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[80] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            read_paged(&path, &PlainCodec, PageType::Segment),
            Err(CoreError::PageCorrupt { .. })
        ));
    }
}
