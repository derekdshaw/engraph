//! Every filter must surface a failing command's stderr: `engraph run` prints
//! only the filtered text, so a filter that shapes stdout alone would turn
//! `fatal: not a git repository` or rg's `No such file` into empty output.

use engraph_compress::filters::{FilterCtx, pick};

/// One argv per `filters::pick` arm.
const COMMANDS: &[&[&str]] = &[
    &["git", "log"],
    &["git", "log", "--oneline"],
    &["git", "diff"],
    &["git", "diff", "--stat"],
    &["git", "status"],
    &["git", "show"],
    &["git", "push"],
    &["git", "pull"],
    &["git", "fetch"],
    &["git", "commit"],
    &["cargo", "test"],
    &["cargo", "build"],
    &["cargo", "check"],
    &["cargo", "clippy"],
    &["cargo", "doc"],
    &["cargo", "bench"],
    &["cargo", "audit"],
    &["cargo", "tree"],
    &["npm", "install"],
    &["npm", "test"],
    &["pytest"],
    &["pip", "install"],
    &["pip", "list"],
    &["uv", "sync"],
    &["ruff"],
    &["mypy"],
    &["eslint"],
    &["tsc"],
    &["go", "test"],
    &["go", "build"],
    &["go", "vet"],
    &["go", "mod", "tidy"],
    &["go", "mod", "download"],
    &["bazel", "build"],
    &["bazel", "test"],
    &["bazel", "query"],
    &["yarn", "install"],
    &["pnpm", "install"],
    &["jest"],
    &["docker", "ps"],
    &["docker", "logs"],
    &["docker", "compose", "ps"],
    &["kubectl", "get"],
    &["kubectl", "logs"],
    &["kubectl", "describe"],
    &["gh", "pr", "list"],
    &["gh", "pr", "view"],
    &["make"],
    &["mvn"],
    &["gradle"],
    &["rg"],
    &["tree"],
    &["fd"],
    &["find"],
    &["ls"],
    &["cat"],
    &["head"],
    &["some-unknown-tool"],
];

const ERR: &str = "fatal: no such file or directory\n";

fn run(argv: &[&str], exit_code: i32) -> (String, &'static str) {
    let args: Vec<String> = argv[1..].iter().map(|s| s.to_string()).collect();
    let (f, id) = pick(argv[0], &args);
    let out = f(&FilterCtx {
        args: &args,
        stdout: "",
        stderr: ERR,
        exit_code,
    });
    (out.text, id)
}

#[test]
fn failing_command_keeps_its_stderr() {
    let lost: Vec<&str> = COMMANDS
        .iter()
        .filter_map(|argv| {
            let (text, id) = run(argv, 1);
            (!text.contains("fatal: no such file")).then_some(id)
        })
        .collect();
    assert!(
        lost.is_empty(),
        "filters dropped stderr on failure: {lost:?}"
    );
}

#[test]
fn successful_stdout_only_filters_still_drop_stderr_noise() {
    for argv in [&["rg"][..], &["ls"], &["git", "status"], &["fd"]] {
        let (text, id) = run(argv, 0);
        assert!(
            !text.contains("fatal"),
            "{id} leaked stderr on success: {text:?}"
        );
    }
}
