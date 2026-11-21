//! Content-defined chunking (FastCDC 2020) with BLAKE3 ids, plus a whole-file fast path for small
//! files. Parameters are constants of format v1.

use crate::error::{Error, Result};
use crate::hash::ChunkId;
use std::io::Read;

/// Files below this size are one chunk.
pub const SMALL_FILE: u64 = 1 << 20;
pub const CDC_MIN: u32 = 256 << 10;
pub const CDC_AVG: u32 = 1 << 20;
pub const CDC_MAX: u32 = 4 << 20;

pub struct ChunkedFile {
    /// (id, length) in file order.
    pub chunks: Vec<(ChunkId, u32)>,
    /// BLAKE3 of the whole content.
    pub blake3: [u8; 32],
    /// Bytes actually read.
    pub bytes: u64,
}

/// Read `r` to its end, cutting it into chunks. `size_hint` picks the small-file path; a small
/// file that turns out larger than the hint is still read completely (the caller's before/after
/// identity check reports it as changed). `on_chunk` receives each chunk's id and bytes.
pub fn chunk_stream<R: Read>(
    mut r: R,
    size_hint: u64,
    mut on_chunk: impl FnMut(ChunkId, Vec<u8>) -> Result<()>,
) -> Result<ChunkedFile> {
    if size_hint < SMALL_FILE {
        let mut whole = blake3::Hasher::new();
        let mut chunks = Vec::new();
        let mut buf = Vec::with_capacity(size_hint as usize);
        (&mut r).take(SMALL_FILE).read_to_end(&mut buf)?;
        if buf.len() as u64 >= SMALL_FILE {
            // grew past the small-file limit while we looked: chunk the rest with CDC
            let rest = std::io::Cursor::new(buf).chain(r);
            return chunk_cdc(rest, on_chunk);
        }
        let bytes = buf.len() as u64;
        if !buf.is_empty() {
            whole.update(&buf);
            let id = ChunkId::of(&buf);
            chunks.push((id, buf.len() as u32));
            on_chunk(id, buf)?;
        }
        return Ok(ChunkedFile {
            chunks,
            blake3: *whole.finalize().as_bytes(),
            bytes,
        });
    }
    chunk_cdc(r, on_chunk)
}

fn chunk_cdc<R: Read>(
    r: R,
    mut on_chunk: impl FnMut(ChunkId, Vec<u8>) -> Result<()>,
) -> Result<ChunkedFile> {
    let mut whole = blake3::Hasher::new();
    let mut chunks = Vec::new();
    let mut bytes = 0u64;
    for c in fastcdc::v2020::StreamCDC::new(r, CDC_MIN, CDC_AVG, CDC_MAX) {
        let c = c.map_err(|e| match e {
            fastcdc::v2020::Error::IoError(io) => Error::Io(io),
            other => Error::Io(std::io::Error::other(format!("{other:?}"))),
        })?;
        whole.update(&c.data);
        bytes += c.data.len() as u64;
        let id = ChunkId::of(&c.data);
        chunks.push((id, c.data.len() as u32));
        on_chunk(id, c.data)?;
    }
    Ok(ChunkedFile {
        chunks,
        blake3: *whole.finalize().as_bytes(),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::Rng;

    fn ids(data: &[u8]) -> Vec<ChunkId> {
        chunk_stream(data, data.len() as u64, |_, _| Ok(()))
            .unwrap()
            .chunks
            .iter()
            .map(|c| c.0)
            .collect()
    }

    #[test]
    fn small_and_empty() {
        let r = chunk_stream(&b""[..], 0, |_, _| Ok(())).unwrap();
        assert!(r.chunks.is_empty());
        assert_eq!(r.blake3, *blake3::hash(b"").as_bytes());
        let r = chunk_stream(&b"abc"[..], 3, |_, _| Ok(())).unwrap();
        assert_eq!(r.chunks.len(), 1);
        assert_eq!(r.blake3, *blake3::hash(b"abc").as_bytes());
    }

    #[test]
    fn a_mid_file_edit_changes_few_chunks() {
        let mut rng = Rng::new(7);
        let mut data = vec![0u8; 32 << 20];
        rng.fill(&mut data);
        let a = ids(&data);
        data[16 << 20..(16 << 20) + 4096].fill(0xAA);
        let b = ids(&data);
        let changed = b.iter().filter(|id| !a.contains(id)).count();
        assert!(
            a.len() >= 16,
            "expected CDC to cut many chunks, got {}",
            a.len()
        );
        assert!(changed <= 3, "{changed} chunks changed");
        let whole = chunk_stream(&data[..], data.len() as u64, |_, _| Ok(())).unwrap();
        assert_eq!(whole.blake3, *blake3::hash(&data).as_bytes());
        assert_eq!(whole.bytes, data.len() as u64);
    }
}
