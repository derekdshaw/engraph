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

fn ctx<'a>(args: &'a [String], stdout: &'a str) -> FilterCtx<'a> {
    FilterCtx {
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
    let out = filter(&ctx(&args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.3, "git_log ratio {r:.3} >= 0.3");
    assert!(out.text.contains("[engraph: 50 commits]"));
}

#[test]
fn cargo_test_drops_passes() {
    let mut stdout = String::from("running 100 tests\n");
    for i in 0..100 {
        stdout.push_str(&format!(
            "test some::very::nested::module::test_function_{i} ... ok\n"
        ));
    }
    stdout.push_str("\ntest result: ok. 100 passed; 0 failed; 0 ignored\n");
    let args = vec!["test".to_string()];
    let (filter, _) = filters::pick("cargo", &args);
    let out = filter(&ctx(&args, &stdout));
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
    let out = filter(&ctx(&args, stdout));
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
    let out = filter(&ctx(&args, input));
    assert!(
        out.text.contains("aaaaaaa Fix something"),
        "missing first commit subject in {out:?}",
        out = out.text
    );
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
    let out = filter(&ctx(&args, input));
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
    let out = filter(&ctx(&args, stdout));
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
    let out = filter(&ctx(&args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.5, "tree ratio {r:.3} >= 0.5");
}

#[test]
fn fd_truncates_long_lists() {
    let input: String = (0..400).map(|i| format!("src/path/file{i}.rs\n")).collect();
    let args = vec![];
    let (filter, _) = filters::pick("fd", &args);
    let out = filter(&ctx(&args, &input));
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
    let out = filter(&ctx(&args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.2, "npm_install ratio {r:.3} >= 0.2");
    assert!(out.text.contains("added 200 packages in 12s"));
}

#[test]
fn pytest_drops_pass_progress() {
    let mut stdout = String::new();
    for i in 0..200 {
        stdout.push_str(&format!(
            "tests/test_unit.py::test_case_{i} PASSED                                              [{}%]\n",
            i / 2
        ));
    }
    stdout.push_str("=================== 200 passed in 1.23s =====================\n");
    let args = vec!["-q".to_string()];
    let (filter, _) = filters::pick("pytest", &args);
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.3, "pytest ratio {r:.3} >= 0.3");
    assert!(out.text.contains("200 passed"));
}

#[test]
fn pip_install_drops_collecting_lines() {
    let mut stdout = String::new();
    for i in 0..100 {
        stdout.push_str(&format!("Collecting package-{i}\n"));
        stdout.push_str(&format!(
            "  Downloading package_{i}-1.0.0-py3-none-any.whl (50 kB)\n"
        ));
    }
    stdout.push_str("Installing collected packages: package-0, package-1\n");
    stdout.push_str("Successfully installed package-0-1.0.0 package-1-1.0.0\n");
    let args = vec!["install".to_string()];
    let (filter, _) = filters::pick("pip", &args);
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.2, "pip_install ratio {r:.3} >= 0.2");
    assert!(out.text.contains("Successfully installed"));
}

#[test]
fn go_test_caps_summary() {
    let mut stdout = String::new();
    for i in 0..50 {
        stdout.push_str(&format!("=== RUN   TestX{i}\n--- PASS: TestX{i} (0.00s)\n"));
    }
    stdout.push_str("PASS\nok  \texample.com/m\t0.05s\n");
    let args = vec!["test".to_string()];
    let (filter, _) = filters::pick("go", &args);
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.3, "go_test ratio {r:.3} >= 0.3");
    assert!(out.text.contains("1 ok, 0 failed pkgs"));
}

#[test]
fn yarn_install_drops_resolution_spam() {
    let mut stdout = String::new();
    for stage in ["Resolving", "Fetching", "Linking", "Building"] {
        for i in 0..30 {
            stdout.push_str(&format!("[{i}/30] {stage} packages...\n"));
        }
    }
    stdout.push_str("Done in 4.2s\n");
    let args = vec!["install".to_string()];
    let (filter, _) = filters::pick("yarn", &args);
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.2, "yarn_install ratio {r:.3} >= 0.2");
    assert!(out.text.contains("Done in 4.2s"));
}

#[test]
fn docker_ps_truncates_long_table() {
    let input: String = (0..150)
        .map(|i| format!("container-{i}  running\n"))
        .collect();
    let args = vec!["ps".to_string()];
    let (filter, _) = filters::pick("docker", &args);
    let out = filter(&ctx(&args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.8, "docker_ps ratio {r:.3} >= 0.8");
    assert!(out.text.contains("truncated 50 more rows"));
}

#[test]
fn kubectl_describe_drops_spec_annotations() {
    let mut stdout = String::from("Name: pod\nStatus: Running\nSpec:\n");
    for i in 0..50 {
        stdout.push_str(&format!("  field{i}: value{i}\n"));
    }
    stdout.push_str("Annotations:\n");
    for i in 0..50 {
        stdout.push_str(&format!("  ann/k{i}: vvv{i}\n"));
    }
    stdout.push_str("Events:\n  Normal Pulled\n");
    let args = vec!["describe".to_string()];
    let (filter, _) = filters::pick("kubectl", &args);
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.4, "kubectl_describe ratio {r:.3} >= 0.4");
    assert!(out.text.contains("Events:"));
    assert!(!out.text.contains("field49"));
}

#[test]
fn make_drops_dir_echoes() {
    let mut stdout = String::new();
    for i in 0..80 {
        stdout.push_str(&format!("make[1]: Entering directory '/build/sub{i}'\n"));
        stdout.push_str(&format!("cc -c -o file{i}.o file{i}.c\n"));
        stdout.push_str(&format!("make[1]: Leaving directory '/build/sub{i}'\n"));
    }
    stdout.push_str("file42.c:10: error: 'x' undeclared\n");
    let args = vec![];
    let (filter, _) = filters::pick("make", &args);
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.1, "make ratio {r:.3} >= 0.1");
    assert!(out.text.contains("'x' undeclared"));
}

#[test]
fn rg_truncates_long_match_lists() {
    let input: String = (0..400)
        .map(|i| format!("src/file{i}.rs:10:42:match-here\n"))
        .collect();
    let args = vec!["pattern".to_string()];
    let (filter, _) = filters::pick("rg", &args);
    let out = filter(&ctx(&args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.6, "rg ratio {r:.3} >= 0.6");
    assert!(out.text.contains("truncated 200 more matches"));
}

#[test]
fn rg_groups_multi_match_files() {
    // Many matches per file: the repeated path prefix is the redundancy the
    // group-by-file heading removes. 10 files * 15 hits = 150 (< the 200 cap).
    let mut input = String::new();
    for f in 0..10 {
        for m in 0..15 {
            input.push_str(&format!(
                "crates/engraph-compress/src/filters/file{f:02}.rs:{m}:    handler_call_site_{m}();\n"
            ));
        }
    }
    let args = vec!["handler".to_string()];
    let (filter, _) = filters::pick("rg", &args);
    let out = filter(&ctx(&args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.6, "rg grouped ratio {r:.3} >= 0.6");
    // Each path heading appears exactly once despite 15 matches.
    assert_eq!(out.text.matches("file00.rs").count(), 1);
}

#[test]
fn git_status_strips_boilerplate() {
    let mut input = String::from(
        "On branch main\nYour branch is up to date with 'origin/main'.\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n  (use \"git restore <file>...\" to discard changes in working directory)\n",
    );
    // A realistic working set is a handful of files, where the hint/branch
    // boilerplate is a large fraction of the output.
    for i in 0..4 {
        input.push_str(&format!("\tmodified:   src/file_{i}.rs\n"));
    }
    input.push_str("\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tnew_thing.rs\n\nno changes added to commit (use \"git add\" and/or \"git commit -a\")\n");
    let args = vec!["status".to_string()];
    let (filter, _) = filters::pick("git", &args);
    let out = filter(&ctx(&args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.65, "git_status ratio {r:.3} >= 0.65");
    assert!(!out.text.contains("(use \""));
    assert!(out.text.contains("modified:   src/file_0.rs"));
}

#[test]
fn cargo_test_strips_backtraces_on_failures() {
    // A failing run used to be ~no-op (only `... ok` lines were dropped). The
    // backtrace frames are the bulk; strip them, keep panic header + message.
    let mut stdout = String::from("running 3 tests\n");
    for t in 0..3 {
        stdout.push_str(&format!("test mod::case_{t} ... FAILED\n"));
    }
    stdout.push_str("\nfailures:\n\n");
    for t in 0..3 {
        stdout.push_str(&format!("---- mod::case_{t} stdout ----\n\n"));
        stdout.push_str(&format!(
            "thread 'mod::case_{t}' panicked at src/x.rs:{t}:9:\n"
        ));
        stdout.push_str("assertion failed: something\n");
        stdout.push_str("stack backtrace:\n");
        for f in 0..12 {
            stdout.push_str(&format!("  {f}: core::some::deep::frame_{f}\n"));
            stdout.push_str("             at /rustc/abc/library/core/src/panicking.rs:80:14\n");
        }
        stdout.push_str("note: Some details are omitted, run with `RUST_BACKTRACE=full`.\n\n");
    }
    stdout.push_str("test result: FAILED. 0 passed; 3 failed; 0 ignored\n");
    let args = vec!["test".to_string()];
    let (filter, _) = filters::pick("cargo", &args);
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.4, "cargo_test failing-run ratio {r:.3} >= 0.4");
    assert!(out.text.contains("panicked at src/x.rs:0:9"));
    assert!(!out.text.contains("stack backtrace"));
}

#[test]
fn cargo_build_collapses_warnings() {
    let mut stderr = String::new();
    for i in 0..20 {
        stderr.push_str(&format!("warning: unused variable: `v{i}`\n"));
        stderr.push_str(&format!(" --> src/lib.rs:{i}:9\n"));
        stderr.push_str("  |\n");
        stderr.push_str(&format!(
            "{i} | pub fn f() {{ let v{i} = compute_something(); }}\n"
        ));
        stderr.push_str("  |                   ^ help: prefix with underscore\n");
        stderr.push_str("  |\n");
        stderr.push_str("  = note: `#[warn(unused_variables)]` on by default\n\n");
    }
    stderr.push_str("    Finished `dev` profile in 2.0s\n");
    let args = vec!["build".to_string()];
    let (filter, _) = filters::pick("cargo", &args);
    let out = filter(&FilterCtx {
        args: &args,
        stdout: "",
        stderr: &stderr,
        exit_code: 0,
    });
    let r = ratio(&stderr, &out.text);
    assert!(r < 0.4, "cargo_build warning-heavy ratio {r:.3} >= 0.4");
    assert!(out.text.contains("warning: unused variable: `v0`"));
    assert!(!out.text.contains("= note"));
}

#[test]
fn find_groups_by_directory() {
    // Many files sharing deep prefixes: the repeated dir is the redundancy.
    let mut input = String::new();
    for d in 0..8 {
        for f in 0..12 {
            input.push_str(&format!(
                "./crates/engraph-compress/src/dir{d}/file_{f}.rs\n"
            ));
        }
    }
    let args = vec![".".to_string()];
    let (filter, id) = filters::pick("find", &args);
    assert_eq!(id, "find");
    let out = filter(&ctx(&args, &input));
    let r = ratio(&input, &out.text);
    assert!(r < 0.5, "find ratio {r:.3} >= 0.5");
}

#[test]
fn git_push_drops_transfer_progress() {
    let stderr = "\
Enumerating objects: 12, done.
Counting objects: 100% (12/12), done.
Delta compression using up to 8 threads
Compressing objects: 100% (6/6), done.
Writing objects: 100% (7/7), 1.2 KiB | 1.2 MiB/s, done.
Total 7 (delta 3), reused 0 (delta 0), pack-reused 0
remote: Resolving deltas: 100% (3/3), completed with 2 local objects.
To github.com:user/repo.git
   abc1234..def5678  main -> main
";
    let args = vec!["push".to_string()];
    let (filter, id) = filters::pick("git", &args);
    assert_eq!(id, "git_push");
    let out = filter(&FilterCtx {
        args: &args,
        stdout: "",
        stderr,
        exit_code: 0,
    });
    let r = ratio(stderr, &out.text);
    assert!(r < 0.4, "git_push ratio {r:.3} >= 0.4");
    assert!(out.text.contains("To github.com:user/repo.git"));
    assert!(out.text.contains("abc1234..def5678  main -> main"));
    assert!(!out.text.contains("Writing objects"));
    assert!(!out.text.contains("Resolving deltas"));
}

#[test]
fn bazel_build_drops_progress() {
    let mut stderr = String::from(
        "Starting local Bazel server and connecting to it...\nINFO: Analyzed 42 targets (118 packages loaded).\nINFO: Found 42 targets...\n",
    );
    for i in 0..150 {
        stderr.push_str(&format!(
            "[{i},234 / 5,678] Compiling src/file{i}.cc; {i}s linux-sandbox\n"
        ));
    }
    stderr.push_str("ERROR: /w/pkg/BUILD.bazel:1:1: Compiling failed: (Exit 1)\nsrc/foo.cc:5:10: error: 'x' was not declared\nERROR: Build did NOT complete successfully\n");
    let args = vec!["build".to_string()];
    let (filter, id) = filters::pick("bazel", &args);
    assert_eq!(id, "bazel_build");
    let out = filter(&FilterCtx {
        args: &args,
        stdout: "",
        stderr: &stderr,
        exit_code: 1,
    });
    let r = ratio(&stderr, &out.text);
    assert!(r < 0.3, "bazel_build ratio {r:.3} >= 0.3");
    assert!(out.text.contains("error: 'x' was not declared"));
    assert!(!out.text.contains("Compiling src/file0.cc"));
}

#[test]
fn bazel_test_drops_passed_targets() {
    let mut stdout = String::new();
    for i in 0..150 {
        stdout.push_str(&format!("//pkg:target_{i}              PASSED in 0.{i}s\n"));
    }
    stdout.push_str(
        "//pkg:broken                  FAILED in 1.2s\n  /home/u/.cache/bazel/test.log\n",
    );
    stdout.push_str("Executed 151 out of 151 tests: 150 tests pass and 1 fails locally.\n");
    let args = vec!["test".to_string()];
    let (filter, id) = filters::pick("bazel", &args);
    assert_eq!(id, "bazel_test");
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.2, "bazel_test ratio {r:.3} >= 0.2");
    assert!(out.text.contains("//pkg:broken"));
    assert!(out.text.contains("Executed 151 out of 151"));
    assert!(!out.text.contains("//pkg:target_0 "));
}

#[test]
fn go_test_strips_goroutine_dumps() {
    let mut stdout =
        String::from("=== RUN   TestPanics\n--- FAIL: TestPanics (0.00s)\npanic: boom\n\n");
    stdout.push_str("goroutine 19 [running]:\n");
    for f in 0..30 {
        stdout.push_str(&format!("example.com/m.frame_{f}(0x{f:x})\n"));
        stdout.push_str("\t/usr/local/go/src/runtime/panic.go:1545 +0x3e6\n");
    }
    stdout.push_str("exit status 2\nFAIL\texample.com/m\t0.012s\n");
    let args = vec!["test".to_string()];
    let (filter, _) = filters::pick("go", &args);
    let out = filter(&ctx(&args, &stdout));
    let r = ratio(&stdout, &out.text);
    assert!(r < 0.3, "go_test goroutine-dump ratio {r:.3} >= 0.3");
    assert!(out.text.contains("panic: boom"));
    assert!(!out.text.contains("goroutine 19"));
}

#[test]
fn unknown_command_falls_back_to_generic() {
    let args = vec!["something".to_string()];
    let (_, id) = filters::pick("totally-made-up", &args);
    assert_eq!(id, "generic");
}

#[test]
fn picker_and_filter_output_agree_on_filter_id() {
    // Regression: the picker's id and the FilterOutput.filter_id must match
    // for every wrapped command. A drift here was the cargo_check / cargo_build
    // mismatch flagged in the v1 review.
    let cases: &[(&str, &[&str], &str)] = &[
        ("cargo", &["build"], "cargo_build"),
        ("cargo", &["check"], "cargo_check"),
        ("cargo", &["clippy"], "cargo_clippy"),
        ("cargo", &["doc"], "cargo_doc"),
        ("cargo", &["test"], "cargo_test"),
        ("cargo", &["bench"], "cargo_bench"),
        ("cargo", &["audit"], "cargo_audit"),
        ("cargo", &["tree"], "cargo_tree"),
        ("git", &["log"], "git_log"),
        ("git", &["diff"], "git_diff"),
        ("git", &["status"], "git_status"),
        ("git", &["show"], "git_show"),
        ("git", &["push"], "git_push"),
        ("git", &["pull"], "git_pull"),
        ("git", &["fetch"], "git_fetch"),
        ("git", &["commit"], "git_commit"),
        ("find", &["."], "find"),
        ("fd", &["foo"], "fd"),
        ("go", &["test"], "go_test"),
        ("go", &["mod", "download"], "go_mod_download"),
        ("bazel", &["build"], "bazel_build"),
        ("bazel", &["test"], "bazel_test"),
        ("bazel", &["query"], "bazel_query"),
        ("bazelisk", &["build"], "bazel_build"),
    ];
    for (cmd, args, expected) in cases {
        let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (filter_fn, picker_id) = filters::pick(cmd, &args_owned);
        assert_eq!(
            picker_id, *expected,
            "picker id mismatch for {cmd} {args:?}"
        );
        let out = filter_fn(&ctx(&args_owned, ""));
        assert_eq!(
            out.filter_id, picker_id,
            "FilterOutput.filter_id ({}) != picker id ({}) for {cmd} {args:?}",
            out.filter_id, picker_id,
        );
    }
}
