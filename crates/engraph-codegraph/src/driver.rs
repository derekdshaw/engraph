use std::path::{Path, PathBuf};
use std::process::Command;

/// One adapter per (build system, language). Engraph shells out to the external
/// SCIP indexer; the driver is a thin file-probe + argv builder.
pub trait Driver: Send + Sync {
    fn name(&self) -> &'static str;
    fn detect(&self, repo: &Path) -> bool;
    fn command(&self, repo: &Path) -> Command;
    fn output_path(&self, repo: &Path) -> PathBuf {
        repo.join("index.scip")
    }
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
    fn command(&self, repo: &Path) -> Command {
        // rust-analyzer's `scip` defaults `--output` to `index.scip` in the
        // process's CWD, not the repo. Pin both: pass --output explicitly and
        // also chdir into the repo so any auxiliary files land alongside it.
        let mut c = Command::new("rust-analyzer");
        c.arg("scip")
            .arg(repo)
            .arg("--output")
            .arg(self.output_path(repo))
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
    fn command(&self, repo: &Path) -> Command {
        let project_name = repo
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string());
        // scip-python's --cwd defaults to the spawning process's CWD, and
        // --output is resolved relative to --cwd. Pin both so the SCIP file
        // lands at <repo>/index.scip regardless of where engraph was invoked.
        let mut c = Command::new("scip-python");
        c.arg("index")
            .arg("--cwd")
            .arg(repo)
            .arg("--output")
            .arg(self.output_path(repo))
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
    fn command(&self, repo: &Path) -> Command {
        let mut c = Command::new("scip-go");
        c.arg("--module-root").arg(repo).current_dir(repo);
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
    fn command(&self, repo: &Path) -> Command {
        let mut c = Command::new("scip-typescript");
        c.arg("index").current_dir(repo);
        c
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
    fn command(&self, repo: &Path) -> Command {
        // scip-java auto-detects Maven vs Gradle; let it. We just chdir into
        // the repo and pin --output so the SCIP file lands at <repo>/index.scip.
        let mut c = Command::new("scip-java");
        c.arg("index")
            .arg("--output")
            .arg(self.output_path(repo))
            .current_dir(repo);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scip_python_command_pins_project_version() {
        let cmd = ScipPython.command(Path::new("/tmp/some-repo"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Pinning --project-version avoids scip-python's undefined-version crash.
        assert!(args.contains(&"--project-version".to_string()));
        assert!(args.contains(&SCIP_PYTHON_VERSION.to_string()));
        assert!(args.contains(&"--project-name".to_string()));
    }
}
