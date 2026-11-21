//! Chunk ids (BLAKE3) and the store-side checksum kinds (SHA1 on B2, MD5 on S3).

use crate::util::{hex, unhex};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkId(pub [u8; 32]);

impl ChunkId {
    pub fn of(data: &[u8]) -> ChunkId {
        ChunkId(*blake3::hash(data).as_bytes())
    }
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
    pub fn from_hex(s: &str) -> Option<ChunkId> {
        let v = unhex(s)?;
        let a: [u8; 32] = v.try_into().ok()?;
        Some(ChunkId(a))
    }
}

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.to_hex()[..16])
    }
}

impl Serialize for ChunkId {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ChunkId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ChunkId::from_hex(&s).ok_or_else(|| serde::de::Error::custom("bad chunk id"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashKind {
    Sha1,
    Md5,
}

impl HashKind {
    pub fn name(self) -> &'static str {
        match self {
            HashKind::Sha1 => "sha1",
            HashKind::Md5 => "md5",
        }
    }
    pub fn parse(s: &str) -> Option<HashKind> {
        match s.to_ascii_lowercase().as_str() {
            "sha1" => Some(HashKind::Sha1),
            "md5" => Some(HashKind::Md5),
            _ => None,
        }
    }
    /// Lowercase hex digest of `data`, the value a store reports for an object with these bytes.
    pub fn digest(self, data: &[u8]) -> String {
        match self {
            HashKind::Sha1 => {
                use sha1::Digest;
                hex(&sha1::Sha1::digest(data))
            }
            HashKind::Md5 => {
                use md5::Digest;
                hex(&md5::Md5::digest(data))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn digests_match_known_values() {
        assert_eq!(
            HashKind::Sha1.digest(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            HashKind::Md5.digest(b"abc"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        let id = ChunkId::of(b"abc");
        assert_eq!(ChunkId::from_hex(&id.to_hex()), Some(id));
    }
}
