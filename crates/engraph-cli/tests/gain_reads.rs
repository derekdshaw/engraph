//! `engraph gain --reads`: the tokens of all file reads, and of reads of a
//! file that the session read before.

mod common;
use common::{engraph, row};
use engraph_core::{db, telemetry};
use tempfile::tempdir;

fn db_with_reads() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.db");
    let pool = db::open_pool(&path).unwrap();
    let conn = pool.get().unwrap();
    for (path, range, tokens) in [
        ("/r/a.rs", "1+10", 100),
        ("/r/a.rs", "1+10", 40),
        ("/r/a.rs", "11+10", 60),
        ("/r/b.rs", "1+5", 30),
    ] {
        telemetry::record_read(&conn, Some("s1"), "text", path, Some(range), tokens, 0).unwrap();
    }
    (dir, path)
}

#[test]
fn reads_text_shows_all_repeat_and_new_range_reads() {
    let (dir, db) = db_with_reads();
    let out = engraph(&db, dir.path(), &["gain", "--reads"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(row(&text, "all")[1..3], ["4", "230"]);
    assert_eq!(row(&text, "repeat")[1..3], ["1", "40"]);
    assert_eq!(row(&text, "new_range")[1..3], ["1", "60"]);
}

#[test]
fn reads_json_has_a_reads_object() {
    let (dir, db) = db_with_reads();
    let out = engraph(&db, dir.path(), &["gain", "--reads", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["reads"]["reads"], 4);
    assert_eq!(v["reads"]["read_tokens"], 230);
    assert_eq!(v["reads"]["repeat_reads"], 1);
    assert_eq!(v["reads"]["repeat_tokens"], 40);
    assert_eq!(v["reads"]["new_range_reads"], 1);
    assert_eq!(v["reads"]["new_range_tokens"], 60);
    // Reads do not change the savings totals.
    assert_eq!(v["summary"]["commands"], 0);
}

#[test]
fn all_includes_reads_and_plain_gain_does_not() {
    let (dir, db) = db_with_reads();
    let all: serde_json::Value =
        serde_json::from_slice(&engraph(&db, dir.path(), &["gain", "--all", "--json"]).stdout)
            .unwrap();
    assert_eq!(all["reads"]["reads"], 4);
    let plain: serde_json::Value =
        serde_json::from_slice(&engraph(&db, dir.path(), &["gain", "--json"]).stdout).unwrap();
    assert!(plain.get("reads").is_none());
}

#[test]
fn reads_csv_has_three_rows() {
    let (dir, db) = db_with_reads();
    let out = engraph(&db, dir.path(), &["gain", "--reads", "--format", "csv"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().any(|l| l.starts_with("reads,all,4,230,")),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("reads,repeat,1,40,")),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("reads,new_range,1,60,")),
        "{text}"
    );
}

#[test]
fn reads_text_without_reads_says_so() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("t.db");
    let out = engraph(&db, dir.path(), &["gain", "--reads"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("(no file reads)"), "{text}");
}

#[test]
fn reads_text_of_only_images_has_no_share() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("t.db");
    let pool = db::open_pool(&db).unwrap();
    let conn = pool.get().unwrap();
    telemetry::record_read(&conn, Some("s1"), "image", "/r/p.png", None, 0, 0).unwrap();
    let out = engraph(&db, dir.path(), &["gain", "--reads"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(row(&text, "all")[1..], ["1", "0", "0.0%"]);
}
