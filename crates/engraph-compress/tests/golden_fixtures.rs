//! Byte-exact golden snapshot tests for high-signal filters. Each case loads
//! `<name>.in.txt` and `<name>.expected.txt` from `tests/fixtures/` and asserts
//! the filter output matches verbatim. Catches accidental output-format drift
//! that the ratio-based tests would miss.

use engraph_compress::filters::{self, FilterCtx};
use std::path::PathBuf;

#[derive(Copy, Clone)]
enum Stream {
    Stdout,
    Stderr,
}

struct Case<'a> {
    name: &'a str,
    cmd: &'a str,
    args: &'a [&'a str],
    stream: Stream,
    exit_code: i32,
}

const CASES: &[Case<'static>] = &[
    Case {
        name: "git_log_basic",
        cmd: "git",
        args: &["log"],
        stream: Stream::Stdout,
        exit_code: 0,
    },
    Case {
        name: "cargo_check_basic",
        cmd: "cargo",
        args: &["check"],
        stream: Stream::Stderr,
        exit_code: 0,
    },
    Case {
        name: "cargo_test_nextest",
        cmd: "cargo",
        args: &["test"],
        stream: Stream::Stderr,
        exit_code: 1,
    },
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load(name: &str, suffix: &str) -> String {
    let path = fixtures_dir().join(format!("{name}.{suffix}.txt"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn golden_fixtures_byte_exact() {
    let mut failures = Vec::new();
    for case in CASES {
        let input = load(case.name, "in");
        let expected = load(case.name, "expected");
        let args: Vec<String> = case.args.iter().map(|s| s.to_string()).collect();
        let (stdout, stderr) = match case.stream {
            Stream::Stdout => (input.as_str(), ""),
            Stream::Stderr => ("", input.as_str()),
        };
        let (filter_fn, _) = filters::pick(case.cmd, &args);
        let out = filter_fn(&FilterCtx {
            cmd: case.cmd,
            args: &args,
            stdout,
            stderr,
            exit_code: case.exit_code,
        });
        if out.text != expected {
            failures.push(format!(
                "fixture `{}` mismatch.\n--- expected ---\n{expected}--- got ---\n{got}",
                case.name,
                got = out.text,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} golden fixture(s) drifted:\n\n{}",
        failures.len(),
        failures.join("\n\n"),
    );
}

#[test]
fn unknown_fixture_name_is_a_test_authoring_error() {
    // Negative: trying to load a non-existent fixture must panic, not
    // silently pass. Guards against typos in the CASES table.
    let result = std::panic::catch_unwind(|| {
        let _ = load("does_not_exist_xyz", "in");
    });
    assert!(result.is_err(), "loading a missing fixture must panic");
}
