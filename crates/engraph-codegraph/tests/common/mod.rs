//! Helpers shared by the codegraph integration tests. Each test binary that
//! declares `mod common;` compiles the whole module and uses a subset of it,
//! hence the `dead_code` allowance.
#![allow(dead_code)]

pub mod snapshot;

use engraph_core::db::{Pool, open_pool};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One stable-symbol-id fixture repo (`testdata/fixtures/<lang>`) and the
/// indexer that drives it.
pub struct Fixture {
    pub lang: &'static str,
    pub indexer: &'static str,
}

pub const FIXTURES: [Fixture; 5] = [
    Fixture {
        lang: "rust",
        indexer: "rust-analyzer",
    },
    Fixture {
        lang: "python",
        indexer: "scip-python",
    },
    Fixture {
        lang: "go",
        indexer: "scip-go",
    },
    Fixture {
        lang: "typescript",
        indexer: "scip-typescript",
    },
    Fixture {
        lang: "java",
        indexer: "scip-java",
    },
];

impl Fixture {
    /// The fixture's source tree in the repo.
    pub fn dir(&self) -> PathBuf {
        testdata().join("fixtures").join(self.lang)
    }

    /// The snapshot file of the fixture's symbol ids.
    pub fn snapshot_path(&self) -> PathBuf {
        testdata()
            .join("snapshots")
            .join(format!("{}.json", self.lang))
    }

    /// The prebuilt SCIP file of the fixture (plan 05, part 1, step 15).
    pub fn scip_path(&self) -> PathBuf {
        testdata().join("scip").join(format!("{}.scip", self.lang))
    }

    /// `tree_hash` of the fixture's sources as they are in the repo now.
    pub fn source_hash(&self) -> String {
        tree_hash(&self.dir())
    }

    /// The project key the fixture is indexed under in a test database.
    pub fn project(&self) -> String {
        format!("/fixtures/{}", self.lang)
    }

    /// Why this machine cannot index the fixture, or `None` when it can:
    /// the indexer is absent, or scip-java has no build tool to drive.
    pub fn missing_tool(&self) -> Option<String> {
        if !binary_present(self.indexer) {
            return Some(format!("{} not installed", self.indexer));
        }
        if self.indexer == "scip-java" && java_build_tool().is_none() {
            return Some("scip-java needs `mvn` or `gradle` on PATH; none found".into());
        }
        if self.indexer == "scip-go" && !go_toolchain_present() {
            return Some("scip-go needs the `go` toolchain on PATH; `go version` failed".into());
        }
        None
    }

    /// Copy the fixture into `into/<lang>` and return that path. The python
    /// fixture's ids take their package name from its `pyproject.toml`, not
    /// from this directory name.
    pub fn copy_into(&self, into: &Path) -> PathBuf {
        let dst = into.join(self.lang);
        copy_dir_all(&self.dir(), &dst);
        dst
    }
}

/// `crates/engraph-codegraph/testdata`.
pub fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// One SHA-256 over every file under `dir` — its path relative to `dir` and
/// its bytes, in path order — so any edit, rename, or added file changes it.
/// Build output that an in-place `cargo check` or `npm` run would leave
/// behind (`target`, `node_modules`, `__pycache__`, `Cargo.lock`) is skipped,
/// so only the sources count.
pub fn tree_hash(dir: &Path) -> String {
    fn visit(dir: &Path, root: &Path, buf: &mut Vec<u8>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy();
            if matches!(
                name.as_ref(),
                "target" | "node_modules" | "__pycache__" | "Cargo.lock"
            ) {
                continue;
            }
            if path.is_dir() {
                visit(&path, root, buf);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                buf.extend_from_slice(rel.as_bytes());
                buf.push(0);
                buf.extend_from_slice(&std::fs::read(&path).unwrap());
                buf.push(0);
            }
        }
    }
    let mut buf = Vec::new();
    visit(dir, dir, &mut buf);
    engraph_core::utils::sha256_hex(&buf)
}

fn copy_dir_all(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Whether `bin` is on PATH and answers `--version`.
pub fn binary_present(bin: &str) -> bool {
    probe(bin, "--version").is_some()
}

/// Whether `bazel` or `bazelisk` is on PATH.
pub fn bazel_present() -> bool {
    ["bazel", "bazelisk"].iter().any(|b| binary_present(b))
}

/// The build tool scip-java can drive on this machine. scip-java is a
/// frontend: it shells out to `mvn` or `gradle` for the build that produces
/// SemanticDB, so without one an index run fails on an otherwise fine repo.
pub fn java_build_tool() -> Option<&'static str> {
    ["mvn", "gradle"].into_iter().find(|t| binary_present(t))
}

/// The SCIP blob of an index run with the machine's paths removed, fit to
/// keep in the repo: `metadata.project_root` (the `file://` URI of the
/// indexed tree, a temp path in an update run) becomes a fixed URI, and
/// `metadata.tool_info.arguments` (the indexer's argv, which carries the repo
/// and output paths) is cleared. Everything else — documents, symbols,
/// occurrences — holds only repo-relative paths, so the result is the same on
/// every machine; the indexer's name and version stay.
///
/// The documents, each document's symbols and their relationships, and the
/// external symbols are sorted too. SCIP treats them as sets, but scip-go
/// writes symbols in Go map order and scip-java writes documents in directory
/// order, so two runs of one fixture give the same bytes only after sorting.
/// Occurrences keep the indexer's (source) order.
pub fn normalized_scip(bytes: &[u8]) -> Vec<u8> {
    use protobuf::Message;
    let mut index = scip::types::Index::parse_from_bytes(bytes).unwrap();
    let metadata = index.metadata.mut_or_insert_default();
    metadata.project_root = "file:///fixture".into();
    metadata.tool_info.mut_or_insert_default().arguments = Vec::new();
    let sort_infos = |infos: &mut Vec<scip::types::SymbolInformation>| {
        infos.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        for info in infos {
            info.relationships.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        }
    };
    index
        .documents
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    for doc in &mut index.documents {
        sort_infos(&mut doc.symbols);
    }
    sort_infos(&mut index.external_symbols);
    index.write_to_bytes().unwrap()
}

/// Whether the Go toolchain is on PATH. `go` rejects `--version` (it prints
/// its help text and exits 2 even when installed), so `binary_present` would
/// call it absent; the probe is the `go version` subcommand, which exits 0.
pub fn go_toolchain_present() -> bool {
    probe("go", "version").is_some()
}

/// The first line `bin --version` prints (stdout, else stderr), or `None`
/// when the binary is absent or the probe fails.
pub fn indexer_version(bin: &str) -> Option<String> {
    let out = probe(bin, "--version")?;
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    let line = String::from_utf8_lossy(&text)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    Some(line)
}

/// The output of `bin arg`, or `None` when the binary is absent or exits
/// with a failure.
fn probe(bin: &str, arg: &str) -> Option<std::process::Output> {
    Command::new(bin)
        .arg(arg)
        .output()
        .ok()
        .filter(|o| o.status.success())
}

/// Redirect the SCIP scratch file off the user's real data dir
/// (`~/.local/share/engraph/scip`) and into OS temp, so running the suites
/// doesn't accumulate `index.scip` files there. Set exactly once via `Once`:
/// tests run in parallel and `index_repo` reads the env var with `getenv`,
/// which is not safe against a concurrent `set_var` — so the write must
/// complete before any test proceeds. All tests share one base; the per-repo
/// hash subdir inside it keeps each test's output separate.
pub fn isolate_scip_output() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: `Once` serializes this write and runs it before any test
        // body reaches `index_repo`, so no concurrent `getenv` overlaps it.
        unsafe {
            std::env::set_var(
                "ENGRAPH_SCIP_OUTPUT_DIR",
                std::env::temp_dir().join("engraph-test-scip"),
            );
        }
    });
}

/// One test's index locks, in a temp dir instead of the user's real
/// `~/.local/share/engraph/locks`. The locks live next to `ENGRAPH_DB_PATH`
/// and their files are never deleted (`engraph_codegraph::lock`), so without
/// this every indexing test run leaves its lock files there. On drop the
/// variable is put back and the dir, lock files included, is removed.
///
/// Holds a mutex for its whole life: the variable is process-global, so a
/// second test in the binary must not repoint it while this one indexes.
pub struct IsolatedLocks {
    old: Option<std::ffi::OsString>,
    _dir: tempfile::TempDir,
    // Declared last, so it is released after the variable and the dir.
    _guard: std::sync::MutexGuard<'static, ()>,
}

pub fn isolate_index_locks() -> IsolatedLocks {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A failed test poisons the mutex; the next test still gets the lock.
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let old = std::env::var_os("ENGRAPH_DB_PATH");
    set_db_path(Some(dir.path().join("eg.db").as_os_str()));
    IsolatedLocks {
        old,
        _dir: dir,
        _guard: guard,
    }
}

impl Drop for IsolatedLocks {
    fn drop(&mut self) {
        set_db_path(self.old.as_deref());
    }
}

fn set_db_path(value: Option<&std::ffi::OsStr>) {
    // SAFETY: the `isolate_index_locks` mutex is held, so no other test in
    // this binary writes the variable meanwhile, and the code that reads it
    // (`engraph_core::db::default_db_path`) uses `std::env::var`, which takes
    // std's env lock, the one `set_var` takes.
    unsafe {
        match value {
            Some(v) => std::env::set_var("ENGRAPH_DB_PATH", v),
            None => std::env::remove_var("ENGRAPH_DB_PATH"),
        }
    }
}

/// A fresh database at `dir/eg.db`.
pub fn db_for_test(dir: &Path) -> Pool {
    open_pool(&dir.join("eg.db")).unwrap()
}
