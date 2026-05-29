//! Per-command output filters. Each filter knows the format of a specific
//! command's output and produces a structured summary.

pub mod cargo;
pub mod docker;
pub mod generic;
pub mod gh;
pub mod git;
pub mod go;
pub mod js;
pub mod jvm;
pub mod kubectl;
pub mod lint;
pub mod make;
pub mod npm;
pub mod python;
pub mod read;
pub mod search;
pub mod tree;
pub mod util;

/// Input to a filter. `cmd` is omitted intentionally — the dispatcher
/// (`pick`) routes on `cmd`, so filters that receive a `FilterCtx` already
/// know which command they were chosen for.
#[derive(Debug, Clone)]
pub struct FilterCtx<'a> {
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
    // `git`'s subcommand can be preceded by global options (`-C <path>`,
    // `-c <k=v>`, `--git-dir[=...]`, `--work-tree[=...]`). Skip them so we
    // classify on the subcommand, not the global flag — otherwise
    // `git -C /repo log` would key off `-C` and fall through to `generic`.
    let args = if cmd == "git" {
        &args[git_global_opt_len(args)..]
    } else {
        args
    };
    let first = args.first().map(|s| s.as_str()).unwrap_or("");
    let second = args.get(1).map(|s| s.as_str()).unwrap_or("");
    match (cmd, first) {
        // git
        ("git", "log") => (git::log, "git_log"),
        ("git", "diff") => (git::diff, "git_diff"),
        ("git", "status") => (git::status, "git_status"),
        ("git", "show") => (git::show, "git_show"),

        // cargo / rust
        ("cargo", "test") => (cargo::test, "cargo_test"),
        ("cargo", "build") => (cargo::build, "cargo_build"),
        ("cargo", "check") => (cargo::check, "cargo_check"),
        ("cargo", "clippy") => (cargo::clippy, "cargo_clippy"),
        ("cargo", "doc") => (cargo::doc, "cargo_doc"),
        ("cargo", "bench") => (cargo::bench, "cargo_bench"),
        ("cargo", "audit") => (cargo::audit, "cargo_audit"),
        ("cargo", "tree") => (cargo::tree_cmd, "cargo_tree"),

        // npm
        ("npm", "install" | "i" | "ci") => (npm::install, "npm_install"),
        ("npm", "test" | "t") => (npm::test, "npm_test"),

        // Python
        ("pytest", _) => (python::pytest, "pytest"),
        ("pip", "install") => (python::pip_install, "pip_install"),
        ("pip", "list") => (python::pip_list, "pip_list"),
        ("uv", "install" | "sync" | "add") => (python::uv, "uv"),

        // Lint family (shared shape)
        ("ruff", _) => (lint::ruff, "ruff"),
        ("mypy", _) => (lint::mypy, "mypy"),
        ("eslint", _) => (lint::eslint, "eslint"),
        ("tsc", _) => (lint::tsc, "tsc"),

        // Go
        ("go", "test") => (go::test, "go_test"),
        ("go", "build") => (go::build, "go_build"),
        ("go", "vet") => (go::vet, "go_vet"),
        ("go", "mod") if second == "tidy" => (go::mod_tidy, "go_mod_tidy"),

        // JS/TS extras
        ("yarn", "install" | "add") => (js::yarn_install, "yarn_install"),
        ("pnpm", "install" | "add" | "i") => (js::pnpm_install, "pnpm_install"),
        ("jest" | "vitest" | "mocha", _) => (js::js_test, "js_test"),

        // Containers
        ("docker", "ps" | "images") => (docker::ps, "docker_ps"),
        ("docker", "logs") => (docker::logs, "docker_logs"),
        ("docker", "compose") if matches!(second, "ps" | "logs") => {
            (docker::compose, "docker_compose")
        }
        ("kubectl", "get") => (kubectl::get, "kubectl_get"),
        ("kubectl", "logs") => (kubectl::logs, "kubectl_logs"),
        ("kubectl", "describe") => (kubectl::describe, "kubectl_describe"),

        // gh
        ("gh", "pr" | "issue" | "repo") if matches!(second, "list") => (gh::list, "gh_list"),
        ("gh", "pr" | "issue" | "repo") if matches!(second, "view") => (gh::view, "gh_view"),

        // Build systems
        ("make", _) => (make::make, "make"),
        ("mvn", _) => (jvm::mvn, "mvn"),
        ("gradle", _) | ("./gradlew", _) => (jvm::gradle, "gradle"),

        // Search
        ("rg" | "grep", _) => (search::rg, "rg"),

        // Listings / trees
        ("tree", _) => (tree::tree, "tree"),
        ("fd", _) => (tree::fd, "fd"),
        ("ls", _) => (tree::ls, "ls"),

        // File reads: whole-file (cat/bat/less) and user-windowed (head/tail).
        ("cat" | "bat" | "less", _) => (read::cat, "read_cat"),
        ("head" | "tail", _) => (read::head_tail, "read_head_tail"),

        _ => (generic::filter, "generic"),
    }
}

/// Number of leading global-option tokens in `args` (the tokens *after* the
/// `git` word), i.e. the index of the git subcommand. For `git -C <path> log`
/// the caller passes `["-C", "<path>", "log", ...]` and this returns 2.
///
/// Handles the value-taking flags `-C`/`-c` (flag + separate value) and the
/// inline `--git-dir=...`/`--work-tree=...` forms. Stops at the first token
/// that isn't one of those — the subcommand, or an unrecognized flag we leave
/// for the classifier to reject. Result is clamped to `args.len()`, so a
/// trailing `-C` with no value can't push the index out of bounds.
pub fn git_global_opt_len(args: &[String]) -> usize {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-C" | "-c" => i += 2,
            s if s.starts_with("--git-dir=") || s.starts_with("--work-tree=") => i += 1,
            _ => break,
        }
    }
    i.min(args.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn git_global_opts_classify_to_subcommand() {
        assert_eq!(pick("git", &argv(&["-C", "/repo", "log"])).1, "git_log");
        assert_eq!(
            pick("git", &argv(&["-c", "color.ui=always", "log"])).1,
            "git_log"
        );
        assert_eq!(pick("git", &argv(&["-C", "/repo", "show"])).1, "git_show");
        assert_eq!(
            pick("git", &argv(&["--git-dir=/r/.git", "diff"])).1,
            "git_diff"
        );
        assert_eq!(
            pick("git", &argv(&["-C", "/repo", "status"])).1,
            "git_status"
        );
    }

    #[test]
    fn git_without_global_opts_still_classifies() {
        assert_eq!(pick("git", &argv(&["log", "--oneline"])).1, "git_log");
    }

    #[test]
    fn git_trailing_value_flag_does_not_panic() {
        // Malformed `git -C` (no value, no subcommand) must classify, not panic.
        assert_eq!(pick("git", &argv(&["-C"])).1, "generic");
    }
}
