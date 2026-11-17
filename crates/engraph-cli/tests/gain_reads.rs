//! `engraph gain --reads`: the tokens of all file reads, and of reads of a
//! file that the session read before.

mod common;
use common::engraph;
use engraph_core::{db, telemetry};
use tempfile::tempdir;

fn db_with_reads() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.db");
    let pool = db::open_pool(&path).unwrap();
    let conn = pool.get().unwrap();
    telemetry::record_read(&conn, Some("s1"), "text", "/r/a.rs", 100, 0).unwrap();
    telemetry::record_read(&conn, Some("s1"), "text", "/r/a.rs", 40, 0).unwrap();
    telemetry::record_read(&conn, Some("s1"), "text", "/r/b.rs", 30, 0).unwrap();
    (dir, path)
}

#[test]
fn reads_text_shows_all_and_repeat_reads() {
    let (dir, db) = db_with_reads();
    let out = engraph(&db, dir.path(), &["gain", "--reads"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let line = |key: &str| {
        text.lines()
            .find(|l| l.split_whitespace().next() == Some(key))
            .unwrap_or_else(|| panic!("no `{key}` row in:\n{text}"))
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(line("all")[1..3], ["3", "170"]);
    assert_eq!(line("repeat")[1..3], ["1", "40"]);
}

#[test]
fn reads_json_has_a_reads_object() {
    let (dir, db) = db_with_reads();
    let out = engraph(&db, dir.path(), &["gain", "--reads", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["reads"]["reads"], 3);
    assert_eq!(v["reads"]["read_tokens"], 170);
    assert_eq!(v["reads"]["repeat_reads"], 1);
    assert_eq!(v["reads"]["repeat_tokens"], 40);
    // Reads do not change the savings totals.
    assert_eq!(v["summary"]["commands"], 0);
}

#[test]
fn all_includes_reads_and_plain_gain_does_not() {
    let (dir, db) = db_with_reads();
    let all: serde_json::Value =
        serde_json::from_slice(&engraph(&db, dir.path(), &["gain", "--all", "--json"]).stdout)
            .unwrap();
    assert_eq!(all["reads"]["reads"], 3);
    let plain: serde_json::Value =
        serde_json::from_slice(&engraph(&db, dir.path(), &["gain", "--json"]).stdout).unwrap();
    assert!(plain.get("reads").is_none());
}

#[test]
fn reads_csv_has_two_rows() {
    let (dir, db) = db_with_reads();
    let out = engraph(&db, dir.path(), &["gain", "--reads", "--format", "csv"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().any(|l| l.starts_with("reads,all,3,170,")),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("reads,repeat,1,40,")),
        "{text}"
    );
}
