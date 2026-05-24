use std::collections::HashSet;
use std::sync::OnceLock;

pub fn stopwords() -> &'static HashSet<&'static str> {
    static SW: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SW.get_or_init(|| {
        [
            "a", "an", "the", "and", "or", "but", "if", "then", "else", "is", "are", "was",
            "were", "be", "been", "being", "have", "has", "had", "do", "does", "did", "of",
            "to", "in", "on", "at", "by", "for", "with", "as", "this", "that", "these",
            "those", "it", "its", "from", "into", "about", "over", "under", "than", "so",
            "not", "no", "yes", "i", "you", "he", "she", "we", "they", "them", "us", "our",
            "your", "their", "my", "me", "him", "her", "will", "would", "should", "could",
            "can", "may", "might", "must", "just", "also", "very", "really",
        ]
        .into_iter()
        .collect()
    })
}
