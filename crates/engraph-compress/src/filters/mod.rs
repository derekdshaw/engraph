//! Per-command output filters. Each filter knows the format of a specific
//! command's output and produces a structured summary.

pub mod bazel;
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

/// What a command's output is. Decides the generic passes `engraph run`
/// applies before the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// A test, build, lint, install, or log run. Lines are events: an
    /// identical repeated line is noise, as is one that repeats with only
    /// its timestamp changed.
    Event,
    /// A file read, a diff, search matches, a listing, a table. Every line
    /// is data; nothing is collapsed. The safe default: an unknown command
    /// is content.
    Content,
}

/// The filter `pick` chose: the function, its telemetry id, and what kind
/// of output it reads. The kind is declared beside the command it
/// classifies, so a new arm cannot be added without the decision.
#[derive(Debug, Clone, Copy)]
pub struct Filter {
    pub id: &'static str,
    pub kind: OutputKind,
    pub run: FilterFn,
}

impl Filter {
    const fn event(id: &'static str, run: FilterFn) -> Self {
        Self {
            id,
            kind: OutputKind::Event,
            run,
        }
    }
    const fn content(id: &'static str, run: FilterFn) -> Self {
        Self {
            id,
            kind: OutputKind::Content,
            run,
        }
    }
}

pub fn pick(cmd: &str, args: &[String]) -> Filter {
    let (cmd, args) = strip_launcher(command_basename(cmd), args);
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
        ("git", "log") => Filter::content("git_log", git::log),
        ("git", "diff") => Filter::content("git_diff", git::diff),
        ("git", "status") => Filter::content("git_status", git::status),
        ("git", "show") => Filter::content("git_show", git::show),
        ("git", "push") => Filter::content("git_push", git::push),
        ("git", "pull") => Filter::content("git_pull", git::pull),
        ("git", "fetch") => Filter::content("git_fetch", git::fetch),
        ("git", "commit") => Filter::content("git_commit", git::commit),
        // (`git add` is silent on success — wrapping it only adds latency.)

        // cargo / rust
        ("cargo", "test") => Filter::event("cargo_test", cargo::test),
        ("cargo", "build") => Filter::event("cargo_build", cargo::build),
        ("cargo", "check") => Filter::event("cargo_check", cargo::check),
        ("cargo", "clippy") => Filter::event("cargo_clippy", cargo::clippy),
        ("cargo", "doc") => Filter::event("cargo_doc", cargo::doc),
        ("cargo", "bench") => Filter::event("cargo_bench", cargo::bench),
        ("cargo", "audit") => Filter::event("cargo_audit", cargo::audit),
        ("cargo", "tree") => Filter::content("cargo_tree", cargo::tree_cmd),

        // npm
        ("npm", "install" | "i" | "ci") => Filter::event("npm_install", npm::install),
        // pnpm and yarn run package.json scripts with npm-shaped output;
        // `yarn t` is a user script, not a test alias, so it stays generic.
        ("npm" | "pnpm", "test" | "t") | ("yarn", "test") => Filter::event("npm_test", npm::test),

        // Python
        ("pytest", _) => Filter::event("pytest", python::pytest),
        ("pip", "install") => Filter::event("pip_install", python::pip_install),
        ("pip", "list") => Filter::content("pip_list", python::pip_list),
        ("uv", "install" | "sync" | "add") => Filter::event("uv", python::uv),

        // Lint family (shared shape)
        ("ruff", _) => Filter::event("ruff", lint::ruff),
        ("mypy", _) => Filter::event("mypy", lint::mypy),
        ("eslint", _) => Filter::event("eslint", lint::eslint),
        ("tsc", _) => Filter::event("tsc", lint::tsc),

        // Go
        ("go", "test") => Filter::event("go_test", go::test),
        ("go", "build") => Filter::event("go_build", go::build),
        ("go", "vet") => Filter::event("go_vet", go::vet),
        ("go", "mod") if second == "tidy" => Filter::event("go_mod_tidy", go::mod_tidy),
        ("go", "mod") if second == "download" => Filter::event("go_mod_download", go::mod_download),

        // Bazel (`bazelisk` is the common version-pinning wrapper)
        ("bazel" | "bazelisk", "build") => Filter::event("bazel_build", bazel::build),
        ("bazel" | "bazelisk", "test") => Filter::event("bazel_test", bazel::test),
        ("bazel" | "bazelisk", "query" | "cquery" | "aquery") => {
            Filter::content("bazel_query", bazel::query)
        }

        // JS/TS extras
        ("yarn", "install" | "add") => Filter::event("yarn_install", js::yarn_install),
        ("pnpm", "install" | "add" | "i") => Filter::event("pnpm_install", js::pnpm_install),
        ("jest" | "vitest" | "mocha", _) => Filter::event("js_test", js::js_test),

        // Containers
        ("docker", "ps" | "images") => Filter::content("docker_ps", docker::ps),
        ("docker", "logs") => Filter::event("docker_logs", docker::logs),
        ("docker", "compose") if matches!(second, "ps" | "logs") => {
            Filter::content("docker_compose", docker::compose)
        }
        ("kubectl", "get") => Filter::content("kubectl_get", kubectl::get),
        ("kubectl", "logs") => Filter::event("kubectl_logs", kubectl::logs),
        ("kubectl", "describe") => Filter::content("kubectl_describe", kubectl::describe),

        // gh
        ("gh", "pr" | "issue" | "repo") if matches!(second, "list") => {
            Filter::content("gh_list", gh::list)
        }
        ("gh", "pr" | "issue" | "repo") if matches!(second, "view") => {
            Filter::content("gh_view", gh::view)
        }

        // Build systems
        ("make", _) => Filter::event("make", make::make),
        ("mvn", _) => Filter::event("mvn", jvm::mvn),
        ("gradle" | "gradlew", _) => Filter::event("gradle", jvm::gradle),

        // Search
        ("rg" | "grep", _) => Filter::content("rg", search::rg),

        // Listings / trees
        ("tree", _) => Filter::content("tree", tree::tree),
        ("fd", _) => Filter::content("fd", tree::fd),
        ("find", _) => Filter::content("find", tree::find_cmd),
        ("ls", _) => Filter::content("ls", tree::ls),

        // File reads: whole-file (cat/bat/less) and user-windowed (head/tail).
        ("cat" | "bat" | "less", _) => Filter::content("read_cat", read::cat),
        ("head" | "tail", _) => Filter::content("read_head_tail", read::head_tail),

        _ => Filter::content("generic", generic::filter),
    }
}

/// `/usr/bin/git` and `./gradlew` classify as `git` and `gradlew`. The hook
/// keeps a path-like command word verbatim in the wrapped command (the user
/// named that binary, and a bare `gradlew` is not on PATH), so `engraph run`
/// sees the path and must route on its basename. A plain word is returned
/// as is.
fn command_basename(cmd: &str) -> &str {
    if !(cmd.starts_with('/') || cmd.starts_with("./")) {
        return cmd;
    }
    std::path::Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd)
}

/// Peel a launcher prefix so the launched command selects the filter:
/// `uv run pytest` classifies as `pytest`, `npx tsc` as `tsc`, `python -m
/// pytest` as `pytest`, `pnpm exec vitest` as `vitest`. Launcher flags
/// (`npx --yes`, `uv run --with`) are not peeled; the flag word classifies as
/// `generic`, the same as before launchers were recognized. Callers that pass
/// the unstripped args on to the filter are fine: no filter reachable through
/// a launcher reads `FilterCtx::args`.
fn strip_launcher<'a>(cmd: &'a str, args: &'a [String]) -> (&'a str, &'a [String]) {
    let first = args.first().map(String::as_str).unwrap_or("");
    let launched = match (cmd, first) {
        ("uv", "run") | ("python" | "python3", "-m") | ("pnpm", "exec") => 1,
        ("npx", _) => 0,
        _ => return (cmd, args),
    };
    match args.get(launched) {
        Some(c) if !c.starts_with('-') => (c.as_str(), &args[launched + 1..]),
        _ => (cmd, args),
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
        assert_eq!(pick("git", &argv(&["-C", "/repo", "log"])).id, "git_log");
        assert_eq!(
            pick("git", &argv(&["-c", "color.ui=always", "log"])).id,
            "git_log"
        );
        assert_eq!(pick("git", &argv(&["-C", "/repo", "show"])).id, "git_show");
        assert_eq!(
            pick("git", &argv(&["--git-dir=/r/.git", "diff"])).id,
            "git_diff"
        );
        assert_eq!(
            pick("git", &argv(&["-C", "/repo", "status"])).id,
            "git_status"
        );
    }

    #[test]
    fn git_without_global_opts_still_classifies() {
        assert_eq!(pick("git", &argv(&["log", "--oneline"])).id, "git_log");
    }

    #[test]
    fn gradlew_classifies_to_gradle() {
        assert_eq!(pick("gradlew", &argv(&["test"])).id, "gradle");
        assert_eq!(pick("./gradlew", &argv(&["test"])).id, "gradle");
    }

    #[test]
    fn launcher_prefix_classifies_to_launched_command() {
        assert_eq!(pick("uv", &argv(&["run", "pytest"])).id, "pytest");
        assert_eq!(pick("python", &argv(&["-m", "pytest"])).id, "pytest");
        assert_eq!(pick("python3", &argv(&["-m", "pytest"])).id, "pytest");
        assert_eq!(pick("npx", &argv(&["tsc", "--noEmit"])).id, "tsc");
        assert_eq!(pick("npx", &argv(&["vitest", "run"])).id, "js_test");
        assert_eq!(
            pick("pnpm", &argv(&["exec", "vitest", "run"])).id,
            "js_test"
        );
    }

    #[test]
    fn bare_or_flagged_launcher_stays_generic() {
        assert_eq!(pick("npx", &argv(&[])).id, "generic");
        assert_eq!(pick("uv", &argv(&["run"])).id, "generic");
        assert_eq!(pick("python", &argv(&["-m"])).id, "generic");
        assert_eq!(pick("npx", &argv(&["--yes", "vitest"])).id, "generic");
    }

    #[test]
    fn pnpm_and_yarn_test_use_npm_test_filter() {
        assert_eq!(pick("pnpm", &argv(&["test"])).id, "npm_test");
        assert_eq!(pick("pnpm", &argv(&["t"])).id, "npm_test");
        assert_eq!(pick("yarn", &argv(&["test"])).id, "npm_test");
    }

    #[test]
    fn path_like_command_word_classifies_by_basename() {
        assert_eq!(pick("/usr/bin/git", &argv(&["log"])).id, "git_log");
        assert_eq!(
            pick("./node_modules/.bin/tsc", &argv(&["--noEmit"])).id,
            "tsc"
        );
    }

    #[test]
    fn pick_declares_output_kinds() {
        assert_eq!(pick("pytest", &argv(&[])).kind, OutputKind::Event);
        assert_eq!(pick("gradle", &argv(&["test"])).kind, OutputKind::Event);
        assert_eq!(pick("cat", &argv(&["x"])).kind, OutputKind::Content);
        assert_eq!(pick("git", &argv(&["diff"])).kind, OutputKind::Content);
        assert_eq!(pick("rg", &argv(&["foo"])).kind, OutputKind::Content);
        // An unknown command is content: it can never be collapsed.
        assert_eq!(
            pick("totally-made-up", &argv(&[])).kind,
            OutputKind::Content
        );
    }

    #[test]
    fn git_trailing_value_flag_does_not_panic() {
        // Malformed `git -C` (no value, no subcommand) must classify, not panic.
        assert_eq!(pick("git", &argv(&["-C"])).id, "generic");
    }
}
