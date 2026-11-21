//! Pack format and its single decode-before-upload choke point, [`seal_and_check`].
//!
//! ```text
//! "XFPK1\n\0\0" | segment bytes ... | zstd(JSON Trailer) | u32 LE trailer length | "XFPKEND1"
//! ```
//! A segment is an independent unit of at most [`SEG_MAX`] decoded bytes, stored raw (codec 0) or
//! as one zstd frame (codec 1). The trailer lists every segment and every chunk, so a pack is
//! self-describing without any index object.

use crate::error::{Error, Result};
use crate::hash::ChunkId;
use serde::{Deserialize, Serialize};
use std::io::Read;

pub const HEAD: &[u8; 8] = b"XFPK1\n\0\0";
pub const TAIL: &[u8; 8] = b"XFPKEND1";
/// Largest decoded segment.
pub const SEG_MAX: usize = 4 << 20;
/// Largest compressed trailer accepted.
pub const TRAILER_MAX: usize = 16 << 20;
/// Largest decoded trailer JSON accepted.
pub const TRAILER_JSON_MAX: usize = 48 << 20;
/// Hard ceiling for a pack: below every supported backend's single-part cutoff (200 MiB).
pub const PACK_CEILING: u64 = 128 << 20;
/// Largest pack object accepted when reading (ceiling plus slack for the trailer).
pub const PACK_READ_MAX: u64 = 144 << 20;

pub const CODEC_RAW: u8 = 0;
pub const CODEC_ZSTD: u8 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SegmentInfo {
    pub off: u64,
    pub clen: u64,
    pub ulen: u64,
    pub codec: u8,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChunkLoc {
    pub id: ChunkId,
    pub seg: u32,
    pub off: u32,
    pub len: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trailer {
    pub v: u32,
    pub segments: Vec<SegmentInfo>,
    pub chunks: Vec<ChunkLoc>,
}

/// Decode a zstd stream with a hard output limit; never allocates past `limit + 1` bytes.
pub fn bounded_zstd(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let dec =
        zstd::stream::read::Decoder::new(data).map_err(|e| Error::corrupt(format!("zstd: {e}")))?;
    let mut out = Vec::new();
    dec.take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::corrupt(format!("zstd: {e}")))?;
    if out.len() > limit {
        return Err(Error::corrupt(format!(
            "decoded data exceeds the {limit} byte limit"
        )));
    }
    Ok(out)
}

/// One segment being filled.
#[derive(Default)]
pub struct OpenSegment {
    pub data: Vec<u8>,
    pub chunks: Vec<(ChunkId, u32, u32)>,
}

impl OpenSegment {
    pub fn fits(&self, len: usize) -> bool {
        self.data.len() + len <= SEG_MAX
    }
    pub fn push(&mut self, id: ChunkId, bytes: &[u8]) {
        let off = self.data.len() as u32;
        self.data.extend_from_slice(bytes);
        self.chunks.push((id, off, bytes.len() as u32));
    }
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// Assembles segments into a pack buffer.
pub struct PackBuilder {
    buf: Vec<u8>,
    segments: Vec<SegmentInfo>,
    chunks: Vec<ChunkLoc>,
    raw_bytes: u64,
}

impl Default for PackBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PackBuilder {
    pub fn new() -> Self {
        let mut buf = Vec::with_capacity(1 << 20);
        buf.extend_from_slice(HEAD);
        PackBuilder {
            buf,
            segments: Vec::new(),
            chunks: Vec::new(),
            raw_bytes: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
    pub fn raw_bytes(&self) -> u64 {
        self.raw_bytes
    }

    /// Append a finished segment. `stored` is either the raw bytes (codec 0) or one zstd frame.
    pub fn add_segment(&mut self, seg: &OpenSegment, codec: u8, stored: &[u8]) {
        self.add_stored(&seg.chunks, seg.data.len() as u64, codec, stored);
    }

    /// Append a segment whose raw bytes are already gone: `chunks` are (id, offset, len) within
    /// the decoded segment of `ulen` bytes.
    pub fn add_stored(
        &mut self,
        chunks: &[(ChunkId, u32, u32)],
        ulen: u64,
        codec: u8,
        stored: &[u8],
    ) {
        let idx = self.segments.len() as u32;
        // The broken variant records an off-by-one offset; seal_and_check must catch it
        // (AC-14a). Never enabled in a release build.
        let skew = if cfg!(feature = "broken-pack-offset") {
            1
        } else {
            0
        };
        self.segments.push(SegmentInfo {
            off: self.buf.len() as u64 + skew,
            clen: stored.len() as u64,
            ulen,
            codec,
        });
        self.buf.extend_from_slice(stored);
        for &(id, off, len) in chunks {
            self.chunks.push(ChunkLoc {
                id,
                seg: idx,
                off,
                len,
            });
        }
        self.raw_bytes += ulen;
    }

    /// Serialize the trailer and return the finished pack bytes and its trailer.
    pub fn finish(mut self) -> Result<(Vec<u8>, Trailer)> {
        let trailer = Trailer {
            v: 1,
            segments: self.segments,
            chunks: self.chunks,
        };
        let json =
            serde_json::to_vec(&trailer).map_err(|e| Error::Bug(format!("trailer json: {e}")))?;
        let z =
            zstd::bulk::compress(&json, 3).map_err(|e| Error::Bug(format!("trailer zstd: {e}")))?;
        if z.len() > TRAILER_MAX {
            return Err(Error::Bug("pack trailer too large".into()));
        }
        self.buf.extend_from_slice(&z);
        self.buf.extend_from_slice(&(z.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(TAIL);
        Ok((self.buf, trailer))
    }
}

/// The content-addressed object key of a pack.
pub fn pack_key(bytes: &[u8]) -> String {
    let h = blake3::hash(bytes).to_hex();
    format!("v1/packs/{}/{}.pack", &h[..2], h)
}

/// Parse and bounds-check a pack's trailer. Every offset is validated with checked arithmetic.
pub fn parse_pack(bytes: &[u8]) -> Result<Trailer> {
    let n = bytes.len();
    if n as u64 > PACK_READ_MAX {
        return Err(Error::corrupt("pack larger than the read limit"));
    }
    if n < HEAD.len() + 12 || &bytes[..8] != HEAD || &bytes[n - 8..] != TAIL {
        return Err(Error::corrupt("pack magic missing"));
    }
    let tlen = u32::from_le_bytes(bytes[n - 12..n - 8].try_into().expect("4 bytes")) as usize;
    let body_end = (n - 12)
        .checked_sub(tlen)
        .filter(|&e| e >= HEAD.len() && tlen <= TRAILER_MAX)
        .ok_or_else(|| Error::corrupt("pack trailer length out of range"))?;
    let json = bounded_zstd(&bytes[body_end..n - 12], TRAILER_JSON_MAX)?;
    let t: Trailer =
        serde_json::from_slice(&json).map_err(|e| Error::corrupt(format!("pack trailer: {e}")))?;
    if t.v != 1 {
        return Err(Error::corrupt(format!("unknown pack version {}", t.v)));
    }
    for s in &t.segments {
        let end = s
            .off
            .checked_add(s.clen)
            .ok_or_else(|| Error::corrupt("segment overflow"))?;
        if s.off < HEAD.len() as u64 || end > body_end as u64 || s.ulen as usize > SEG_MAX {
            return Err(Error::corrupt("segment out of range"));
        }
        match s.codec {
            CODEC_RAW if s.clen == s.ulen => {}
            CODEC_ZSTD => {}
            _ => return Err(Error::corrupt("bad segment codec")),
        }
    }
    for c in &t.chunks {
        let s = t
            .segments
            .get(c.seg as usize)
            .ok_or_else(|| Error::corrupt("chunk segment out of range"))?;
        let end = (c.off as u64)
            .checked_add(c.len as u64)
            .ok_or_else(|| Error::corrupt("chunk overflow"))?;
        if end > s.ulen {
            return Err(Error::corrupt("chunk out of segment range"));
        }
    }
    Ok(t)
}

/// Decode one segment, bounded by its declared length (and [`SEG_MAX`]).
pub fn decode_segment(bytes: &[u8], s: &SegmentInfo) -> Result<Vec<u8>> {
    let stored = &bytes[s.off as usize..(s.off + s.clen) as usize];
    let out = match s.codec {
        CODEC_RAW => stored.to_vec(),
        _ => bounded_zstd(stored, s.ulen as usize)?,
    };
    if out.len() as u64 != s.ulen {
        return Err(Error::corrupt("segment decoded to the wrong length"));
    }
    Ok(out)
}

/// The decode-before-upload choke point: parse the serialized pack, decode every segment and check
/// every chunk's BLAKE3 against its id. A pack that fails here is never uploaded.
pub fn seal_and_check(bytes: &[u8]) -> Result<Trailer> {
    let t = parse_pack(bytes)?;
    let mut by_seg: Vec<Vec<&ChunkLoc>> = vec![Vec::new(); t.segments.len()];
    for c in &t.chunks {
        by_seg[c.seg as usize].push(c);
    }
    for (i, s) in t.segments.iter().enumerate() {
        let data = decode_segment(bytes, s)?;
        for c in &by_seg[i] {
            let slice = &data[c.off as usize..(c.off + c.len) as usize];
            if ChunkId::of(slice) != c.id {
                return Err(Error::corrupt(format!(
                    "chunk {:?} does not match its id",
                    c.id
                )));
            }
        }
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(chunks: &[&[u8]], compress: bool) -> Vec<u8> {
        let mut seg = OpenSegment::default();
        for c in chunks {
            seg.push(ChunkId::of(c), c);
        }
        let mut pb = PackBuilder::new();
        if compress {
            let z = zstd::bulk::compress(&seg.data, 3).unwrap();
            pb.add_segment(&seg, CODEC_ZSTD, &z);
        } else {
            pb.add_segment(&seg, CODEC_RAW, &seg.data.clone());
        }
        pb.finish().unwrap().0
    }

    #[test]
    fn roundtrip_raw_and_zstd() {
        for compress in [false, true] {
            let p = build(&[b"hello world", b"second chunk", &[7u8; 5000]], compress);
            let t = seal_and_check(&p).unwrap();
            assert_eq!(t.chunks.len(), 3);
        }
    }

    #[test]
    fn corrupt_offsets_and_bytes_are_refused() {
        let p = build(&[b"hello world", b"second chunk"], false);
        // flip a data byte: the chunk hash check must catch it
        let mut q = p.clone();
        q[HEAD.len() + 1] ^= 1;
        assert!(seal_and_check(&q).is_err());
        // truncated: magic missing
        assert!(seal_and_check(&p[..p.len() - 1]).is_err());
        // trailer length beyond the object
        let mut r = p.clone();
        let n = r.len();
        r[n - 12..n - 8].copy_from_slice(&(u32::MAX).to_le_bytes());
        assert!(parse_pack(&r).is_err());
    }

    #[test]
    fn bounded_decoder_refuses_bombs() {
        let big = vec![0u8; 8 << 20];
        let z = zstd::bulk::compress(&big, 3).unwrap();
        assert!(bounded_zstd(&z, 4 << 20).is_err());
        assert_eq!(bounded_zstd(&z, 8 << 20).unwrap().len(), 8 << 20);
    }
}
