use std::path::Path;
use std::process::Command;

/// One adapter per (build system, language). Engraph shells out to the external
/// SCIP indexer; the driver is a thin file-probe + argv builder.
///
/// `out` is the absolute path the indexer must write its SCIP file to. The
/// caller (`index::run_driver_to_scip`) picks it — deliberately OUTSIDE the
/// repo, so indexing never pollutes the working tree — and reads it back. Every
/// driver pins `--output` (or the indexer's equivalent) to `out`.
pub trait Driver: Send + Sync {
    fn name(&self) -> &'static str;
    fn detect(&self, repo: &Path) -> bool;
    /// Every file extension the indexer can read, in any configuration (a
    /// superset: scip-typescript reads `.js` only with `allowJs`). An
    /// untracked file with one of them starts an `--if-changed` refresh.
    fn extensions(&self) -> &'static [&'static str];
    fn command(&self, repo: &Path, out: &Path) -> Command;
    /// Give the run's `--go-tags` to `cmd`. Only scip-go takes them.
    fn set_go_tags(&self, _cmd: &mut Command, _tags: Option<&str>) {}
}

pub fn registry() -> Vec<Box<dyn Driver>> {
    vec![
        Box::new(RustAnalyzer),
        Box::new(ScipPython),
        Box::new(ScipGo),
        Box::new(ScipTypescript),
        Box::new(ScipJava),
    ]
}

pub fn by_name(name: &str) -> Option<Box<dyn Driver>> {
    registry().into_iter().find(|d| d.name() == name)
}

pub struct RustAnalyzer;
impl Driver for RustAnalyzer {
    fn name(&self) -> &'static str {
        "rust-analyzer"
    }
    fn detect(&self, repo: &Path) -> bool {
        repo.join("Cargo.toml").is_file()
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }
    fn command(&self, repo: &Path, out: &Path) -> Command {
        // rust-analyzer's `scip` defaults `--output` to `index.scip` in the
        // process's CWD; pin it explicitly to `out`. Still chdir into the repo
        // so any auxiliary files it writes land there, not in engraph's cwd.
        let mut c = Command::new("rust-analyzer");
        c.arg("scip")
            .arg(repo)
            .arg("--output")
            .arg(out)
            .current_dir(repo);
        c
    }
}

/// The version every pinned symbol id carries: the scip-python and scip-go
/// drivers give it to the indexer, and `scip_loader::normalize_moniker` gives
/// it to rust-analyzer, scip-typescript, and semanticdb ids (plan 05, part 2,
/// rule R3). One value, so a pinned id looks the same whatever its scheme.
pub(crate) const PINNED_VERSION: &str = "0.0.0";

/// Fixed `--project-version` passed to scip-python (standalone driver and the
/// Bazel symbol pass). Intentionally overrides scip-python's resolved version
/// (git revision / pyproject) — see `ScipPython::command` for the full
/// rationale (crash avoidance + stable entity IDs).
pub(crate) const SCIP_PYTHON_VERSION: &str = PINNED_VERSION;

/// The `--project-name` for scip-python (plan 05, part 2, rule R1): the
/// `[project].name` of `pyproject.toml`, so the package name in every
/// first-party id comes from the repo, not from the name the checkout
/// directory happens to have (a worktree named `elm` gave other ids than a
/// clone named `engraph`). `[tool.poetry].name` answers for a Poetry project
/// without a `[project]` table. Without a `pyproject.toml`, or without a name
/// in either table, the basename of the **main worktree** of the repo: every
/// worktree of a repo answers the same name, so the base graph and the overlay
/// of a worktree write one id for a symbol (plan 10, part 1, step 4). A
/// project below the root of the repo already had a name that does not change
/// (`services/api` is `api` in each checkout); this rule is for a project at
/// the root, where the basename is the name the user gave the checkout. The
/// directory's basename outside a repo, as before.
///
/// This is an id rule: a change to what it returns changes symbol ids and
/// bumps `scip_loader::NORMALIZATION_VERSION`.
pub(crate) fn python_project_name(repo: &Path) -> String {
    std::fs::read_to_string(repo.join("pyproject.toml"))
        .ok()
        .and_then(|text| pyproject_name(&text))
        .or_else(|| main_worktree_name(repo))
        .unwrap_or_else(|| {
            repo.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string())
        })
}

/// The basename of the main worktree of the repo, when `repo` is the root of
/// its own checkout: the worktree `elm/` and the main worktree `engraph/` then
/// both answer `engraph`. `None` outside git, and for a project below the root
/// of its checkout — `services/api` is `api` in every checkout already, so the
/// caller's own basename is right for it.
fn main_worktree_name(repo: &Path) -> Option<String> {
    let top = engraph_core::utils::current_toplevel(repo)?
        .canonicalize()
        .ok()?;
    if repo.canonicalize().ok()? != top {
        return None;
    }
    let main = engraph_core::utils::main_worktree(repo)?;
    Path::new(&main)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

/// The project name in `pyproject.toml` text: `[project].name`, else
/// `[tool.poetry].name` (a Poetry project without a `[project]` table).
/// `None` when neither table has a string name, or the text is not TOML.
fn pyproject_name(text: &str) -> Option<String> {
    let table: toml::Table = text.parse().ok()?;
    let project = table.get("project").and_then(toml::Value::as_table);
    let poetry = table
        .get("tool")
        .and_then(toml::Value::as_table)
        .and_then(|tool| tool.get("poetry"))
        .and_then(toml::Value::as_table);
    [project, poetry].into_iter().flatten().find_map(|t| {
        t.get("name")
            .and_then(toml::Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })
}

/// The heap ceiling, in MB, of a Node indexer (scip-python, scip-typescript)
/// when `ENGRAPH_NODE_HEAP_MB` is not set. Node's own default, about 4 GB,
/// runs out on ray (5,100 Python files). Measured on ray (2026-11-18,
/// scip-python 0.6.8): an 8 GB ceiling completes but is GC-bound — 7m54s with
/// RSS pinned at 8.3 GiB, against 5m21s and a 9.85 GiB peak under 16 GB, with
/// identical SCIP. 12 GB leaves about 20% above what ray uses. It is a
/// ceiling, not a reservation: Node only grows to what a repo needs.
const DEFAULT_NODE_HEAP_MB: u32 = 12288;

/// `NODE_OPTIONS` for a Node indexer: `existing` (the user's `NODE_OPTIONS`)
/// with `--max-old-space-size` added. A heap the user set in `existing` wins
/// over the default; `override_mb` (`ENGRAPH_NODE_HEAP_MB`) wins over both,
/// because Node honors the last of repeated flags. An `override_mb` that is
/// not a positive number of MB is ignored, as if unset.
fn node_options(existing: Option<&str>, override_mb: Option<&str>) -> String {
    let existing = existing.map(str::trim).filter(|s| !s.is_empty());
    let mb = match override_mb {
        Some(v) => match v.trim().parse::<u32>() {
            Ok(mb) if mb > 0 => Some(mb),
            _ => {
                tracing::warn!(
                    value = v,
                    "ENGRAPH_NODE_HEAP_MB is not a positive number of MB; ignoring it"
                );
                None
            }
        },
        None => None,
    };
    let mb = match (mb, existing) {
        (Some(mb), _) => mb,
        // Node reads `_` and `-` alike in a V8 flag name.
        (None, Some(e)) if e.replace('_', "-").contains("--max-old-space-size") => {
            return e.to_string();
        }
        (None, _) => DEFAULT_NODE_HEAP_MB,
    };
    match existing {
        Some(e) => format!("{e} --max-old-space-size={mb}"),
        None => format!("--max-old-space-size={mb}"),
    }
}

/// Give a Node indexer its heap ceiling (`node_options`). The scip-python
/// and scip-typescript commands call it, so the standalone drivers and the
/// Bazel symbol pass, which runs those commands, all get it.
fn set_node_heap(cmd: &mut Command) {
    let existing = std::env::var("NODE_OPTIONS").ok();
    let override_mb = std::env::var("ENGRAPH_NODE_HEAP_MB").ok();
    cmd.env(
        "NODE_OPTIONS",
        node_options(existing.as_deref(), override_mb.as_deref()),
    );
}

pub struct ScipPython;
impl Driver for ScipPython {
    fn name(&self) -> &'static str {
        "scip-python"
    }
    fn detect(&self, repo: &Path) -> bool {
        repo.join("pyproject.toml").is_file() || repo.join("setup.py").is_file()
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }
    fn command(&self, repo: &Path, out: &Path) -> Command {
        let project_name = python_project_name(repo);
        // scip-python's --cwd defaults to the spawning process's CWD, and
        // --output is resolved relative to --cwd. Pin --cwd to the repo (so it
        // indexes the right tree) and --output to `out` (an absolute path
        // outside the repo), regardless of where engraph was invoked.
        let mut c = Command::new("scip-python");
        c.arg("index")
            .arg("--cwd")
            .arg(repo)
            .arg("--output")
            .arg(out)
            .arg("--project-name")
            .arg(project_name)
            // Pin a fixed --project-version, deliberately OVERRIDING scip-python's
            // resolved version (the git revision, or `[project].version` from
            // pyproject.toml). Two reasons to override:
            //   1. Crash avoidance: with no flag and no git repo, the version is
            //      undefined and scip-python 0.6.6 crashes in normalizeNameOrVersion
            //      (ScipSymbol.ts) — it then writes an empty, metadata-only index.
            //   2. Stable entity IDs: SCIP monikers embed the version and engraph
            //      uses monikers as entity IDs (scip_loader.rs). A git-revision
            //      version would change every ID on each commit, churning the whole
            //      codegraph on re-index; a fixed version keeps IDs stable.
            // Discarding the "real" version is an accepted tradeoff: engraph treats
            // monikers as opaque keys, so version fidelity buys nothing here.
            .arg("--project-version")
            .arg(SCIP_PYTHON_VERSION);
        set_node_heap(&mut c);
        c
    }
}

/// Fixed `--module-version` passed to scip-go by the `ScipGo` driver and the
/// Bazel multi-module symbol pass (`bazel_symbols::run_go_modules`). scip-go
/// defaults this to the git short hash of the cwd repo, which embeds into
/// every moniker — so without a pin, Go entity IDs churn on each commit. Same
/// rationale as `SCIP_PYTHON_VERSION`. The loader gives the same version to
/// the ids scip-go writes with the version `.`
/// (`scip_loader::normalize_moniker`).
pub(crate) const SCIP_GO_VERSION: &str = PINNED_VERSION;

pub struct ScipGo;
impl Driver for ScipGo {
    fn name(&self) -> &'static str {
        "scip-go"
    }
    fn detect(&self, repo: &Path) -> bool {
        repo.join("go.mod").is_file() || repo.join("go.work").is_file()
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["go"]
    }
    fn command(&self, repo: &Path, out: &Path) -> Command {
        let mut c = scip_go_command(repo, repo, out);
        // scip-go's default pattern `./...` reaches only the module of the
        // cwd. In a go.work, one `<dir>/...` pattern per `use` directory
        // indexes every module in one run, each under its own module path.
        if let Some(uses) = go_work_uses(repo) {
            c.args(uses.iter().map(|u| go_work_pattern(u)));
        }
        c
    }
    fn set_go_tags(&self, cmd: &mut Command, tags: Option<&str>) {
        apply_go_tags(cmd, tags);
    }
}

/// scip-go, run in `cwd`, indexing the module at `module_root` (relative to
/// `cwd`, or absolute) into `out`, with the pinned module version and
/// `GOWORK=off` when the module's go.work does not list it. scip-go defaults
/// `--output` to `index.scip` in the cwd; pinning it to `out` keeps the repo
/// clean. Used by the `ScipGo` driver and the Bazel Go pass.
pub(crate) fn scip_go_command(cwd: &Path, module_root: &Path, out: &Path) -> Command {
    let mut c = Command::new("scip-go");
    c.arg("index")
        .arg("--module-root")
        .arg(module_root)
        .arg("--module-version")
        .arg(SCIP_GO_VERSION)
        .arg("--output")
        .arg(out)
        .current_dir(cwd);
    let dir = if module_root == Path::new(".") {
        cwd.to_path_buf()
    } else {
        cwd.join(module_root)
    };
    if outside_its_go_work(&dir) {
        c.env("GOWORK", "off");
    }
    c
}

/// `GOFLAGS` for a scip-go run with the build `tags` (comma-separated): the
/// user's `existing` flags, then `-tags=<tags>`. Go takes the last `-tags`,
/// so these win over a `-tags` the user set, and the other flags stay.
pub(crate) fn go_flags_with_tags(existing: Option<&str>, tags: &str) -> String {
    match existing.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => format!("{e} -tags={tags}"),
        None => format!("-tags={tags}"),
    }
}

/// Give the build `tags` to the scip-go `cmd`. scip-go has no tags flag, but
/// its package loader reads `GOFLAGS` (verified 2026-11-16, scip-go 0.2.7).
/// The existing flags come from `go env`, not the process environment: a
/// `GOFLAGS` variable replaces the one `go env -w` saved, so reading only the
/// variable would drop the saved flags.
pub(crate) fn apply_go_tags(cmd: &mut Command, tags: Option<&str>) {
    if let Some(tags) = tags {
        let existing = Command::new("go")
            .args(["env", "GOFLAGS"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        cmd.env("GOFLAGS", go_flags_with_tags(existing.as_deref(), tags));
    }
}

/// Whether the nearest `go.work` above `dir` does not list it. Go then
/// builds nothing in `dir`, and scip-go exits 0 with an empty index, so the
/// run needs `GOWORK=off`. A `go.work` in `dir` itself is the one its run
/// uses (`ScipGo::command`).
pub(crate) fn outside_its_go_work(dir: &Path) -> bool {
    if dir.join("go.work").is_file() {
        return false;
    }
    dir.ancestors()
        .skip(1)
        .find(|work_dir| work_dir.join("go.work").is_file())
        .is_some_and(|work_dir| !go_work_lists(work_dir, dir))
}

/// Whether the `go.work` in `work_dir` lists `dir` as a `use` directory, by
/// a path relative to `work_dir` or an absolute one. `dir` may be relative
/// (discovery from `.`), so an absolute `use` is compared canonicalized.
pub(crate) fn go_work_lists(work_dir: &Path, dir: &Path) -> bool {
    let rel = crate::index::workspace_rel(work_dir, dir);
    go_work_uses(work_dir).is_some_and(|uses| {
        uses.iter().any(|u| {
            let u = go_work_rel(u);
            u == rel
                || (Path::new(u).is_absolute()
                    && matches!(
                        (Path::new(u).canonicalize(), dir.canonicalize()),
                        (Ok(a), Ok(b)) if a == b
                    ))
        })
    })
}

/// The `use` directories of the `go.work` in `dir`, as written: relative to
/// `dir`, or absolute. `None` without a `go.work`.
pub(crate) fn go_work_uses(dir: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(dir.join("go.work")).ok()?;
    Some(parse_go_work_uses(&text))
}

/// The paths of the `use` directives of a `go.work`: `use ./a`, or a
/// `use ( ... )` block with one path per line. A path may be quoted, and
/// `//` starts a comment.
fn parse_go_work_uses(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let path = if in_block {
            if line == ")" {
                in_block = false;
                continue;
            }
            line
        } else {
            let Some(rest) = line
                .strip_prefix("use")
                .filter(|r| r.starts_with([' ', '\t', '(']))
            else {
                continue;
            };
            let rest = rest.trim();
            if rest == "(" {
                in_block = true;
                continue;
            }
            rest
        };
        out.push(path.trim_matches('"').to_string());
    }
    out
}

/// A `use` path of a `go.work` as `workspace_rel` gives it: `.` for the root,
/// else `/`-separated without a leading `./` or a trailing `/`. An absolute
/// path stays as it is.
fn go_work_rel(path: &str) -> &str {
    if Path::new(path).is_absolute() {
        return path;
    }
    let rel = path.trim_start_matches("./").trim_end_matches('/');
    if rel.is_empty() { "." } else { rel }
}

/// The scip-go package pattern for every package of a `use` directory.
fn go_work_pattern(path: &str) -> String {
    match go_work_rel(path) {
        "." => "./...".to_string(),
        abs if Path::new(abs).is_absolute() => format!("{abs}/..."),
        rel => format!("./{rel}/..."),
    }
}

pub struct ScipTypescript;
impl Driver for ScipTypescript {
    fn name(&self) -> &'static str {
        "scip-typescript"
    }
    fn detect(&self, repo: &Path) -> bool {
        repo.join("package.json").is_file() && repo.join("tsconfig.json").is_file()
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"]
    }
    fn command(&self, repo: &Path, out: &Path) -> Command {
        typescript_command(repo, out, &crate::which::on_path)
    }
}

pub struct ScipJava;
impl Driver for ScipJava {
    fn name(&self) -> &'static str {
        "scip-java"
    }
    fn detect(&self, repo: &Path) -> bool {
        // scip-java's `index` command auto-detects {Maven, Gradle, sbt, mill}.
        // It does NOT drive Bazel — the --bazel-* flags in its help are for a
        // manual aspect workflow, not autodetection. Java-on-Bazel coverage
        // is Phase 2.3 territory (separate scip-bazel tool).
        repo.join("pom.xml").is_file()
            || repo.join("build.gradle").is_file()
            || repo.join("build.gradle.kts").is_file()
            || repo.join("build.sbt").is_file()
            || repo.join("build.sc").is_file() // mill
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["java", "scala", "kt"]
    }
    fn command(&self, repo: &Path, out: &Path) -> Command {
        // scip-java auto-detects Maven vs Gradle; let it. We just chdir into
        // the repo and pin --output to `out` (outside the repo).
        let mut c = Command::new("scip-java");
        c.arg("index").arg("--output").arg(out).current_dir(repo);
        c
    }
}

/// The scip-typescript command for `repo`. `on_path` says whether a package
/// manager is installed.
pub(crate) fn typescript_command(
    repo: &Path,
    out: &Path,
    on_path: &dyn Fn(&str) -> bool,
) -> Command {
    // scip-typescript defaults --output to `index.scip` in the cwd; pin it
    // to `out`. Still chdir into the repo so it picks up tsconfig.json.
    let mut c = Command::new("scip-typescript");
    c.arg("index").arg("--output").arg(out).current_dir(repo);
    set_node_heap(&mut c);
    let Some(ws) = TsWorkspace::of(repo) else {
        return c;
    };
    if !on_path(ws.tool()) {
        // The flag makes scip-typescript run the tool to list the packages,
        // and fail the whole run without it. Index the root alone, as before.
        tracing::warn!(
            repo = %repo.display(),
            "{} is not installed: indexing without {}, so the workspace packages are not indexed",
            ws.tool(),
            ws.flag()
        );
        return c;
    }
    // With the flag, scip-typescript indexes only the packages the tool
    // lists, and yarn 1 does not list the root: name the root too. It skips
    // a project already indexed only when the path is the same string, and
    // pnpm and yarn print the real path, so pass the real path.
    let root = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    c.arg(ws.flag()).arg(root);
    c
}

/// A JavaScript workspace whose packages scip-typescript can index from the
/// workspace's directory, each with its package globs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TsWorkspace {
    /// `pnpm-workspace.yaml`: `--pnpm-workspaces` (runs `pnpm ls -r`).
    Pnpm(Vec<String>),
    /// `package.json` `workspaces` and a `yarn.lock`: `--yarn-workspaces`
    /// (runs `yarn workspaces list`, or `info` for yarn 1).
    Yarn(Vec<String>),
}

impl TsWorkspace {
    /// The workspace in `dir`, if any. npm workspaces are not one: no
    /// scip-typescript flag lists their packages.
    pub(crate) fn of(dir: &Path) -> Option<Self> {
        if let Ok(yaml) = std::fs::read_to_string(dir.join("pnpm-workspace.yaml")) {
            // No globs read (a form `pnpm_packages` doesn't parse): with the
            // flag, the root's run would index packages that discovery also
            // records as projects of their own.
            let globs = pnpm_packages(&yaml);
            return (!globs.is_empty()).then_some(Self::Pnpm(globs));
        }
        if !dir.join("yarn.lock").is_file() {
            return None;
        }
        // `yarn workspaces` fails outside a workspace.
        let globs = package_json_workspaces(&dir.join("package.json"));
        (!globs.is_empty()).then_some(Self::Yarn(globs))
    }

    /// The package globs, relative to the workspace's directory.
    pub(crate) fn globs(&self) -> &[String] {
        match self {
            Self::Pnpm(g) | Self::Yarn(g) => g,
        }
    }

    pub(crate) fn tool(&self) -> &'static str {
        match self {
            Self::Pnpm(_) => "pnpm",
            Self::Yarn(_) => "yarn",
        }
    }

    fn flag(&self) -> &'static str {
        match self {
            Self::Pnpm(_) => "--pnpm-workspaces",
            Self::Yarn(_) => "--yarn-workspaces",
        }
    }
}

/// The `packages` list of a `pnpm-workspace.yaml`. Reads only the common
/// block form (`packages:` and then `- <glob>` items, quoted or not), not a
/// YAML parse.
fn pnpm_packages(yaml: &str) -> Vec<String> {
    let mut in_packages = false;
    let mut out = Vec::new();
    for line in yaml.lines() {
        let body = line.trim();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        // A line at column 0 that is not a list item starts a top-level key.
        if !line.starts_with([' ', '\t', '-']) {
            in_packages = body
                .split('#')
                .next()
                .is_some_and(|k| k.trim() == "packages:");
            continue;
        }
        if let Some(item) = body.strip_prefix('-').filter(|_| in_packages) {
            let item = item.trim();
            let glob = match item.chars().next() {
                Some(q @ ('\'' | '"')) => item[1..].split(q).next().unwrap_or_default(),
                _ => item.split(" #").next().unwrap_or_default().trim(),
            };
            if !glob.is_empty() {
                out.push(glob.to_string());
            }
        }
    }
    out
}

/// The `workspaces` globs of a `package.json`: an array, or yarn's
/// `{"packages": [...]}` object.
fn package_json_workspaces(path: &Path) -> Vec<String> {
    let Some(json) = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    else {
        return Vec::new();
    };
    let list = match &json["workspaces"] {
        serde_json::Value::Object(o) => o.get("packages"),
        w => Some(w),
    };
    list.and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|g| g.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_options_add_the_heap_and_keep_the_rest() {
        let default = format!("--max-old-space-size={DEFAULT_NODE_HEAP_MB}");
        assert_eq!(node_options(None, None), default);
        // The user's other Node flags stay.
        assert_eq!(
            node_options(Some("--require ./x.js"), None),
            format!("--require ./x.js {default}")
        );
        // A heap the user already set in NODE_OPTIONS wins over the default...
        assert_eq!(
            node_options(Some("--max-old-space-size=2048"), None),
            "--max-old-space-size=2048"
        );
        // Node also reads the flag with underscores.
        assert_eq!(
            node_options(Some("--max_old_space_size=2048"), None),
            "--max_old_space_size=2048"
        );
        // ...and ENGRAPH_NODE_HEAP_MB over both: Node honors the last flag.
        assert_eq!(
            node_options(Some("--max-old-space-size=2048"), Some("12288")),
            "--max-old-space-size=2048 --max-old-space-size=12288"
        );
        // Not a positive number of MB: ignored, so the default...
        assert_eq!(node_options(None, Some("lots")), default);
        assert_eq!(node_options(None, Some("0")), default);
        // ...or the heap the user set.
        assert_eq!(
            node_options(Some("--max-old-space-size=2048"), Some("lots")),
            "--max-old-space-size=2048"
        );
    }

    #[test]
    fn standalone_node_indexers_carry_a_heap_ceiling() {
        // scip-python and scip-typescript are Node programs, and Node's own
        // heap (about 4 GB) runs out on a large repo (ray) for a plain
        // `engraph index` too. The values are `node_options`'; this checks the
        // standalone commands get them. No env var is set here: a value is
        // always there (the default when nothing overrides it). The lock keeps
        // a test that sets these variables from running meanwhile.
        let _env = crate::test_env::lock();
        let repo = Path::new("/tmp/some-repo");
        let out = Path::new("/tmp/out/index.scip");
        for cmd in [
            ScipPython.command(repo, out),
            ScipTypescript.command(repo, out),
        ] {
            let node_options = cmd
                .get_envs()
                .find(|(k, _)| *k == "NODE_OPTIONS")
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned());
            assert!(
                node_options.is_some_and(|v| v.contains("--max-old-space-size=")),
                "{:?} has no heap ceiling",
                cmd.get_program()
            );
        }
    }

    #[test]
    fn python_project_name_comes_from_pyproject_else_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("elm");
        std::fs::create_dir_all(&repo).unwrap();
        // No pyproject: the directory basename, as before R1.
        assert_eq!(python_project_name(&repo), "elm");
        // The [project] name wins over the directory, whatever the quotes,
        // and a `name` in another table or a `names` key does not count.
        std::fs::write(
            repo.join("pyproject.toml"),
            "[tool.poetry]\nname = \"wrong\"\n[project]\nnames = [\"x\"]\nname = 'engraph-fixture' # the one\n[project.urls]\nname = \"also wrong\"\n",
        )
        .unwrap();
        assert_eq!(python_project_name(&repo), "engraph-fixture");
        // A [project] table without a name: the directory again.
        std::fs::write(repo.join("pyproject.toml"), "[project]\nversion = \"1\"\n").unwrap();
        assert_eq!(python_project_name(&repo), "elm");
        let cmd = ScipPython.command(&repo, Path::new("/tmp/out/index.scip"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"elm".to_string()));
    }

    /// Plan 10, part 1, step 4: the fallback name must be the same in every
    /// checkout of a repo. A `pyproject.toml` with no name at the root of a
    /// repo would otherwise put the name of the checkout directory into every
    /// first-party id, and a worktree's overlay rows would not join the base
    /// rows (the ids differ in the package field).
    #[test]
    fn python_project_name_is_the_same_in_every_worktree_of_a_repo() {
        fn git(cwd: &Path, args: &[&str]) {
            let out = engraph_core::utils::git_cmd(cwd)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("engraph");
        std::fs::create_dir_all(&main).unwrap();
        // A [project] table without a name, so the fallback decides.
        std::fs::write(main.join("pyproject.toml"), "[project]\nversion = \"1\"\n").unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-q", "-m", "init"]);
        let wt = dir.path().join("elm");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );

        // Both checkouts name the repo by its main worktree, not by their own
        // directory.
        assert_eq!(python_project_name(&main), "engraph");
        assert_eq!(python_project_name(&wt), "engraph");
    }

    #[test]
    fn pyproject_name_handles_real_toml() {
        // Shapes a line reader misses: a header with a trailing comment, a
        // dotted key, an inline table, and a Poetry project without a
        // [project] table.
        assert_eq!(
            pyproject_name("[project]  # metadata\nname = \"a\"\n").as_deref(),
            Some("a")
        );
        assert_eq!(
            pyproject_name("project.name = \"b\"\n").as_deref(),
            Some("b")
        );
        assert_eq!(
            pyproject_name("project = { name = \"c\" }\n").as_deref(),
            Some("c")
        );
        assert_eq!(
            pyproject_name("[tool.poetry]\nname = \"d\"\n").as_deref(),
            Some("d")
        );
        // A [project] table without a name falls back to Poetry's.
        assert_eq!(
            pyproject_name("[project]\nversion = \"1\"\n[tool.poetry]\nname = \"e\"\n").as_deref(),
            Some("e")
        );
        // A top-level name, a non-string name, broken TOML: no name.
        assert_eq!(pyproject_name("name = \"top\"\n"), None);
        assert_eq!(pyproject_name("[project]\nname = 3\n"), None);
        assert_eq!(pyproject_name("[project\nname = \"x\"\n"), None);
    }

    #[test]
    fn scip_python_command_pins_project_version() {
        let cmd = ScipPython.command(
            Path::new("/tmp/some-repo"),
            Path::new("/tmp/out/index.scip"),
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Pinning --project-version avoids scip-python's undefined-version crash.
        assert!(args.contains(&"--project-version".to_string()));
        assert!(args.contains(&SCIP_PYTHON_VERSION.to_string()));
        assert!(args.contains(&"--project-name".to_string()));
        // The caller-chosen output path must reach scip-python's --output.
        assert!(args.contains(&"--output".to_string()));
        assert!(args.contains(&"/tmp/out/index.scip".to_string()));
    }

    /// The args `typescript_command` gives for `repo` when `on_path` says
    /// which tools are installed.
    fn ts_args(repo: &Path, on_path: &dyn Fn(&str) -> bool) -> Vec<String> {
        typescript_command(repo, Path::new("/tmp/out/index.scip"), on_path)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn ts_repo(package_json: &str, extra: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), package_json).unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        for f in extra {
            std::fs::write(dir.path().join(f), "packages:\n  - 'packages/*'\n").unwrap();
        }
        dir
    }

    #[test]
    fn go_work_uses_reads_single_and_block_forms() {
        let text = "go 1.21\n\nuse ./a // first\nuse (\n\t.\n\t\"./b/c\"\n\t/abs/d\n\t// comment line\n)\n\nuser ./not-a-use\n";
        assert_eq!(parse_go_work_uses(text), ["./a", ".", "./b/c", "/abs/d"]);
    }

    #[test]
    fn scip_go_names_each_go_work_module() {
        let dir = tempfile::tempdir().unwrap();
        let args = |dir: &Path| -> Vec<String> {
            ScipGo
                .command(dir, Path::new("/tmp/out/index.scip"))
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let root = dir.path().to_string_lossy().into_owned();
        let base = [
            "index",
            "--module-root",
            &root,
            "--module-version",
            SCIP_GO_VERSION,
            "--output",
            "/tmp/out/index.scip",
        ];
        // A plain module: scip-go's default `./...` covers it.
        std::fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
        assert_eq!(args(dir.path()), base);
        // A go.work: scip-go indexes only the patterns it is given, so one
        // per `use` directory, the root included.
        std::fs::write(
            dir.path().join("go.work"),
            "go 1.21\n\nuse (\n\t.\n\t./tools/\n\tsvc\n)\n",
        )
        .unwrap();
        assert_eq!(
            args(dir.path()),
            [&base[..], &["./...", "./tools/...", "./svc/..."]].concat()
        );
    }

    #[test]
    fn go_tags_append_to_the_goflags_of_the_user() {
        // The last `-tags` in GOFLAGS wins, so the appended one does.
        assert_eq!(go_flags_with_tags(None, "a,b"), "-tags=a,b");
        assert_eq!(
            go_flags_with_tags(Some("-mod=mod -tags=x"), "a"),
            "-mod=mod -tags=x -tags=a"
        );
        assert_eq!(go_flags_with_tags(Some("  "), "a"), "-tags=a");
    }

    #[test]
    fn scip_go_turns_off_a_go_work_that_does_not_list_the_module() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("go.work"), "go 1.21\n\nuse ./a\n").unwrap();
        for m in ["a", "c"] {
            std::fs::create_dir_all(root.join(m)).unwrap();
            std::fs::write(root.join(m).join("go.mod"), "module x\n").unwrap();
        }
        let gowork = |dir: &Path| {
            ScipGo
                .command(dir, Path::new("/tmp/out/index.scip"))
                .get_envs()
                .find(|(k, _)| *k == "GOWORK")
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        // Listed, or the go.work root itself: Go uses the go.work.
        assert_eq!(gowork(&root.join("a")), None);
        assert_eq!(gowork(root), None);
        // Not listed: Go would build nothing in it.
        assert_eq!(gowork(&root.join("c")), Some("off".into()));
    }

    #[test]
    fn scip_typescript_indexes_pnpm_and_yarn_workspaces_with_their_root() {
        let all = |_: &str| true;
        let base = ["index", "--output", "/tmp/out/index.scip"];

        let pnpm = ts_repo("{}", &["pnpm-workspace.yaml"]);
        let root = pnpm.path().canonicalize().unwrap();
        let root = root.to_string_lossy();
        assert_eq!(
            ts_args(pnpm.path(), &all),
            [&base[..], &["--pnpm-workspaces", &root]].concat()
        );

        let yarn = ts_repo(r#"{"workspaces": ["packages/*"]}"#, &["yarn.lock"]);
        let root = yarn.path().canonicalize().unwrap();
        let root = root.to_string_lossy();
        assert_eq!(
            ts_args(yarn.path(), &all),
            [&base[..], &["--yarn-workspaces", &root]].concat()
        );
    }

    #[test]
    fn scip_typescript_runs_plain_without_a_usable_workspace() {
        let all = |_: &str| true;
        let plain = ["index", "--output", "/tmp/out/index.scip"];
        // npm workspaces: no flag can list them.
        let npm = ts_repo(r#"{"workspaces": ["packages/*"]}"#, &[]);
        assert_eq!(ts_args(npm.path(), &all), plain);
        // A yarn.lock without workspaces is no yarn workspace.
        let yarn = ts_repo("{}", &["yarn.lock"]);
        assert_eq!(ts_args(yarn.path(), &all), plain);
        // The package manager isn't installed: scip-typescript would fail to
        // list the workspace, so it indexes the root alone, as before.
        let pnpm = ts_repo("{}", &["pnpm-workspace.yaml"]);
        assert_eq!(ts_args(pnpm.path(), &|tool| tool != "pnpm"), plain);
        // A packages list the line scan can't read (flow form): the flag
        // would index packages discovery records as their own projects.
        let flow = ts_repo("{}", &[]);
        std::fs::write(
            flow.path().join("pnpm-workspace.yaml"),
            "packages: ['packages/*']\n",
        )
        .unwrap();
        assert_eq!(ts_args(flow.path(), &all), plain);
    }
}
