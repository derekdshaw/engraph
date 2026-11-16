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

/// Fixed `--module-version` passed to scip-go by the Bazel multi-module symbol
/// pass (`bazel_symbols::run_go_modules`). scip-go defaults this to the git
/// short hash of the cwd repo, which embeds into every moniker — so without a
/// pin, Go entity IDs churn on each commit. Same rationale as
/// `SCIP_PYTHON_VERSION`. NOT applied to the standalone `ScipGo` driver below.
pub(crate) const SCIP_GO_VERSION: &str = "0.0.0";

pub struct ScipGo;
impl Driver for ScipGo {
    fn name(&self) -> &'static str {
        "scip-go"
    }
    fn detect(&self, repo: &Path) -> bool {
        repo.join("go.mod").is_file()
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
            .arg("--output")
            .arg(out)
            .current_dir(repo);
        c
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
        typescript_command(repo, out, &on_path)
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

/// Whether `tool` is an executable file in a directory of `PATH`.
pub(crate) fn on_path(tool: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        [
            tool.to_string(),
            format!("{tool}.exe"),
            format!("{tool}.cmd"),
        ]
        .iter()
        .any(|name| dir.join(name).is_file())
    })
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
