//! Deterministic extractive sentence ranking based on term frequency.
//!
//! For each sentence, score = sum of (frequency of non-stopword terms across
//! the whole document, for terms in this sentence) / sqrt(sentence_word_count).
//! Pick highest-scoring sentences in document order until target token budget hit.

use crate::stopwords::stopwords;
use engraph_core::tokens;

pub(crate) fn extract(text: &str, target_tokens: u32) -> String {
    let sentences = split_sentences(text);
    if sentences.is_empty() {
        return String::new();
    }

    // Tokenize each sentence into lowercased words ONCE. Both the freq table
    // and the per-sentence scorer below iterate this same vector — previously
    // they each called words_lowercase(), doubling the per-word allocations.
    let per_sentence_words: Vec<Vec<String>> =
        sentences.iter().map(|s| words_lowercase(s)).collect();

    let mut freqs: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for words in &per_sentence_words {
        for w in words {
            if stopwords().contains(w.as_str()) {
                continue;
            }
            *freqs.entry(w.as_str()).or_insert(0) += 1;
        }
    }

    let scored: Vec<(usize, f64)> = per_sentence_words
        .iter()
        .enumerate()
        .filter_map(|(i, words)| {
            let kept = words
                .iter()
                .filter(|w| !stopwords().contains(w.as_str()))
                .count();
            if kept == 0 {
                // Drop stopword-only sentences entirely instead of giving them
                // a zero score that ties against real low-frequency content.
                return None;
            }
            let sum: u32 = words
                .iter()
                .filter(|w| !stopwords().contains(w.as_str()))
                .map(|w| *freqs.get(w.as_str()).unwrap_or(&0))
                .sum();
            let denom = (kept as f64).sqrt();
            Some((i, sum as f64 / denom))
        })
        .collect();

    // Sort by descending score; ties broken by original index (lower wins).
    let mut order = scored;
    order.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    let mut keep = vec![false; sentences.len()];
    let mut running = 0_u32;
    for (idx, _score) in order {
        let tk = tokens::count(&sentences[idx]);
        if running + tk > target_tokens && running > 0 {
            continue;
        }
        keep[idx] = true;
        running += tk;
        if running >= target_tokens {
            break;
        }
    }

    let mut out = String::new();
    for (i, kept) in keep.iter().enumerate() {
        if *kept {
            out.push_str(sentences[i].trim());
            out.push('\n');
        }
    }
    out
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        buf.push(ch);
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let trimmed = buf.trim().to_string();
            if !trimmed.is_empty() {
                out.push(trimmed);
            }
            buf.clear();
        }
    }
    let last = buf.trim();
    if !last.is_empty() {
        out.push(last.to_string());
    }
    out
}

fn words_lowercase(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_high_frequency_sentences() {
        let text = "\
            The engraph project uses sqlite. \
            Cats are nice. \
            The engraph compressor is deterministic. \
            Weather is rainy today. \
            Engraph stores telemetry events.\
        ";
        let out = extract(text, 30);
        assert!(out.to_lowercase().contains("engraph"));
        assert!(!out.is_empty());
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(extract("", 100), "");
    }

    #[test]
    fn deterministic() {
        let text = "alpha beta. gamma delta. epsilon zeta. eta theta.";
        let a = extract(text, 10);
        let b = extract(text, 10);
        assert_eq!(a, b);
    }
}
