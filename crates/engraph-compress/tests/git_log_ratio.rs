//! Verification gate from the plan:
//! "Token reduction: on a 2k-line `git log` fixture, ratio < 0.5"

use engraph_compress::{CompressInput, CompressKind, compress};

fn synthetic_git_log(commits: usize) -> String {
    let mut s = String::new();
    for i in 0..commits {
        let hash = format!("{:040x}", i.wrapping_mul(0xdeadbeef_u64 as usize));
        s.push_str(&format!("commit {hash}\n"));
        s.push_str(&format!(
            "Author: Developer <dev@example.com>\nDate:   Mon Jan 0{day} 12:00:00 2026 -0700\n\n",
            day = (i % 9) + 1,
        ));
        let topic = match i % 5 {
            0 => "refactor parser to handle whitespace",
            1 => "fix off-by-one in counter loop",
            2 => "add test coverage for edge case",
            3 => "update dependency version",
            _ => "tweak logging output formatting",
        };
        s.push_str(&format!("    {topic}\n\n"));
    }
    s
}

#[test]
fn git_log_2k_lines_compresses_below_half() {
    let log = synthetic_git_log(400); // 400 commits * ~5 lines = 2k lines
    assert!(log.lines().count() >= 2000, "fixture must be >= 2k lines");

    let r = compress(CompressInput {
        text: &log,
        kind: CompressKind::ToolOutput,
        target_ratio: 0.4,
        brevity: true,
    });

    assert!(
        r.ratio() < 0.5,
        "ratio {:.3} >= 0.5 (orig {} → comp {})",
        r.ratio(),
        r.original_tokens,
        r.compressed_tokens
    );
}

#[test]
fn deterministic_on_same_input() {
    let log = synthetic_git_log(50);
    let a = compress(CompressInput {
        text: &log,
        kind: CompressKind::ToolOutput,
        target_ratio: 0.4,
        brevity: true,
    });
    let b = compress(CompressInput {
        text: &log,
        kind: CompressKind::ToolOutput,
        target_ratio: 0.4,
        brevity: true,
    });
    assert_eq!(a.text, b.text);
    assert_eq!(a.original_hash, b.original_hash);
}
