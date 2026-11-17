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

/// Fixed `--project-version` passed to scip-python (standalone driver and the
/// Bazel symbol pass). Intentionally overrides scip-python's resolved version
/// (git revision / pyproject) — see `ScipPython::command` for the full
/// rationale (crash avoidance + stable entity IDs).
pub(crate) const SCIP_PYTHON_VERSION: &str = "0.0.0";

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
        let project_name = repo
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string());
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
        c
    }
}

/// Fixed `--module-version` passed to scip-go by the `ScipGo` driver and the
/// Bazel multi-module symbol pass (`bazel_symbols::run_go_modules`). scip-go
/// defaults this to the git short hash of the cwd repo, which embeds into
/// every moniker — so without a pin, Go entity IDs churn on each commit. Same
/// rationale as `SCIP_PYTHON_VERSION`. The loader gives the same version to
/// the ids scip-go writes with the version `.`
/// (`scip_loader::pin_go_module_versions`).
pub(crate) const SCIP_GO_VERSION: &str = "0.0.0";

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
        // scip-go defaults --output to `index.scip` in the cwd; pin it to `out`
        // so nothing lands in the repo. Still chdir into the repo for module
        // resolution.
        let mut c = Command::new("scip-go");
        c.arg("--module-root")
            .arg(repo)
            .arg("--module-version")
            .arg(SCIP_GO_VERSION)
            .arg("--output")
            .arg(out)
            .current_dir(repo);
        // scip-go's default pattern `./...` reaches only the module of the
        // cwd. In a go.work, one `<dir>/...` pattern per `use` directory
        // indexes every module in one run, each under its own module path.
        if let Some(uses) = go_work_uses(repo) {
            c.args(uses.iter().map(|u| go_work_pattern(u)));
        }
        if outside_its_go_work(repo) {
            c.env("GOWORK", "off");
        }
        c
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
        .find_map(|work_dir| {
            let uses = go_work_uses(work_dir)?;
            let rel = crate::index::workspace_rel(work_dir, dir);
            Some(!uses.iter().any(|u| {
                let u = go_work_rel(u);
                u == rel || Path::new(u) == dir
            }))
        })
        .unwrap_or(false)
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
pub(crate) fn go_work_rel(path: &str) -> &str {
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
fn typescript_command(repo: &Path, out: &Path, on_path: &dyn Fn(&str) -> bool) -> Command {
    // scip-typescript defaults --output to `index.scip` in the cwd; pin it
    // to `out`. Still chdir into the repo so it picks up tsconfig.json.
    let mut c = Command::new("scip-typescript");
    c.arg("index").arg("--output").arg(out).current_dir(repo);
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
