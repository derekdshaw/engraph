//! Per-command output filters. Each filter knows the format of a specific
//! command's output and produces a structured summary.

pub mod build_system;
pub mod cargo;
pub mod docker;
pub mod generic;
pub mod gh;
pub mod git;
pub mod go;
pub mod js;
pub mod kubectl;
pub mod lint;
pub mod npm;
pub mod python;
pub mod search;
pub mod tree;
pub mod util;

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
        ("make", _) => (build_system::make, "make"),
        ("mvn", _) => (build_system::mvn, "mvn"),
        ("gradle", _) | ("./gradlew", _) => (build_system::gradle, "gradle"),

        // Search
        ("rg" | "grep", _) => (search::rg, "rg"),

        // Listings / trees
        ("tree", _) => (tree::tree, "tree"),
        ("fd", _) => (tree::fd, "fd"),
        ("ls", _) => (tree::ls, "ls"),

        _ => (generic::filter, "generic"),
    }
}
