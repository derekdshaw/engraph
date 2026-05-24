//! Per-command output filters. Each filter knows the format of a specific
//! command's output and produces a structured summary.

pub mod cargo;
pub mod generic;
pub mod git;
pub mod npm;
pub mod tree;

#[derive(Debug, Clone)]
pub struct FilterCtx<'a> {
    pub cmd: &'a str,
    pub args: &'a [String],
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub exit_code: i32,
}

#[derive(Debug, Clone)]
pub struct FilterOutput {
    pub text: String,
    pub filter_id: &'static str,
}

pub type FilterFn = fn(&FilterCtx) -> FilterOutput;

pub fn pick(cmd: &str, args: &[String]) -> (FilterFn, &'static str) {
    let first = args.first().map(|s| s.as_str()).unwrap_or("");
    match (cmd, first) {
        ("git", "log") => (git::log, "git_log"),
        ("git", "diff") => (git::diff, "git_diff"),
        ("git", "status") => (git::status, "git_status"),
        ("git", "show") => (git::show, "git_show"),
        ("cargo", "test") => (cargo::test, "cargo_test"),
        ("cargo", "build") => (cargo::build, "cargo_build"),
        ("cargo", "check") => (cargo::build, "cargo_check"),
        ("cargo", "clippy") => (cargo::clippy, "cargo_clippy"),
        ("npm", "install" | "i" | "ci") => (npm::install, "npm_install"),
        ("npm", "test" | "t") => (npm::test, "npm_test"),
        ("tree", _) => (tree::tree, "tree"),
        ("fd", _) => (tree::fd, "fd"),
        ("ls", _) => (tree::ls, "ls"),
        _ => (generic::filter, "generic"),
    }
}
