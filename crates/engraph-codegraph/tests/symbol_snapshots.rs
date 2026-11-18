//! Stable symbol ids (plan 05, part 1): snapshots of the symbol ids each
//! fixture repo in `testdata/fixtures` produces.
//!
//! The live test indexes each fixture whose indexer is installed and compares
//! the result with `testdata/snapshots/<lang>.json`; a missing indexer skips
//! that fixture, as in `drivers_live.rs`. With `ENGRAPH_UPDATE_SNAPSHOTS=1`
//! it writes the snapshot files and the prebuilt `testdata/scip/<lang>.scip`
//! blobs (machine paths normalized away) instead. Examine the diff before you commit a
//! new snapshot: a changed id can be a correct change of the indexer, or a
//! regression in the loader.

mod common;

use common::snapshot::{Snapshot, SymbolRecord};
use common::{
    FIXTURES, Fixture, binary_present, db_for_test, indexer_version, isolate_scip_output,
    normalized_scip, tree_hash,
};
use engraph_codegraph::{index_repo, load, scip_output_dir};
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

    let snap = Snapshot::from_db(&conn, p, "x", "x 1.0", "h");
    assert_eq!(snap.indexer, "x");
    assert_eq!(snap.indexer_version, "x 1.0");
    assert_eq!(snap.fixture_hash, "h");
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
fn snapshot_diff_lists_a_changed_hash_and_symbols_not_the_version() {
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
        fixture_hash: "h".into(),
        symbols: vec![
            rec("a", "a.rs", &["c"]),
            rec("b", "b.rs", &[]),
            rec("c", "c.rs", &[]),
        ],
    };
    let run = Snapshot {
        indexer: "x".into(),
        indexer_version: "2".into(),
        fixture_hash: "h2".into(),
        symbols: vec![
            rec("a", "a2.rs", &["d"]),
            rec("c", "c.rs", &[]),
            rec("d", "d.rs", &[]),
        ],
    };
    assert_eq!(
        snapshot.diff(&run),
        [
            "fixture_hash: h (snapshot) -> h2 (index run)",
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
    // Another indexer version with the same symbols is no difference.
    let newer = Snapshot {
        indexer_version: "2".into(),
        ..snapshot.clone()
    };
    assert!(snapshot.diff(&newer).is_empty());
}

#[test]
fn normalized_scip_keeps_only_machine_free_metadata() {
    use protobuf::Message;
    let index_at = |root: &str, args: &[&str]| {
        let mut index = scip::types::Index::new();
        let metadata = index.metadata.mut_or_insert_default();
        metadata.project_root = root.into();
        let tool = metadata.tool_info.mut_or_insert_default();
        tool.name = "scip-test".into();
        tool.version = "1.2.3".into();
        tool.arguments = args.iter().map(|a| a.to_string()).collect();
        let mut doc = scip::types::Document::new();
        doc.relative_path = "src/lib.rs".into();
        index.documents.push(doc);
        index.write_to_bytes().unwrap()
    };
    let normalized = normalized_scip(&index_at(
        "file:///tmp/one/repo",
        &["--cwd", "/tmp/one/repo"],
    ));
    let index = scip::types::Index::parse_from_bytes(&normalized).unwrap();
    assert_eq!(index.metadata.project_root, "file:///fixture");
    assert!(index.metadata.tool_info.arguments.is_empty());
    assert_eq!(index.metadata.tool_info.name, "scip-test");
    assert_eq!(index.metadata.tool_info.version, "1.2.3");
    assert_eq!(index.documents[0].relative_path, "src/lib.rs");
    // Two runs of the same fixture from different directories give the same
    // bytes, so a regeneration does not churn the committed file.
    assert_eq!(
        normalized,
        normalized_scip(&index_at(
            "file:///tmp/two/repo",
            &["--cwd", "/tmp/two/repo"]
        ))
    );
}

#[test]
fn normalized_scip_sorts_the_set_like_lists() {
    use protobuf::Message;
    // scip-go writes a document's symbols in Go map order and scip-java its
    // documents in directory order: the same index, differently ordered,
    // must normalize to the same bytes.
    let info = |symbol: &str, rels: &[&str]| {
        let mut i = scip::types::SymbolInformation::new();
        i.symbol = symbol.into();
        for r in rels {
            let mut rel = scip::types::Relationship::new();
            rel.symbol = r.to_string();
            i.relationships.push(rel);
        }
        i
    };
    let doc = |path: &str, symbols: Vec<scip::types::SymbolInformation>| {
        let mut d = scip::types::Document::new();
        d.relative_path = path.into();
        d.symbols = symbols;
        d
    };
    let index_of = |docs: Vec<scip::types::Document>,
                    external: Vec<scip::types::SymbolInformation>| {
        let mut index = scip::types::Index::new();
        index.documents = docs;
        index.external_symbols = external;
        index.write_to_bytes().unwrap()
    };
    let one = index_of(
        vec![
            doc("b.go", vec![info("s2", &[]), info("s1", &["r2", "r1"])]),
            doc("a.go", vec![]),
        ],
        vec![info("e2", &[]), info("e1", &[])],
    );
    let other = index_of(
        vec![
            doc("a.go", vec![]),
            doc("b.go", vec![info("s1", &["r1", "r2"]), info("s2", &[])]),
        ],
        vec![info("e1", &[]), info("e2", &[])],
    );
    assert_eq!(normalized_scip(&one), normalized_scip(&other));
    let sorted = scip::types::Index::parse_from_bytes(&normalized_scip(&one)).unwrap();
    assert_eq!(sorted.documents[0].relative_path, "a.go");
    assert_eq!(sorted.documents[1].symbols[0].symbol, "s1");
    assert_eq!(sorted.documents[1].symbols[0].relationships[0].symbol, "r1");
    assert_eq!(sorted.external_symbols[0].symbol, "e1");
}

#[test]
fn tree_hash_follows_the_sources_and_ignores_build_output() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
    let before = tree_hash(root);
    assert_eq!(before.len(), 64);
    assert_eq!(tree_hash(root), before, "deterministic");
    // Build output does not count.
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join("target/debug/x"), "junk").unwrap();
    std::fs::write(root.join("Cargo.lock"), "lock").unwrap();
    assert_eq!(tree_hash(root), before);
    // A changed byte, a new file, and a rename each count.
    std::fs::write(root.join("src/a.rs"), "fn a() { }").unwrap();
    let edited = tree_hash(root);
    assert_ne!(edited, before);
    std::fs::write(root.join("src/b.rs"), "").unwrap();
    let added = tree_hash(root);
    assert_ne!(added, edited);
    std::fs::rename(root.join("src/b.rs"), root.join("src/c.rs")).unwrap();
    assert_ne!(tree_hash(root), added);
}

#[test]
fn snapshot_diff_marks_an_order_only_change() {
    // The same ids in a different order: the record differs, but no id went
    // or came, so the set-style caller lines would say nothing at all.
    let rec = |callers: &[&str]| Snapshot {
        indexer: "x".into(),
        indexer_version: "1".into(),
        fixture_hash: "h".into(),
        symbols: vec![SymbolRecord {
            id: "a".into(),
            name: "a".into(),
            file_path: "a.rs".into(),
            line_range: "1:1".into(),
            callers: callers.iter().map(|c| c.to_string()).collect(),
            callees: vec![],
        }],
    };
    assert_eq!(
        rec(&["c", "d"]).diff(&rec(&["d", "c"])),
        [
            "only in the snapshot: 0, only in the index run: 0, changed: 1",
            "~ a",
            "    callers: order changed",
        ]
    );
}

/// Plan 05 test: two index runs of a Go repo at different commits give the
/// same Go symbol ids. The driver pins the module version, so nothing from a
/// commit reaches an id; the ids of the first run are all in the second, and
/// the second adds only the function the commit added. Skips without
/// scip-go, the Go toolchain, or git.
#[test]
fn go_ids_are_the_same_at_two_commits() {
    let go = FIXTURES.iter().find(|f| f.lang == "go").unwrap();
    if let Some(why) = go.missing_tool() {
        eprintln!("skip: {why}");
        return;
    }
    if !binary_present("git") {
        eprintln!("skip: git not installed");
        return;
    }
    isolate_scip_output();
    let dir = tempdir().unwrap();
    let repo = go.copy_into(dir.path());
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let ids_at = |run: &str| -> std::collections::BTreeSet<String> {
        let db_dir = dir.path().join(run);
        std::fs::create_dir_all(&db_dir).unwrap();
        let pool = db_for_test(&db_dir);
        let conn = pool.get().unwrap();
        index_repo(&conn, &repo, None, None, &go.project(), false, false).unwrap();
        Snapshot::from_db(&conn, &go.project(), go.indexer, "", "")
            .symbols
            .into_iter()
            .map(|s| s.id)
            .collect()
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "first"]);
    let first = ids_at("first");

    let text_go = repo.join("text/text.go");
    let mut text = std::fs::read_to_string(&text_go).unwrap();
    text.push_str("\nfunc Shout(s string) string {\n\treturn strings.ToUpper(s) + \"!\"\n}\n");
    std::fs::write(&text_go, text).unwrap();
    git(&["commit", "-q", "-am", "second"]);
    let second = ids_at("second");

    assert!(
        first.is_subset(&second),
        "ids lost between commits: {:?}",
        first.difference(&second).collect::<Vec<_>>()
    );
    let added: Vec<&String> = second.difference(&first).collect();
    assert_eq!(added.len(), 1, "{added:?}");
    assert!(
        added[0].ends_with("`example.com/engraph/fixture/text`/Shout()."),
        "{}",
        added[0]
    );
}

/// Compare `actual` with `want`, the fixture's snapshot. Equal: print `ok`
/// and give `None`. Otherwise the failure, headed `<lang> <what> vs
/// <snapshot path>`, with the differences and then `note`, if any.
fn compare(
    fixture: &Fixture,
    what: &str,
    want: &Snapshot,
    actual: &Snapshot,
    note: Option<String>,
    ok: String,
) -> Option<String> {
    let mut diff = want.diff(actual);
    if diff.is_empty() {
        eprintln!("{ok}");
        return None;
    }
    diff.extend(note);
    Some(format!(
        "{} {what} vs {}:\n{}",
        fixture.lang,
        fixture.snapshot_path().display(),
        diff.join("\n")
    ))
}

/// Step 16: load each prebuilt `testdata/scip/<lang>.scip` blob and compare
/// the result with the snapshot file. No indexer runs — the blob is the
/// indexer's output, committed — so this operates in every CI job and a
/// difference can only come from a change to the loader (or a blob committed
/// without its snapshot). The indexer name and version come from the snapshot
/// itself, so only the symbols can differ — and the fixture hash, which says
/// the sources changed and both files are stale (a fixture whose indexer CI
/// does not install, like Java, has no other guard).
#[test]
fn prebuilt_scip_blobs_load_to_the_snapshots() {
    let mut failures = Vec::new();
    for fixture in &FIXTURES {
        let dir = tempdir().unwrap();
        let pool = db_for_test(dir.path());
        let conn = pool.get().unwrap();
        let blob = std::fs::read(fixture.scip_path())
            .unwrap_or_else(|e| panic!("reading {}: {e}", fixture.scip_path().display()));
        load(&conn, &fixture.project(), &blob)
            .unwrap_or_else(|e| panic!("loading {}: {e:#}", fixture.scip_path().display()));
        let want = Snapshot::read(&fixture.snapshot_path());
        let snap = Snapshot::from_db(
            &conn,
            &fixture.project(),
            &want.indexer,
            &want.indexer_version,
            &fixture.source_hash(),
        );
        let note = (want.fixture_hash != snap.fixture_hash).then(|| {
            format!(
                "note: the {} fixture sources changed after the snapshot and blob were \
                 made; regenerate with ENGRAPH_UPDATE_SNAPSHOTS=1 on a machine with {}.",
                fixture.lang, fixture.indexer
            )
        });
        let ok = format!(
            "ok: {} blob loads to its snapshot ({} symbols)",
            fixture.lang,
            snap.symbols.len()
        );
        failures.extend(compare(fixture, "blob", &want, &snap, note, ok));
    }
    assert!(
        failures.is_empty(),
        "the loader no longer turns the prebuilt SCIP blobs into the snapshots:\n\n{}",
        failures.join("\n\n")
    );
}

/// Steps 10 to 13: index each fixture repo and compare the result with its
/// snapshot file, skipping a fixture whose indexer (or its build tool /
/// toolchain) is not installed, and showing the differences of every fixture
/// that changed, with both indexer versions when they are not the same. A
/// new indexer version that writes the same symbols passes with a warning.
/// `ENGRAPH_UPDATE_SNAPSHOTS=1` writes the snapshots instead.
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
        let snap = Snapshot::from_db(
            &conn,
            &fixture.project(),
            fixture.indexer,
            &version,
            &fixture.source_hash(),
        );
        assert!(
            snap.symbols.len() >= MIN_SYMBOLS,
            "{} fixture defines {} symbols, want at least {MIN_SYMBOLS}",
            fixture.lang,
            snap.symbols.len()
        );
        if update {
            snap.write(&fixture.snapshot_path());
            let blob = std::fs::read(scip_output_dir(&repo).join("index.scip"))
                .expect("the index run leaves index.scip in scip_output_dir");
            std::fs::create_dir_all(fixture.scip_path().parent().unwrap()).unwrap();
            std::fs::write(fixture.scip_path(), normalized_scip(&blob)).unwrap();
            eprintln!(
                "wrote {} ({} symbols, {} {version}) and {}",
                fixture.snapshot_path().display(),
                snap.symbols.len(),
                fixture.indexer,
                fixture.scip_path().display()
            );
        } else {
            let want = Snapshot::read(&fixture.snapshot_path());
            let note = (want.indexer_version != version).then(|| {
                format!(
                    "note: this machine runs {} {version}; the snapshot was made with \
                     {}. The differences can be correct changes of the indexer \
                     version: examine them, then regenerate with \
                     ENGRAPH_UPDATE_SNAPSHOTS=1.",
                    fixture.indexer, want.indexer_version
                )
            });
            // CI greps for this line (ci.yml, codegraph-live).
            let ok = format!(
                "ok: {} matches its snapshot ({} symbols, {} {version})",
                fixture.lang,
                snap.symbols.len(),
                fixture.indexer
            );
            let failure = compare(fixture, "fixture", &want, &snap, note, ok);
            if failure.is_none() && want.indexer_version != version {
                eprintln!(
                    "warning: {} {version} writes the ids the snapshot recorded with {}; \
                     regenerate with ENGRAPH_UPDATE_SNAPSHOTS=1 to record the new version.",
                    fixture.indexer, want.indexer_version
                );
            }
            failures.extend(failure);
        }
    }
    assert!(
        failures.is_empty(),
        "fixture symbol ids changed:\n\n{}",
        failures.join("\n\n")
    );
}
