//! Deterministic, idempotent compression for persistent context and stored content.
//!
//! Algorithm (in order):
//!   1. Sentinel check — `<<engraph:v1:compressed>>` prefix → return as-is.
//!   2. Whitespace normalization.
//!   3. Per-kind preprocessing (ToolOutput, SessionMessage, ProjectNotes, Generic).
//!   4. Extractive sentence ranking (TF-based, deterministic).
//!   5. Optional caveman-style brevity (configurable per kind).
//!   6. Stamp with sentinel header and provenance trailer.

mod brevity;
mod preprocess;
mod rank;
mod sentinel;
mod stopwords;

pub use sentinel::{is_compressed, ALGO_ID, SENTINEL};

use engraph_core::tokens;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressKind {
    ProjectNotes,
    SessionMessage,
    ToolOutput,
    Generic,
}

#[derive(Debug, Clone)]
pub struct CompressInput<'a> {
    pub text: &'a str,
    pub kind: CompressKind,
    /// Target ratio of compressed/original tokens (e.g. 0.5 = aim for 50%).
    /// Soft target; falls back to whatever the algorithm can guarantee.
    pub target_ratio: f32,
}

impl<'a> CompressInput<'a> {
    pub fn new(text: &'a str, kind: CompressKind) -> Self {
        Self {
            text,
            kind,
            target_ratio: 0.5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompressResult {
    pub text: String,
    pub original_tokens: u32,
    pub compressed_tokens: u32,
    pub algorithm_id: &'static str,
    pub original_hash: [u8; 32],
}

impl CompressResult {
    pub fn ratio(&self) -> f32 {
        if self.original_tokens == 0 {
            return 1.0;
        }
        self.compressed_tokens as f32 / self.original_tokens as f32
    }

    pub fn original_hash_hex(&self) -> String {
        hex_encode(&self.original_hash)
    }
}

pub fn compress(input: CompressInput<'_>) -> CompressResult {
    // 1. Sentinel check (idempotency)
    if is_compressed(input.text) {
        let orig_tokens = tokens::count(input.text);
        return CompressResult {
            text: input.text.to_string(),
            original_tokens: orig_tokens,
            compressed_tokens: orig_tokens,
            algorithm_id: ALGO_ID,
            original_hash: hash_bytes(input.text.as_bytes()),
        };
    }

    let orig_tokens = tokens::count(input.text);
    let orig_hash = hash_bytes(input.text.as_bytes());

    // 2. Whitespace normalization
    let normalized = normalize_whitespace(input.text);

    // 3. Per-kind preprocessing
    let preprocessed = preprocess::apply(&normalized, input.kind);

    // 4. Extractive sentence ranking
    let target_tokens = ((orig_tokens as f32) * input.target_ratio).max(32.0) as u32;
    let ranked = rank::extract(&preprocessed, target_tokens);

    // 5. Brevity rules (kind-gated)
    let brevity_on = matches!(
        input.kind,
        CompressKind::ToolOutput | CompressKind::ProjectNotes
    );
    let after_brevity = if brevity_on && tokens::count(&ranked) > target_tokens {
        brevity::strip_fillers(&ranked)
    } else {
        ranked
    };

    // 6. Stamp
    let comp_tokens = tokens::count(&after_brevity);
    let stamped = sentinel::stamp(
        &after_brevity,
        &orig_hash,
        orig_tokens,
        comp_tokens,
        input.kind,
    );

    let final_tokens = tokens::count(&stamped);
    CompressResult {
        text: stamped,
        original_tokens: orig_tokens,
        compressed_tokens: final_tokens,
        algorithm_id: ALGO_ID,
        original_hash: orig_hash,
    }
}

fn normalize_whitespace(s: &str) -> String {
    // Collapse runs of spaces/tabs, trim trailing whitespace on each line,
    // drop trailing empty lines. Preserve paragraph structure.
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let mut last_space = false;
        let mut buf = String::with_capacity(line.len());
        for ch in line.chars() {
            if ch == ' ' || ch == '\t' {
                if !last_space {
                    buf.push(' ');
                    last_space = true;
                }
            } else {
                buf.push(ch);
                last_space = false;
            }
        }
        out.push_str(buf.trim_end());
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

fn hash_bytes(b: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b);
    h.finalize().into()
}

pub(crate) fn hex_encode(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() * 2);
    for byte in b {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_input() -> String {
        let mut s = String::new();
        for i in 0..200 {
            s.push_str(&format!(
                "Sentence number {i} talks about the engraph project and its features.\n"
            ));
        }
        s.push_str("Critical decision: use SQLite for storage.\n");
        for i in 0..200 {
            s.push_str(&format!(
                "Filler sentence {i} about unrelated weather and cats.\n"
            ));
        }
        s
    }

    #[test]
    fn idempotent_on_second_pass() {
        let inp = long_input();
        let r1 = compress(CompressInput::new(&inp, CompressKind::ProjectNotes));
        let r2 = compress(CompressInput::new(&r1.text, CompressKind::ProjectNotes));
        assert_eq!(r1.text, r2.text, "second pass must be no-op");
        assert_eq!(r1.compressed_tokens, r2.compressed_tokens);
        // original_hash differs by design on second pass: it hashes the input bytes,
        // and the input on pass 2 is r1.text. The provenance hash of the underlying
        // original is recorded in r1's trailer.
    }

    #[test]
    fn sentinel_marker_present_after_compress() {
        let r = compress(CompressInput::new(&long_input(), CompressKind::ProjectNotes));
        assert!(is_compressed(&r.text));
    }

    #[test]
    fn short_input_still_idempotent() {
        let r1 = compress(CompressInput::new("short.", CompressKind::Generic));
        let r2 = compress(CompressInput::new(&r1.text, CompressKind::Generic));
        assert_eq!(r1.text, r2.text);
    }

    #[test]
    fn ratio_under_one_for_long_input() {
        let inp = long_input();
        let r = compress(CompressInput::new(&inp, CompressKind::ProjectNotes));
        assert!(
            r.compressed_tokens < r.original_tokens,
            "expected reduction, got {} -> {}",
            r.original_tokens,
            r.compressed_tokens
        );
    }

    #[test]
    fn empty_does_not_panic() {
        let r = compress(CompressInput::new("", CompressKind::Generic));
        assert_eq!(r.original_tokens, 0);
        // Even empty content gets stamped — re-compressing the stamp must be a no-op
        let r2 = compress(CompressInput::new(&r.text, CompressKind::Generic));
        assert_eq!(r.text, r2.text);
    }
}
