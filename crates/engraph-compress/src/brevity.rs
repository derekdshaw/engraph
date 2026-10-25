//! Caveman-style brevity: drop articles and filler words deterministically.
//! Applied only when extractive ranking alone hasn't hit the target budget,
//! and only for kinds where verbatim fidelity isn't required.

const FILLERS: &[&str] = &[
    "just",
    "really",
    "very",
    "actually",
    "basically",
    "literally",
    "simply",
    "quite",
    "rather",
    "somewhat",
    "perhaps",
    "maybe",
];

/// Delete articles and fillers in place; every other byte is copied verbatim.
/// A word is dropped only when it stands alone in prose (start of text or after
/// whitespace, followed by a space, outside backticks), so identifiers like
/// `foo(a, b)` or `a.b` and code spans survive.
pub(crate) fn strip_fillers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code = false;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if !is_word_char(c) {
            if c == '`' {
                in_code = !in_code;
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let end = rest.find(|ch| !is_word_char(ch)).unwrap_or(rest.len());
        let (word, after) = rest.split_at(end);
        let standalone = out.is_empty() || out.ends_with(char::is_whitespace);
        if !in_code && standalone && after.starts_with(' ') && is_filler(word) {
            rest = &after[1..];
            continue;
        }
        out.push_str(word);
        rest = after;
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '\''
}

fn is_filler(word: &str) -> bool {
    let lower = word.to_lowercase();
    matches!(lower.as_str(), "a" | "an" | "the") || FILLERS.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_articles_and_fillers() {
        let s = "The really very simple sentence.";
        let out = strip_fillers(s);
        let lower = out.to_lowercase();
        assert!(!lower.contains("the "));
        assert!(!lower.contains("really"));
        assert!(!lower.contains("very"));
        assert!(lower.contains("simple"));
    }

    #[test]
    fn leaves_everything_else_byte_identical() {
        let s = "Here's what's really hooked in:\n\
                 - `SessionStart` → `engraph hook session-start` (injects the brief)\n\
                 - `engraph ingest --from-stdin` reads foo(a, b) and a.b\n\
                 \tthe `the` value\n";
        assert_eq!(
            strip_fillers(s),
            "Here's what's hooked in:\n\
             - `SessionStart` → `engraph hook session-start` (injects brief)\n\
             - `engraph ingest --from-stdin` reads foo(a, b) and a.b\n\
             \t`the` value\n"
        );
    }

    #[test]
    fn preserves_keywords() {
        let s = "engraph compress events";
        let out = strip_fillers(s);
        assert!(out.contains("engraph"));
        assert!(out.contains("compress"));
        assert!(out.contains("events"));
    }
}
