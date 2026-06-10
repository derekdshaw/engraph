use sha2::{Digest, Sha256};

/// Tokens; messages above this get compressed during ingest.
pub const COMPRESS_THRESHOLD_TOKENS: u32 = 2_000;

pub(crate) fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().to_vec()
}
