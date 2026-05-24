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
fn git_log_graph_handles_decoration() {
    let input = "\
* commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
| Author: Dev <d@x>
| Date: Mon Jan 1 12:00:00 2026 -0700
|
|     Fix something
|
| * commit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
|/  Author: Dev <d@x>
|   Date: Mon Jan 1 12:00:00 2026 -0700
|
|       Add tests
";
    let args = vec!["log".to_string(), "--graph".to_string()];
    let (filter, _) = filters::pick("git", &args);
    let out = filter(&ctx("git", &args, input));
    assert!(out.text.contains("aaaaaaa Fix something"), "missing first commit subject in {out:?}", out = out.text);
    assert!(out.text.contains("2 commits"));
}

#[test]
fn git_diff_stat_passes_through() {
    let input = "\
 foo.rs  | 4 ++--
 bar.rs  | 2 +-
 2 files changed, 3 insertions(+), 3 deletions(-)
";
    let args = vec!["diff".to_string(), "--stat".to_string()];
    let (filter, _) = filters::pick("git", &args);
    let out = filter(&ctx("git", &args, input));
    assert!(out.text.contains("foo.rs"));
    assert!(out.text.contains("2 files changed"));
}

#[test]
fn cargo_test_counts_failures_once() {
    let stdout = "\
running 5 tests
test foo::a ... ok
test foo::b ... FAILED
test foo::c ... ok
test foo::d ... FAILED
test foo::e ... ok

failures:

---- foo::b stdout ----
panicked at b
---- foo::d stdout ----
panicked at d

test result: FAILED. 3 passed; 2 failed; 0 ignored
";
    let args = vec!["test".to_string()];
    let (filter, _) = filters::pick("cargo", &args);
    let out = filter(&ctx("cargo", &args, stdout));
    assert!(
        out.text.contains("failed 2,"),
        "expected exactly 2 failures, got: {}",
        out.text,
    );
}

#[test]
fn tree_ratio_below_half() {
    let mut input = String::new();
    for d1 in 0..10 {
        input.push_str(&format!("├── dir{d1}\n"));
        for d2 in 0..5 {
            input.push_str(&format!("│   ├── sub{d2}\n"));
            for d3 in 0..5 {
                input.push_str(&format!("│   │   ├── deep{d3}\n"));
                for d4 in 0..5 {
                    input.push_str(&format!("│   │   │   ├── deeper{d4}\n"));
                }
            }
        }
    }
    let args = vec![];
    let (filter, _) = filters::pick("tree", &args);
    let out = filter(&ctx("tree", &args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.5, "tree ratio {r:.3} >= 0.5");
}

#[test]
fn fd_truncates_long_lists() {
    let input: String = (0..400)
        .map(|i| format!("src/path/file{i}.rs\n"))
        .collect();
    let args = vec![];
    let (filter, _) = filters::pick("fd", &args);
    let out = filter(&ctx("fd", &args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.6, "fd ratio {r:.3} >= 0.6");
    assert!(out.text.contains("truncated"));
}

#[test]
fn npm_install_ratio_under_half() {
    let mut input = String::new();
    for _ in 0..200 {
        input.push_str("added 1 package\n");
    }
    input.push_str("added 200 packages in 12s\n");
    input.push_str("found 0 vulnerabilities\n");
    let args = vec!["install".to_string()];
    let (filter, _) = filters::pick("npm", &args);
    let out = filter(&ctx("npm", &args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.2, "npm_install ratio {r:.3} >= 0.2");
    assert!(out.text.contains("added 200 packages in 12s"));
}

#[test]
fn unknown_command_falls_back_to_generic() {
    let args = vec!["something".to_string()];
    let (_, id) = filters::pick("totally-made-up", &args);
    assert_eq!(id, "generic");
}
