use crate::{hex_encode, CompressKind};

pub const SENTINEL: &str = "<<engraph:v1:compressed>>";
pub const ALGO_ID: &str = "v1";

pub fn is_compressed(text: &str) -> bool {
    text.starts_with(SENTINEL)
}

pub(crate) fn stamp(
    body: &str,
    orig_hash: &[u8; 32],
    orig_tokens: u32,
    comp_tokens: u32,
    kind: CompressKind,
) -> String {
    let kind_str = match kind {
        CompressKind::ProjectNotes => "project_notes",
        CompressKind::SessionMessage => "session_message",
        CompressKind::ToolOutput => "tool_output",
        CompressKind::Generic => "generic",
    };
    let trailer = format!(
        "\n[engraph:hash={hash},algo={ALGO_ID},kind={kind_str},orig_tokens={orig_tokens},comp_tokens={comp_tokens}]\n",
        hash = hex_encode(orig_hash),
    );
    let body = body.trim_end_matches('\n');
    format!("{SENTINEL}\n{body}{trailer}")
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
        let s = stamp("body", &[0u8; 32], 100, 40, CompressKind::Generic);
        assert!(s.starts_with(SENTINEL));
        assert!(s.contains("orig_tokens=100"));
        assert!(s.contains("comp_tokens=40"));
    }
}
