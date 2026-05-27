//! Read-bucket filter tests: comment stripping by extension, head/tail
//! windowing for whole-file reads, no re-windowing for user-windowed reads,
//! empty-filter fallback when language strip removes everything.

use engraph_compress::filters::{self, FilterCtx};

fn ctx<'a>(cmd: &'a str, args: &'a [String], stdout: &'a str) -> FilterCtx<'a> {
    FilterCtx {
        cmd,
        args,
        stdout,
        stderr: "",
        exit_code: 0,
    }
}

fn run(cmd: &str, args: &[String], stdout: &str) -> (String, &'static str) {
    let (filter, id) = filters::pick(cmd, args);
    let out = filter(&ctx(cmd, args, stdout));
    assert_eq!(out.filter_id, id, "filter_id round-trip");
    (out.text, id)
}

#[test]
fn cat_strips_python_line_comments() {
    let content = "# coding: utf-8\nimport os\n# this is a comment\ndef main():\n    pass\n";
    let args = vec!["foo.py".to_string()];
    let (text, id) = run("cat", &args, content);
    assert_eq!(id, "read_cat");
    assert!(
        !text.contains("# coding"),
        "shebang/coding not stripped: {text}"
    );
    assert!(
        !text.contains("# this is a comment"),
        "comment not stripped: {text}"
    );
    assert!(text.contains("import os"));
    assert!(text.contains("def main():"));
}

#[test]
fn cat_strips_rust_line_comments() {
    let content = "// crate doc\nuse std::io;\n/// docs for foo\nfn foo() {}\n";
    let args = vec!["src/main.rs".to_string()];
    let (text, _) = run("cat", &args, content);
    assert!(!text.contains("crate doc"));
    assert!(!text.contains("docs for foo"));
    assert!(text.contains("use std::io"));
    assert!(text.contains("fn foo()"));
}

#[test]
fn cat_strips_js_ts_line_comments() {
    let content = "// header\nexport const x = 1;\n// inline note\nexport const y = 2;\n";
    let args = vec!["lib.ts".to_string()];
    let (text, _) = run("cat", &args, content);
    assert!(!text.contains("header"));
    assert!(!text.contains("inline note"));
    assert!(text.contains("export const x"));
    assert!(text.contains("export const y"));
}

#[test]
fn cat_strips_go_line_comments() {
    let content = "// pkg doc\npackage main\n// blah\nfunc main() {}\n";
    let args = vec!["main.go".to_string()];
    let (text, _) = run("cat", &args, content);
    assert!(!text.contains("pkg doc"));
    assert!(!text.contains("blah"));
    assert!(text.contains("package main"));
}

#[test]
fn cat_collapses_blank_lines() {
    let content = "a\n\n\n\nb\n";
    let args = vec!["foo.py".to_string()];
    let (text, _) = run("cat", &args, content);
    // At most one consecutive blank line.
    assert!(
        !text.contains("\n\n\n"),
        "multi-blank not collapsed: {text:?}"
    );
}

#[test]
fn cat_windows_large_files_keeping_head_and_tail() {
    let lines: Vec<String> = (0..1000).map(|i| format!("line {i}")).collect();
    let content = lines.join("\n");
    let args = vec!["foo.txt".to_string()];
    let (text, _) = run("cat", &args, &content);
    assert!(text.contains("line 0"));
    assert!(text.contains("line 999"));
    assert!(text.contains("[engraph: omitted"));
    // Middle should be elided.
    assert!(!text.contains("line 500"));
}

#[test]
fn cat_empty_filter_fallback_returns_raw() {
    // All lines are Python comments; language strip empties the file.
    let content = "# comment 1\n# comment 2\n# comment 3\n";
    let args = vec!["all_comments.py".to_string()];
    let (text, _) = run("cat", &args, content);
    assert!(
        text.contains("[engraph: filter emptied"),
        "missing fallback: {text}"
    );
    assert!(text.contains("# comment 1"));
}

#[test]
fn head_does_not_rewindow() {
    // head -n 50 already windowed by user; don't re-window.
    let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
    let content = lines.join("\n");
    let args = vec!["-n".to_string(), "50".to_string(), "foo.txt".to_string()];
    let (text, id) = run("head", &args, &content);
    assert_eq!(id, "read_head_tail");
    assert!(text.contains("line 0"));
    assert!(text.contains("line 49"));
    assert!(
        !text.contains("[engraph: omitted"),
        "head should not re-window"
    );
}

#[test]
fn tail_does_not_rewindow() {
    let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
    let content = lines.join("\n");
    let args = vec!["-n".to_string(), "30".to_string(), "foo.txt".to_string()];
    let (text, _) = run("tail", &args, &content);
    assert!(text.contains("line 0"));
    assert!(text.contains("line 29"));
    assert!(!text.contains("[engraph: omitted"));
}

#[test]
fn unknown_extension_passes_through_unstripped() {
    let content = "# this looks like a comment\nbut this is data\n";
    let args = vec!["data.csv".to_string()];
    let (text, _) = run("cat", &args, content);
    assert!(text.contains("# this looks like a comment"));
    assert!(text.contains("but this is data"));
}

#[test]
fn bat_routes_to_read_cat() {
    let content = "import os\n";
    let args = vec!["foo.py".to_string()];
    let (_text, id) = run("bat", &args, content);
    assert_eq!(id, "read_cat");
}

#[test]
fn less_routes_to_read_cat() {
    let content = "use std::io;\n";
    let args = vec!["src/lib.rs".to_string()];
    let (_text, id) = run("less", &args, content);
    assert_eq!(id, "read_cat");
}
