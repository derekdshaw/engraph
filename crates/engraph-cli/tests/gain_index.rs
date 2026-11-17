//! `engraph gain --index`: one row per `filter_id` of the `index` events.

mod common;
use common::engraph;
use engraph_core::{
    db,
    models::EventKind,
    telemetry::{self, EventInput},
};
use tempfile::tempdir;

fn db_with_index_runs() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.db");
    let pool = db::open_pool(&path).unwrap();
    let conn = pool.get().unwrap();
    for (filter_id, bytes, ms) in [
        ("auto", 1000, 100),
        ("auto", 3000, 300),
        ("if_changed", 500, 50),
        ("rust-analyzer", 9000, 2000),
    ] {
        telemetry::record_event(
            &conn,
            EventInput {
                session_id: None,
                kind: EventKind::Index,
                feature: "codegraph_index",
                filter_id: Some(filter_id),
                input_tokens: bytes,
                output_tokens: 0,
                latency_ms: ms,
            },
        )
        .unwrap();
    }
    (dir, path)
}

fn json(db: &std::path::Path, dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let out = engraph(db, dir, args);
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn index_text_shows_one_row_per_filter_id() {
    let (dir, db) = db_with_index_runs();
    let out = engraph(&db, dir.path(), &["gain", "--index"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let row = |key: &str| {
        text.lines()
            .find(|l| l.split_whitespace().next() == Some(key))
            .unwrap_or_else(|| panic!("no `{key}` row in:\n{text}"))
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    // runs, total_ms, mean_ms, scip_bytes
    assert_eq!(row("auto")[1..], ["2", "400", "200", "4000"]);
    assert_eq!(row("if_changed")[1..], ["1", "50", "50", "500"]);
    assert_eq!(row("rust-analyzer")[1..], ["1", "2000", "2000", "9000"]);
}

#[test]
fn index_json_and_all_have_the_rows_and_the_totals_do_not_change() {
    let (dir, db) = db_with_index_runs();
    let plain = json(&db, dir.path(), &["gain", "--json"]);
    assert!(plain.get("index").is_none());
    for args in [
        &["gain", "--index", "--json"][..],
        &["gain", "--all", "--json"][..],
    ] {
        let v = json(&db, dir.path(), args);
        let rows = v["index"].as_array().expect("index rows");
        assert_eq!(rows.len(), 3);
        let auto = rows.iter().find(|r| r["filter_id"] == "auto").unwrap();
        assert_eq!(auto["runs"], 2);
        assert_eq!(auto["total_ms"], 400);
        assert_eq!(auto["scip_bytes"], 4000);
        assert_eq!(v["summary"], plain["summary"]);
    }
}

#[test]
fn index_csv_has_one_row_per_filter_id() {
    let (dir, db) = db_with_index_runs();
    let out = engraph(&db, dir.path(), &["gain", "--index", "--format", "csv"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().any(|l| l.starts_with("index,auto,2,4000,")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with("index,if_changed,1,500,")),
        "{text}"
    );
}
