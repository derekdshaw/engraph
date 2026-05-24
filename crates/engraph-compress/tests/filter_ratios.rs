//! Per-filter token-reduction verification gates. Each test feeds a
//! representative input to a filter and asserts the output/input token ratio
//! sits below the documented threshold.

use engraph_compress::filters::{self, FilterCtx};
use engraph_core::tokens;

fn ratio(input: &str, output: &str) -> f32 {
    let i = tokens::count(input).max(1);
    let o = tokens::count(output);
    o as f32 / i as f32
}

fn ctx<'a>(cmd: &'a str, args: &'a [String], stdout: &'a str) -> FilterCtx<'a> {
    FilterCtx {
        cmd,
        args,
        stdout,
        stderr: "",
        exit_code: 0,
    }
}

#[test]
fn git_log_under_quarter() {
    let mut input = String::new();
    for i in 0..50 {
        let hash = format!("{:040x}", i * 0xabcdef_usize);
        input.push_str(&format!(
            "commit {hash}\nAuthor: Dev <d@x.io>\nDate: Mon Jan 0{d} 12:00:00 2026 -0700\n\n    subject line {i}\n    body paragraph that should be dropped\n    another body line\n\n",
            d = (i % 9) + 1,
        ));
    }
    let args = vec!["log".to_string()];
    let (filter, _) = filters::pick("git", &args);
    let out = filter(&ctx("git", &args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.3, "git_log ratio {r:.3} >= 0.3");
    assert!(out.text.contains("[engraph: 50 commits]"));
}

#[test]
fn cargo_test_drops_passes() {
    let mut stdout = String::from("running 100 tests\n");
    for i in 0..100 {
        stdout.push_str(&format!("test some::very::nested::module::test_function_{i} ... ok\n"));
    }
    stdout.push_str("\ntest result: ok. 100 passed; 0 failed; 0 ignored\n");
    let args = vec!["test".to_string()];
    let (filter, _) = filters::pick("cargo", &args);
    let out = filter(&ctx("cargo", &args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.2, "cargo_test ratio {r:.3} >= 0.2");
    assert!(out.text.contains("tests 100 (passed 100"));
}

#[test]
fn cargo_build_drops_compiling_lines() {
    let mut stderr = String::new();
    for i in 0..80 {
        stderr.push_str(&format!("   Compiling crate-{i} v0.{i}.0\n"));
    }
    stderr.push_str("warning: unused variable: `x`\n   --> src/lib.rs:10:9\n");
    stderr.push_str("    Finished `dev` profile [unoptimized] target(s) in 5.43s\n");
    let args = vec!["build".to_string()];
    let (filter, _) = filters::pick("cargo", &args);
    let out = filter(&FilterCtx {
        cmd: "cargo",
        args: &args,
        stdout: "",
        stderr: &stderr,
        exit_code: 0,
    });
    let r = ratio(&stderr, &out.text);
    assert!(r < 0.3, "cargo_build ratio {r:.3} >= 0.3");
    assert!(!out.text.contains("Compiling"));
    assert!(out.text.contains("warning"));
}

#[test]
fn npm_install_keeps_summary() {
    let stdout = "\
added 1 package\nadded 1 package\nadded 1 package\nadded 1 package\nadded 1 package\nadded 1 package\nadded 1 package\nadded 1 package\nadded 1 package\nadded 1 package\nadded 12 packages in 4s\n\nfound 0 vulnerabilities\n";
    let args = vec!["install".to_string()];
    let (filter, _) = filters::pick("npm", &args);
    let out = filter(&ctx("npm", &args, stdout));
    assert!(out.text.contains("added 12 packages in 4s"));
    assert!(out.text.contains("0 vulnerabilities"));
}

#[test]
fn unknown_command_falls_back_to_generic() {
    let args = vec!["something".to_string()];
    let (_, id) = filters::pick("totally-made-up", &args);
    assert_eq!(id, "generic");
}
