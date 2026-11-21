//! PruneDisk's backup engine (goodqa#16550).
//!
//! A streaming, zero-staging pipeline: walk, content-defined chunking with BLAKE3, rate-matched
//! zstd, packs of bounded size, parallel single-part uploads through a locally started rclone rcd,
//! and verification against the store's own checksum. A path is reported in a [`receipt::Receipt`]
//! only after the pack(s) holding its bytes and the index object that lists it have been uploaded
//! and stat-verified. The engine never deletes local data.
//!
//! Remote layout (all objects immutable): `v1/format.json`, `v1/packs/<xx>/<blake3>.pack`,
//! `v1/index/<run>/<seq>.idx`, `v1/snapshots/<run>.json`.

pub mod backup;
pub mod chunker;
pub mod compress;
pub mod control;
pub mod error;
pub mod gen;
pub mod hash;
pub mod meta;
pub mod pack;
pub mod receipt;
pub mod repo;
pub mod restore;
pub mod transport;
pub mod util;
pub mod walk;

pub use error::{Error, Result};

/// Crate version recorded in every receipt (`engine` field).
pub const ENGINE: &str = concat!("disk-cleaner-xfer ", env!("CARGO_PKG_VERSION"));
