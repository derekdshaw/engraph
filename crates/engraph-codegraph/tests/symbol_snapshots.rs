//! Stable symbol ids (plan 05, part 1): snapshots of the symbol ids each
//! fixture repo in `testdata/fixtures` produces.
//!
//! The live test indexes each fixture whose indexer is installed and compares
//! the result with `testdata/snapshots/<lang>.json`; a missing indexer skips
//! that fixture, as in `drivers_live.rs`. With `ENGRAPH_UPDATE_SNAPSHOTS=1`
//! it writes the snapshot files instead. Examine the diff before you commit a
//! new snapshot: a changed id can be a correct change of the indexer, or a
//! regression in the loader.

mod common;

use common::snapshot::{Snapshot, SymbolRecord};
use common::{FIXTURES, db_for_test, indexer_version, isolate_scip_output};
use engraph_codegraph::index_repo;
use tempfile::tempdir;

/// A fixture defines at least this many symbols (plan 05, part 1, step 3).
const MIN_SYMBOLS: usize = 50;

#[test]
fn snapshot_records_defined_symbols_with_callers_and_callees() {
    let dir = tempdir().unwrap();
    let pool = db_for_test(dir.path());
    let conn = pool.get().unwrap();
    let p = "/fixtures/x";
    let f = "x cargo fx 0.1.0 ops/f().";
    let g = "x cargo fx 0.1.0 models/g().";
    let t = "x cargo fx 0.1.0 models/T#";
    let ext = "x cargo std 1.0.0 vec/Vec#new().";
    for (id, project, path, range) in [
        (f, p, Some("src/ops.rs"), Some("3:5")),
        (g, p, Some("src/models.rs"), Some("1:2")),
        (t, p, Some("src/models.rs"), None),
        (
            "local 1@0123456789abcdef:src/ops.rs",
            p,
            Some("src/ops.rs"),
            Some("4:4"),
        ),
        (ext, p, None, None),
        (
            "x cargo other 0.1.0 lib/h().",
            "/other",
            Some("src/lib.rs"),
            Some("1:1"),
        ),
    ] {
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range)
             VALUES (?1, 'symbol', ?2, ?3, ?4, ?5)",
            rusqlite::params![id, id.rsplit('/').next().unwrap(), project, path, range],
        )
        .unwrap();
    }
    for (n, (src, dst, kind)) in [
        (f, g, "CALLS"),
        (f, g, "CALLS"),
        (f, ext, "CALLS"),
        (f, t, "REFERENCES"),
        ("x cargo other 0.1.0 lib/h().", f, "CALLS"),
    ]
    .into_iter()
    .enumerate()
    {
        conn.execute(
            "INSERT INTO relations (id, src_entity, dst_entity, kind) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![n.to_string(), src, dst, kind],
        )
        .unwrap();
    }

    let snap = Snapshot::from_db(&conn, p, "x", "x 1.0");
    assert_eq!(snap.indexer, "x");
    assert_eq!(snap.indexer_version, "x 1.0");
    // Sorted by id; the undefined type, the local, the external placeholder,
    // and the other project's symbol are left out.
    assert_eq!(
        snap.symbols,
        vec![
            SymbolRecord {
                id: g.into(),
                name: "g().".into(),
                file_path: "src/models.rs".into(),
                line_range: "1:2".into(),
                callers: vec![f.into()],
                callees: vec![],
            },
            SymbolRecord {
                id: f.into(),
                name: "f().".into(),
                file_path: "src/ops.rs".into(),
                line_range: "3:5".into(),
                // A caller from another project counts; the duplicate CALLS
                // edge is one callee; the REFERENCES edge is not a callee.
                callers: vec!["x cargo other 0.1.0 lib/h().".into()],
                callees: vec![g.into(), ext.into()],
            },
        ]
    );

    let path = dir.path().join("snap/x.json");
    snap.write(&path);
    assert_eq!(Snapshot::read(&path), snap);
    assert!(std::fs::read_to_string(&path).unwrap().ends_with("}\n"));
}

#[test]
fn snapshot_diff_lists_changed_versions_and_symbols() {
    let rec = |id: &str, path: &str, callers: &[&str]| SymbolRecord {
        id: id.into(),
        name: id.into(),
        file_path: path.into(),
        line_range: "1:1".into(),
        callers: callers.iter().map(|c| c.to_string()).collect(),
        callees: vec![],
    };
    let snapshot = Snapshot {
        indexer: "x".into(),
        indexer_version: "1".into(),
        symbols: vec![
            rec("a", "a.rs", &["c"]),
            rec("b", "b.rs", &[]),
            rec("c", "c.rs", &[]),
        ],
    };
    let run = Snapshot {
        indexer: "x".into(),
        indexer_version: "2".into(),
        symbols: vec![
            rec("a", "a2.rs", &["d"]),
            rec("c", "c.rs", &[]),
            rec("d", "d.rs", &[]),
        ],
    };
    assert_eq!(
        snapshot.diff(&run),
        [
            "indexer_version: 1 (snapshot) -> 2 (index run)",
            "only in the snapshot: 1, only in the index run: 1, changed: 1",
            "- b",
            "+ d",
            "~ a",
            "    file_path: a.rs -> a2.rs",
            "    callers: - c",
            "    callers: + d",
        ]
    );
    assert!(snapshot.diff(&snapshot).is_empty());
}

/// Steps 10 to 12: index each fixture repo and compare the result with its
/// snapshot file, skipping a fixture whose indexer (or, for scip-java, build
/// tool) is not installed, and showing the differences of every fixture that
/// changed; `ENGRAPH_UPDATE_SNAPSHOTS=1` writes the snapshots instead.
#[test]
fn fixture_ids_match_the_snapshots() {
    let update = std::env::var("ENGRAPH_UPDATE_SNAPSHOTS").as_deref() == Ok("1");
    isolate_scip_output();
    let mut failures = Vec::new();
    for fixture in &FIXTURES {
        if let Some(why) = fixture.missing_tool() {
            eprintln!("skip: {why}");
            continue;
        }
        let dir = tempdir().unwrap();
        let repo = fixture.copy_into(dir.path());
        let pool = db_for_test(dir.path());
        let conn = pool.get().unwrap();
        index_repo(&conn, &repo, None, None, &fixture.project(), false, false).unwrap_or_else(
            |e| {
                panic!(
                    "{} failed on the {} fixture: {e:#}",
                    fixture.indexer, fixture.lang
                )
            },
        );
        let version = indexer_version(fixture.indexer).unwrap();
        let snap = Snapshot::from_db(&conn, &fixture.project(), fixture.indexer, &version);
        assert!(
            snap.symbols.len() >= MIN_SYMBOLS,
            "{} fixture defines {} symbols, want at least {MIN_SYMBOLS}",
            fixture.lang,
            snap.symbols.len()
        );
        if update {
            snap.write(&fixture.snapshot_path());
            eprintln!(
                "wrote {} ({} symbols, {} {version})",
                fixture.snapshot_path().display(),
                snap.symbols.len(),
                fixture.indexer
            );
        } else {
            let diff = Snapshot::read(&fixture.snapshot_path()).diff(&snap);
            if diff.is_empty() {
                eprintln!(
                    "ok: {} matches its snapshot ({} symbols, {} {version})",
                    fixture.lang,
                    snap.symbols.len(),
                    fixture.indexer
                );
            } else {
                failures.push(format!(
                    "{} fixture vs {}:\n{}",
                    fixture.lang,
                    fixture.snapshot_path().display(),
                    diff.join("\n")
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "fixture symbol ids changed:\n\n{}",
        failures.join("\n\n")
    );
}
