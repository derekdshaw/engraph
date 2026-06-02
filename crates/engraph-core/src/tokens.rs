use std::sync::OnceLock;
use tiktoken_rs::{CoreBPE, cl100k_base};

fn encoder() -> &'static CoreBPE {
    static ENC: OnceLock<CoreBPE> = OnceLock::new();
    ENC.get_or_init(|| cl100k_base().expect("init cl100k_base"))
}

pub fn count(text: &str) -> u32 {
    encoder().encode_with_special_tokens(text).len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_grow_with_length() {
        let a = count("hello");
        let b = count("hello world from engraph");
        assert!(b > a);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(count(""), 0);
    }
}
