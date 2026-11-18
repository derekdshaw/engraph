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

    /// Copy the fixture into `into/<lang>` and return that path. The basename
    /// is kept on purpose: scip-python writes the indexed directory's name
    /// into every first-party symbol id (`--project-name`).
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
    version_output(bin).is_some()
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
    Command::new("go")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The first line `bin --version` prints (stdout, else stderr), or `None`
/// when the binary is absent or the probe fails.
pub fn indexer_version(bin: &str) -> Option<String> {
    let out = version_output(bin)?;
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

fn version_output(bin: &str) -> Option<std::process::Output> {
    Command::new(bin)
        .arg("--version")
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

/// A fresh database at `dir/eg.db`.
pub fn db_for_test(dir: &Path) -> Pool {
    open_pool(&dir.join("eg.db")).unwrap()
}
