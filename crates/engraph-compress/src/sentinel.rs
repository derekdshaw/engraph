pub const SENTINEL: &str = "<<engraph:v1:compressed>>";
pub const ALGO_ID: &str = "v1";

pub fn is_compressed(text: &str) -> bool {
    text.starts_with(SENTINEL)
}

/// Wrap `body` with the sentinel prefix only. Provenance (hash, token counts,
/// kind, algorithm id) lives in `CompressResult` and is persisted by the
/// caller adjacent to the compressed text (e.g. `messages.content_hash`,
/// `content_compressed`). An in-band trailer would be indistinguishable from
/// arbitrary content and so isn't emitted.
pub(crate) fn stamp(body: &str) -> String {
    let body = body.trim_end_matches('\n');
    if body.is_empty() {
        return format!("{SENTINEL}\n");
    }
    format!("{SENTINEL}\n{body}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_marker() {
        assert!(is_compressed("<<engraph:v1:compressed>>\nhello"));
        assert!(!is_compressed("plain text"));
        assert!(!is_compressed(""));
    }

    #[test]
    fn stamped_starts_with_sentinel() {
        let s = stamp("body");
        assert!(s.starts_with(SENTINEL));
        assert!(s.contains("body"));
        assert!(!s.contains("[engraph:hash"));
    }

    #[test]
    fn empty_body_still_marked() {
        let s = stamp("");
        assert_eq!(s, format!("{SENTINEL}\n"));
        assert!(is_compressed(&s));
    }
}
