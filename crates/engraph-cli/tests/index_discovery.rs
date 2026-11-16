//! Workspace discovery flags of `engraph index`, checked through `--dry-run`,
//! which discovers the projects without running an indexer.

use std::path::Path;
use tempfile::tempdir;

mod common;
use common::engraph;

fn manifest(root: &Path, rel: &str, name: &str) {
    let dir = root.join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), "").unwrap();
}

#[test]
fn max_depth_limits_recursive_discovery() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a/b", "Cargo.toml"); // depth 2
    manifest(&ws, "a/b/c", "go.mod"); // depth 3
    let ws_arg = ws.to_str().unwrap();

    let out = engraph(
        &db,
        dir.path(),
        &[
            "index",
            "--recursive",
            "--max-depth",
            "2",
            "--dry-run",
            ws_arg,
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.contains("discovered 1 repo(s)"), "{stdout}");
    assert!(
        stdout.contains(&ws.join("a/b").display().to_string()),
        "{stdout}"
    );

    // The default depth (16) reaches both.
    let out = engraph(
        &db,
        dir.path(),
        &["index", "--recursive", "--dry-run", ws_arg],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.contains("discovered 2 repo(s)"), "{stdout}");
}

#[test]
fn max_depth_requires_recursive() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a", "Cargo.toml");
    let ws_arg = ws.to_str().unwrap();

    let out = engraph(
        &db,
        dir.path(),
        &[
            "index",
            "--workspace",
            ws_arg,
            "--max-depth",
            "2",
            "--dry-run",
        ],
    );
    assert!(
        !out.status.success(),
        "accepted --max-depth without --recursive"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--recursive"), "{stderr}");
}

/// The discovered roots of a dry run, relative to `ws` (`.` for `ws` itself).
fn discovered(db: &Path, ws: &Path, args: &[&str]) -> Vec<String> {
    let mut all = vec!["index", "--dry-run"];
    all.extend_from_slice(args);
    let out = engraph(db, ws, &all);
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("  - "))
        .map(|p| match Path::new(p).strip_prefix(ws) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => panic!("{p} is not under {}", ws.display()),
        })
        .collect()
}

#[test]
fn include_keeps_only_matching_projects() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "", "pom.xml");
    manifest(&ws, "a", "Cargo.toml");
    manifest(&ws, "svc/x", "go.mod");
    manifest(&ws, "svc/x/deep", "pyproject.toml");
    let w = ws.to_str().unwrap();

    // `*` stays at one level, and a glob doesn't cover what's below a match.
    assert_eq!(
        discovered(&db, &ws, &["--recursive", w, "--include", "svc/*"]),
        ["svc/x"]
    );
    // `**` crosses levels, but doesn't match the directory it's under.
    assert_eq!(
        discovered(&db, &ws, &["--recursive", w, "--include", "svc/**"]),
        ["svc/x", "svc/x/deep"]
    );
    // `x/**` leaves out `x` itself; name both to get the directory and below.
    assert_eq!(
        discovered(&db, &ws, &["--recursive", w, "--include", "svc/x/**"]),
        ["svc/x/deep"]
    );
    assert_eq!(
        discovered(
            &db,
            &ws,
            &[
                "--recursive",
                w,
                "--include",
                "svc/x",
                "--include",
                "svc/x/**"
            ]
        ),
        ["svc/x", "svc/x/deep"]
    );
    // Repeated globs add up; `.` is the root, and a leading `./` is accepted.
    assert_eq!(
        discovered(
            &db,
            &ws,
            &["--recursive", w, "--include", ".", "--include", "./a"]
        ),
        [".", "a"]
    );
}

#[test]
fn include_filters_workspace_children() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a", "Cargo.toml");
    manifest(&ws, "b", "Cargo.toml");
    let w = ws.to_str().unwrap();
    assert_eq!(discovered(&db, &ws, &["--workspace", w]), ["a", "b"]);
    assert_eq!(
        discovered(&db, &ws, &["--workspace", w, "--include", "b"]),
        ["b"]
    );
}

#[test]
fn include_needs_workspace_discovery() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "", "Cargo.toml");
    let out = engraph(
        &db,
        dir.path(),
        &["index", "--dry-run", "--include", "a", ws.to_str().unwrap()],
    );
    assert!(
        !out.status.success(),
        "accepted --include without discovery"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--workspace") && stderr.contains("--recursive"),
        "{stderr}"
    );
}

#[test]
fn include_rejects_a_bad_glob() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a", "Cargo.toml");
    let out = engraph(
        &db,
        dir.path(),
        &[
            "index",
            "--dry-run",
            "--recursive",
            ws.to_str().unwrap(),
            "--include",
            "a[",
        ],
    );
    assert!(!out.status.success(), "accepted a bad glob");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("a["), "{stderr}");
}

#[test]
fn filtering_out_every_project_fails_the_run() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a", "Cargo.toml");
    for filter in [["--include", "nothing/*"], ["--exclude", "a"]] {
        let mut args = vec!["index", "--recursive", ws.to_str().unwrap()];
        args.extend_from_slice(&filter);
        let out = engraph(&db, dir.path(), &args);
        assert!(!out.status.success(), "{filter:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("passes the --include and --exclude globs"),
            "{filter:?}: {stderr}"
        );
    }
}

#[test]
fn exclude_drops_matching_projects() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "", "pom.xml");
    manifest(&ws, "a", "Cargo.toml");
    manifest(&ws, "svc/x", "go.mod");
    manifest(&ws, "svc/x/deep", "pyproject.toml");
    let w = ws.to_str().unwrap();

    assert_eq!(
        discovered(&db, &ws, &["--recursive", w, "--exclude", "svc/**"]),
        [".", "a"]
    );
    // Same glob rules as --include: `x/**` leaves `x` itself in.
    assert_eq!(
        discovered(&db, &ws, &["--recursive", w, "--exclude", "svc/x/**"]),
        [".", "a", "svc/x"]
    );
    assert_eq!(
        discovered(
            &db,
            &ws,
            &["--recursive", w, "--exclude", ".", "--exclude", "./a"]
        ),
        ["svc/x", "svc/x/deep"]
    );
}

#[test]
fn exclude_filters_workspace_children() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a", "Cargo.toml");
    manifest(&ws, "b", "Cargo.toml");
    let w = ws.to_str().unwrap();
    assert_eq!(
        discovered(&db, &ws, &["--workspace", w, "--exclude", "a"]),
        ["b"]
    );
}

#[test]
fn exclude_wins_over_include_with_a_warning() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "svc/x", "go.mod");
    manifest(&ws, "svc/x/deep", "pyproject.toml");
    let w = ws.to_str().unwrap();
    let args = [
        "index",
        "--dry-run",
        "--recursive",
        w,
        "--include",
        "svc/**",
        "--exclude",
        "svc/x/deep",
    ];
    assert_eq!(discovered(&db, &ws, &args[2..]), ["svc/x"]);
    let stderr = String::from_utf8_lossy(&engraph(&db, dir.path(), &args).stderr).into_owned();
    assert!(
        stderr.contains("svc/x/deep") && stderr.contains("--exclude"),
        "{stderr}"
    );
    // Excluding a project no --include glob matched is not worth a warning.
    let out = engraph(
        &db,
        dir.path(),
        &["index", "--dry-run", "--recursive", w, "--exclude", "svc/x"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("svc/x"), "{stderr}");
}

#[test]
fn exclude_needs_workspace_discovery_and_a_valid_glob() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a", "Cargo.toml");
    let w = ws.to_str().unwrap();

    let out = engraph(
        &db,
        dir.path(),
        &["index", "--dry-run", "--exclude", "a", w],
    );
    assert!(
        !out.status.success(),
        "accepted --exclude without discovery"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--workspace") && stderr.contains("--recursive"),
        "{stderr}"
    );

    let out = engraph(
        &db,
        dir.path(),
        &["index", "--dry-run", "--recursive", w, "--exclude", "a["],
    );
    assert!(!out.status.success(), "accepted a bad glob");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--exclude") && stderr.contains("a["),
        "{stderr}"
    );
}

#[test]
fn dry_run_lists_each_examined_dir_with_its_cause() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "", "Cargo.toml");
    std::fs::write(ws.join("Cargo.toml"), "[workspace]\n").unwrap();
    manifest(&ws, "crates/b", "Cargo.toml");
    manifest(&ws, "svc", "go.mod");
    manifest(&ws, "deep/er/still", "go.mod");
    let out = engraph(
        &db,
        dir.path(),
        &[
            "index",
            "--dry-run",
            "--recursive",
            ws.to_str().unwrap(),
            "--max-depth",
            "2",
            "--exclude",
            "svc",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let examined: Vec<Vec<&str>> = stdout
        .lines()
        .skip_while(|l| *l != "examined:")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .map(|l| {
            l.split("  ")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .collect();
    assert_eq!(
        examined,
        [
            vec![".", "project (rust-analyzer)"],
            vec!["crates/b", "member of . (rust-analyzer)"],
            vec!["deep/er/still", "beyond --max-depth 2"],
            vec!["svc", "dropped by --exclude"],
        ],
        "{stdout}"
    );
    assert!(stdout.contains("discovered 1 repo(s)"), "{stdout}");
}
