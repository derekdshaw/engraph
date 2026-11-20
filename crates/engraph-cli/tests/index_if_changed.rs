//! `engraph index --if-changed` re-indexes only the recorded subtrees whose
//! files changed since their index commit, and the SessionStart hook starts it
//! detached — but only for a repo that already has `index_state` rows.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

mod common;
use common::{bin, engraph, git};

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A committed monorepo with cargo crates `svc-a` and `svc-b`, plus an empty
/// prebuilt SCIP for driver-free indexing.
fn mono() -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let mono = dir.path().join("mono");
    for (svc, f) in [("svc-a", "alpha_handler"), ("svc-b", "beta_handler")] {
        std::fs::create_dir_all(mono.join(svc).join("src")).unwrap();
        std::fs::write(
            mono.join(svc).join("Cargo.toml"),
            format!("[package]\nname = \"{svc}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(
            mono.join(svc).join("src/lib.rs"),
            format!("pub fn {f}() -> u32 {{ 1 }}\n"),
        )
        .unwrap();
    }
    std::fs::write(mono.join(".gitignore"), "target/\n").unwrap();
    git(&mono, &["init", "-q"]);
    git(&mono, &["add", "."]);
    git(&mono, &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.path().join("empty.scip"), b"").unwrap();
    let mono = mono.canonicalize().unwrap();
    (dir, mono)
}

fn key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `(subtree, commit_sha)` rows of `index_state`, by subtree.
fn index_state(db: &Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT subtree, commit_sha FROM index_state ORDER BY subtree")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn entity_names(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM entities WHERE kind = 'symbol' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// `(filter_id, input_tokens)` of every `index` event, oldest first.
fn index_events(db: &Path) -> Vec<(String, i64)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT filter_id, input_tokens FROM events \
             WHERE kind = 'index' AND feature = 'codegraph_index' ORDER BY seq",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Rewrite how every recorded subtree was indexed (`index_state.source` and
/// `.lang`), to reach refresh paths without running a real indexer.
fn set_mode(db: &Path, source: &str, lang: Option<&str>) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "UPDATE index_state SET source = ?1, lang = ?2",
        rusqlite::params![source, lang],
    )
    .unwrap();
}

fn rust_analyzer_present() -> bool {
    let probe = Command::new("rust-analyzer").arg("--version").output();
    let ok = probe.map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        eprintln!("skip: rust-analyzer not installed");
    }
    ok
}

fn session_start(db: &Path, cwd: &Path, envs: &[(&str, &str)]) -> String {
    let payload = serde_json::json!({ "cwd": cwd, "session_id": "s1" }).to_string();
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db)
        .envs(envs.iter().copied())
        .current_dir(cwd)
        .args(["hook", "session-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    stdout(&out)
}

/// Runs recorded in `auto-index.log` text that have finished: a `=== ` header
/// followed by an outcome line (the header is written by the hook before the
/// child starts, so a header alone means a run in flight).
fn completed_runs(log: &str) -> usize {
    log.split("\n=== ")
        .filter(|seg| {
            [
                "indexed ",
                "err ",
                "up to date",
                "skipped:",
                "no index recorded",
            ]
            .iter()
            .any(|m| seg.contains(m))
        })
        .count()
}

/// The detached refresh writes `auto-index.log` beside the DB; wait until it
/// records `n` finished runs.
fn wait_for_runs(db: &Path, n: usize, timeout: Duration) -> Option<String> {
    let log = db.parent().unwrap().join("auto-index.log");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(&log)
            && completed_runs(&s) >= n
        {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn if_changed_without_a_recorded_index_does_nothing() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("no index recorded"),
        "{}",
        stdout(&out)
    );
    assert!(index_state(&db).is_empty());
    assert!(entity_names(&db).is_empty());
}

#[test]
fn if_changed_rejects_flags_that_pin_a_source() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    for extra in [
        &["--scip", "x.scip"][..],
        &["--lang", "rust-analyzer"],
        &["--workspace", "."],
        &["--recursive"],
        &["--project", "p"],
    ] {
        let mut args = vec!["index", "--if-changed", "."];
        args.extend_from_slice(extra);
        let out = engraph(&db, &mono, &args);
        assert!(!out.status.success(), "accepted {extra:?}");
    }
}

#[test]
fn if_changed_reindexes_only_the_changed_subtrees() {
    if !rust_analyzer_present() {
        return;
    }
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    for sub in ["svc-a", "svc-b"] {
        let out = engraph(&db, &mono, &["index", sub]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let before = index_state(&db);
    assert_eq!(before.len(), 2);

    // Nothing changed: no subtree is touched.
    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("up to date"), "{}", stdout(&out));
    assert_eq!(index_state(&db), before);

    // An uncommitted edit in svc-b: dry-run names it alone, the real run
    // re-indexes it alone and re-records it at the new state.
    std::fs::write(
        mono.join("svc-b/src/lib.rs"),
        "pub fn beta_handler() -> u32 { 1 }\npub fn beta_extra() {}\n",
    )
    .unwrap();
    let out = engraph(&db, &mono, &["index", "--if-changed", "--dry-run", "."]);
    assert!(out.status.success());
    let s = stdout(&out);
    assert!(s.contains("would re-index") && s.contains("svc-b"), "{s}");
    assert!(!s.contains("svc-a"), "{s}");
    assert_eq!(index_state(&db), before, "dry-run must not record");

    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = stdout(&out);
    assert!(s.contains("indexed") && s.contains("svc-b"), "{s}");
    assert!(!s.contains("svc-a"), "{s}");
    assert!(entity_names(&db).iter().any(|n| n == "beta_extra"));
    let after = index_state(&db);
    assert_eq!(after[0], before[0], "svc-a keeps its commit");
    assert_eq!(after[1].0, "svc-b");
    assert_ne!(after[1].1, before[1].1, "svc-b re-recorded");

    // Now recorded at the edited state: a second run is a no-op.
    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(stdout(&out).contains("up to date"), "{}", stdout(&out));
    assert_eq!(index_state(&db), after);

    // Telemetry: the two manual indexes are per-driver rows; each
    // `--if-changed` run is one row (not one per subtree) carrying the SCIP
    // bytes it produced — none for the no-op runs — and the dry run none.
    let events = index_events(&db);
    assert_eq!(events.len(), 5, "{events:?}");
    assert_eq!(events[0].0, "rust-analyzer");
    assert_eq!(events[1].0, "rust-analyzer");
    assert_eq!(events[2], ("if_changed".into(), 0));
    assert_eq!(events[3].0, "if_changed");
    assert!(events[3].1 > 0, "{events:?}");
    assert_eq!(events[4], ("if_changed".into(), 0));
}

#[test]
fn if_changed_reindexes_on_a_new_untracked_file() {
    if !rust_analyzer_present() {
        return;
    }
    let (dir, mono) = mono();
    let db = dir.path().join("u.db");
    let out = engraph(&db, &mono, &["index", "svc-b"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Back-date the record so the new file's mtime is later than `indexed_at`
    // whatever the clock granularity.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE index_state SET indexed_at = datetime('now', '-10 seconds')",
        [],
    )
    .unwrap();
    drop(conn);

    // A new untracked file: `git diff` cannot see it, `--if-changed` must.
    // `src/bin/` so cargo auto-discovers it and rust-analyzer indexes it.
    std::fs::create_dir_all(mono.join("svc-b/src/bin")).unwrap();
    std::fs::write(
        mono.join("svc-b/src/bin/extra.rs"),
        "pub fn beta_untracked() {}\nfn main() {}\n",
    )
    .unwrap();

    let out = engraph(&db, &mono, &["index", "--if-changed", "--dry-run", "."]);
    let s = stdout(&out);
    assert!(s.contains("would re-index") && s.contains("svc-b"), "{s}");

    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = stdout(&out);
    assert!(s.contains("indexed") && s.contains("svc-b"), "{s}");
    assert!(
        entity_names(&db).iter().any(|n| n == "beta_untracked"),
        "untracked file's symbol missing from the graph"
    );

    // The same untracked file does not start a second refresh.
    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(stdout(&out).contains("up to date"), "{}", stdout(&out));
}

#[test]
fn auto_runs_record_one_event_with_filter_auto() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    assert_eq!(index_events(&db).len(), 1);

    // `--auto` is only meaningful for a refresh.
    let out = engraph(&db, &mono, &["index", "--auto", "."]);
    assert!(!out.status.success());

    // An automatic run with nothing to do still leaves its row, so the hook's
    // firing is visible; a dry run leaves none.
    let out = engraph(&db, &mono, &["index", "--if-changed", "--auto", "."]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("up to date"));
    let out = engraph(
        &db,
        &mono,
        &["index", "--if-changed", "--auto", "--dry-run", "."],
    );
    assert!(out.status.success());
    let events = index_events(&db);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1], ("auto".into(), 0));
}

#[test]
fn session_start_refreshes_an_indexed_repo_in_the_background() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    // Indexed driver-free from an empty SCIP, but record it as a driver run:
    // that's what a refresh repeats (a `--scip` subtree it skips).
    set_mode(&db, "drivers", None);
    std::fs::write(mono.join("svc-a/src/lib.rs"), "pub fn alpha_handler() {}\n").unwrap();

    // The hook returns the brief without waiting on the refresh, which then
    // shows up in the log (as an outcome for this project, whatever it is —
    // the driver may be absent here).
    let started = Instant::now();
    let brief = session_start(&db, &mono, &[]);
    assert!(brief.contains("hookSpecificOutput"), "{brief}");
    let log = wait_for_runs(&db, 1, Duration::from_secs(20)).expect("refresh finished and logged");
    assert!(
        log.contains(&key(&mono)) && (log.contains("indexed ") || log.contains("err ")),
        "refresh ran on the changed root: {log}"
    );
    assert!(started.elapsed() < Duration::from_secs(20));

    // The log lives in the data directory (the DB's), and each run opens with
    // a dated header naming the command and the checkout.
    assert!(db.parent().unwrap().join("auto-index.log").exists());
    let header = log.lines().next().unwrap();
    assert!(header.starts_with("=== 20"), "{header}");
    assert!(
        header.contains(&format!("engraph index --if-changed --auto {}", key(&mono))),
        "{header}"
    );
    // A second start appends a second run under its own header.
    session_start(&db, &mono, &[]);
    let log2 = wait_for_runs(&db, 2, Duration::from_secs(20)).expect("second run logged");
    assert_eq!(log2.matches("\n=== 20").count() + 1, 2, "{log2}");
    assert!(log2.starts_with(&log), "log is append-only");

    // Both background runs were recorded with filter `auto`, after the manual
    // index's own row. The child logs its outcome before it writes the row,
    // so give the row a moment.
    let deadline = Instant::now() + Duration::from_secs(10);
    let events = loop {
        let events = index_events(&db);
        if events.iter().filter(|(f, _)| f == "auto").count() == 2 {
            break events;
        }
        assert!(
            Instant::now() < deadline,
            "auto events not recorded: {events:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0].0, "prebuilt", "{events:?}");
}

#[test]
fn session_start_leaves_unindexed_and_opted_out_repos_alone() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let log = dir.path().join("auto-index.log");

    // Never indexed: nothing to refresh, nothing spawned.
    session_start(&db, &mono, &[]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!log.exists(), "spawned a refresh for an unindexed repo");

    // Indexed, but the user opted out.
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    session_start(&db, &mono, &[("ENGRAPH_AUTO_INDEX", "0")]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !log.exists(),
        "spawned a refresh despite ENGRAPH_AUTO_INDEX=0"
    );
}

/// Plan 10, step 7: a worktree that is not the base checkout writes its own
/// overlay, never the base rows the other worktrees read. This project was
/// indexed from prebuilt SCIP, which engraph cannot reproduce, so its overlay
/// is left alone too — but the base is untouched either way.
#[test]
fn a_linked_worktree_writes_an_overlay_not_the_shared_index() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    let before = index_state(&db);

    let wt = dir.path().join("wt");
    git(
        &mono,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
    );
    let wt = wt.canonicalize().unwrap();
    std::fs::write(wt.join("svc-a/src/lib.rs"), "pub fn alpha_handler() {}\n").unwrap();

    // A refresh run by hand there goes to the overlay, and leaves the base
    // rows of every other worktree as they are.
    let out = engraph(&db, &wt, &["index", "--if-changed", "."]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = stdout(&out);
    assert!(s.contains("overlay "), "{s}");
    assert_eq!(index_state(&db), before, "the base graph is untouched");

    // The same for a plain index run there: it names the overlay and points
    // at --full for moving the base.
    let out = engraph(&db, &wt, &["index", "."]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = stdout(&out);
    assert!(
        s.contains("not the base checkout") && s.contains("--full"),
        "{s}"
    );
    assert_eq!(index_state(&db), before, "the base graph is untouched");
}

/// Plan 10, steps 11 and 12: "--if-changed" in a worktree brings the overlay
/// up to date once, and then leaves it alone until something moves.
#[test]
fn if_changed_in_a_worktree_runs_the_overlay_once_until_something_changes() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    let before = index_state(&db);
    let wt = dir.path().join("wt");
    git(
        &mono,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
    );
    let wt = wt.canonicalize().unwrap();

    // A clean worktree: the first run computes the (empty) overlay and
    // records what it was computed from; the second finds it current and
    // indexes nothing.
    let out = engraph(&db, &wt, &["index", "--if-changed", "."]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("overlay "), "{}", stdout(&out));
    let out = engraph(&db, &wt, &["index", "--if-changed", "."]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("overlay up to date"),
        "{}",
        stdout(&out)
    );

    // An edit makes it stale again. This project is prebuilt SCIP, so the
    // run has nothing it can index — but it records the new state, and the
    // check goes quiet again.
    std::fs::write(wt.join("svc-a/src/lib.rs"), "pub fn alpha_handler() {}\n").unwrap();
    let out = engraph(&db, &wt, &["index", "--if-changed", "."]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("nothing engraph can index"),
        "{}",
        stdout(&out)
    );
    let out = engraph(&db, &wt, &["index", "--if-changed", "."]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("overlay up to date"),
        "{}",
        stdout(&out)
    );

    assert_eq!(index_state(&db), before, "the base graph never moved");
}

/// Plan 10, step 12: the SessionStart hook starts the background refresh from
/// a linked worktree too — the child brings that worktree's overlay up to
/// date instead of touching the base.
#[test]
fn session_start_refreshes_a_worktree_overlay_in_the_background() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    let before = index_state(&db);
    let wt = dir.path().join("wt");
    git(
        &mono,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
    );
    let wt = wt.canonicalize().unwrap();

    session_start(&db, &wt, &[]);
    let log = dir.path().join("auto-index.log");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let text = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("overlay ") || std::time::Instant::now() > deadline {
            break text;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        text.contains("overlay "),
        "the hook did not refresh the worktree overlay: {text}"
    );
    assert_eq!(index_state(&db), before, "the base graph never moved");
}

/// Plan 10, step 15: the overlay of a worktree git no longer lists goes with
/// it, and the overlays of the worktrees that remain are untouched.
#[test]
fn a_removed_worktree_loses_its_overlay() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());

    let mut paths = Vec::new();
    for name in ["wt-a", "wt-b"] {
        let wt = dir.path().join(name);
        git(
            &mono,
            &["worktree", "add", "-q", "-b", name, wt.to_str().unwrap()],
        );
        let wt = wt.canonicalize().unwrap();
        // Each worktree records an overlay of its own.
        let out = engraph(&db, &wt, &["index", "--if-changed", "."]);
        assert!(out.status.success(), "{}", stdout(&out));
        paths.push(wt);
    }
    let overlays = |db: &Path| -> Vec<String> {
        let conn = rusqlite::Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare("SELECT worktree FROM overlay_state ORDER BY worktree")
            .unwrap();
        let mut rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows.sort();
        rows
    };
    let mut expected: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    expected.sort();
    assert_eq!(overlays(&db), expected);

    // wt-a goes; the next refresh anywhere in the repo removes its overlay.
    git(&mono, &["worktree", "remove", paths[0].to_str().unwrap()]);
    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("removed the overlay of"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        overlays(&db),
        vec![paths[1].to_string_lossy().into_owned()],
        "the overlay of the worktree that remains was removed too"
    );
}

#[test]
fn a_refresh_skips_subtrees_indexed_from_prebuilt_scip() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    let before = index_state(&db);
    std::fs::write(mono.join("svc-a/src/lib.rs"), "pub fn alpha_handler() {}\n").unwrap();

    for extra in [&["--dry-run"][..], &[]] {
        let mut args = vec!["index", "--if-changed", "."];
        args.extend_from_slice(extra);
        let out = engraph(&db, &mono, &args);
        assert!(out.status.success(), "{extra:?}");
        let s = stdout(&out);
        assert!(s.contains("skip ") && s.contains("(--scip)"), "{s}");
        let reindexed = s.lines().any(|l| l.starts_with("indexed "));
        assert!(!reindexed && !s.contains("would re-index"), "{s}");
    }
    assert_eq!(
        index_state(&db),
        before,
        "a skipped subtree keeps its commit"
    );
}

#[test]
fn a_refresh_repeats_the_recorded_lang_pin() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    set_mode(&db, "drivers", Some("rust-analyzer"));
    std::fs::write(mono.join("svc-a/src/lib.rs"), "pub fn alpha_handler() {}\n").unwrap();

    let out = engraph(&db, &mono, &["index", "--if-changed", "--dry-run", "."]);
    assert!(out.status.success());
    let s = stdout(&out);
    assert!(
        s.contains("would re-index") && s.contains("(--lang rust-analyzer)"),
        "{s}"
    );
}

#[test]
fn a_refresh_does_not_reindex_a_parent_over_a_prebuilt_subproject() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let scip = dir.path().join("empty.scip");
    let scip = scip.to_str().unwrap();
    // The whole repo by drivers (emulated), then svc-a from prebuilt SCIP.
    let out = engraph(&db, &mono, &["index", ".", "--scip", scip]);
    assert!(out.status.success());
    set_mode(&db, "drivers", None);
    let out = engraph(&db, &mono, &["index", "svc-a", "--scip", scip]);
    assert!(out.status.success());
    let before = index_state(&db);
    assert_eq!(before.len(), 2, "{before:?}");

    // An edit in svc-a changes both rows; only the root is outermost.
    std::fs::write(mono.join("svc-a/src/lib.rs"), "pub fn alpha_handler() {}\n").unwrap();
    for extra in [&["--dry-run"][..], &[]] {
        let mut args = vec!["index", "--if-changed", "."];
        args.extend_from_slice(extra);
        let out = engraph(&db, &mono, &args);
        assert!(out.status.success(), "{extra:?}");
        let s = stdout(&out);
        assert!(
            s.contains("skip ") && s.contains("covers") && s.contains("svc-a"),
            "{s}"
        );
        assert!(!s.contains("would re-index"), "{s}");
        assert!(!s.lines().any(|l| l.starts_with("indexed ")), "{s}");
    }
    assert_eq!(index_state(&db), before, "the sub-project keeps its row");
}
