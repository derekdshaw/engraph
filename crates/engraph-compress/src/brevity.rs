//! Caveman-style brevity: drop articles and filler words deterministically.
//! Applied only when extractive ranking alone hasn't hit the target budget,
//! and only for kinds where verbatim fidelity isn't required.

use crate::stopwords::stopwords;

const FILLERS: &[&str] = &[
    "just", "really", "very", "actually", "basically", "literally", "simply", "quite",
    "rather", "somewhat", "perhaps", "maybe",
];

pub(crate) fn strip_fillers(text: &str) -> String {
    let fillers: std::collections::HashSet<&'static str> = FILLERS.iter().copied().collect();
    let stop = stopwords();

    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut first = true;
        for tok in tokenize_keeping_punct(line) {
            let lower = tok.to_lowercase();
            // Drop pure articles ("a", "an", "the") and filler words
            if lower == "a" || lower == "an" || lower == "the" || fillers.contains(lower.as_str())
            {
                continue;
            }
            // Keep other stopwords; they preserve grammar
            let _ = stop; // referenced for future tuning
            if !first && !tok.starts_with(|c: char| c.is_ascii_punctuation()) {
                out.push(' ');
            }
            out.push_str(&tok);
            first = false;
        }
        out.push('\n');
    }
    out
}

fn tokenize_keeping_punct(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    for ch in line.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            buf.push(ch);
        } else {
            if !buf.is_empty() {
                tokens.push(std::mem::take(&mut buf));
            }
            if !ch.is_whitespace() {
                tokens.push(ch.to_string());
            }
        }
    }
    if !buf.is_empty() {
        tokens.push(buf);
    }
    tokens
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
    fn preserves_keywords() {
        let s = "engraph compress events";
        let out = strip_fillers(s);
        assert!(out.contains("engraph"));
        assert!(out.contains("compress"));
        assert!(out.contains("events"));
    }
}
