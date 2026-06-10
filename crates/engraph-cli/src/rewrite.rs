use engraph_compress::filters;

/// Decision returned by the pre-bash analysis: silently rewrite the command to
/// route through `engraph run` (allow + updatedInput), or leave it untouched.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RewriteOutcome {
    Rewrite {
        new_command: String,
        filter_id: &'static str,
    },
    Passthrough,
}

pub(crate) fn try_auto_rewrite(command: &str) -> RewriteOutcome {
    let command = command.trim();
    if command.is_empty() {
        return RewriteOutcome::Passthrough;
    }
    // Recursion guard: never wrap something that already routes through engraph.
    if command.starts_with("engraph ") || command.contains(" engraph run ") {
        return RewriteOutcome::Passthrough;
    }
    // Heredocs (`<<EOF`/`<<-EOF`/`<<'EOF'`): rewriting would corrupt the body.
    // Passthrough so the original command runs untouched.
    if has_heredoc(command) {
        return RewriteOutcome::Passthrough;
    }

    // Commands with shell metacharacters: only a pure pipeline whose downstream
    // stages all just *display* bytes can be rewritten (wrap the producer);
    // everything else passes through untouched — see `rewrite_pipeline`.
    if has_unquoted_shell_meta(command) {
        return rewrite_pipeline(command);
    }

    // Plain single command: wrap it if engraph has a filter for it.
    match classify_for_wrap(command) {
        Some((prefix, argv, filter_id)) => RewriteOutcome::Rewrite {
            new_command: build_wrapped(&prefix, &argv),
            filter_id,
        },
        None => RewriteOutcome::Passthrough,
    }
}

/// Parse a single command (one pipeline stage, or a whole non-compound command)
/// and, if engraph has a non-generic filter for it, return the pieces needed to
/// wrap it: the env prefix to re-emit, the argv to pass to `engraph run`, and
/// the filter id. `None` when the segment is empty, parses badly, carries an
/// unsafe prefix (sudo/env/whitespace-value), or has no dedicated filter.
///
/// `argv` keeps git's global options (`-C <path>`, `-c k=v`, …) so the wrapped
/// command runs against the right repo; classification strips them on a copy so
/// the subcommand is visible to `filters::pick`.
fn classify_for_wrap(segment: &str) -> Option<(Vec<String>, Vec<String>, &'static str)> {
    let mut argv = match shell_words::split(segment) {
        Ok(v) if !v.is_empty() => v,
        _ => return None,
    };
    let prefix = strip_command_prefix(&mut argv)?;
    if argv.is_empty() {
        return None;
    }
    normalize_argv0(&mut argv);
    let mut classify = argv.clone();
    strip_git_global_opts(&mut classify);
    if classify.is_empty() {
        return None;
    }
    let cmd_word = classify[0].as_str();
    let arg_word = classify.get(1).map(String::as_str).unwrap_or("");
    let (_filter_fn, filter_id) = filters::pick(cmd_word, &[arg_word.to_string()]);
    if filter_id == "generic" {
        return None;
    }
    Some((prefix, argv, filter_id))
}

/// Assemble `[env-prefix] engraph run <argv…>`. `shell_words::quote` preserves
/// whitespace/special-char args; the env prefix is emitted verbatim (quoting a
/// `KEY=value` token would turn it into a literal command name — and
/// `strip_command_prefix` already validated the prefix tokens are shape-safe).
fn build_wrapped(prefix: &[String], argv: &[String]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(prefix.len() + argv.len() + 2);
    parts.extend(prefix.iter().cloned());
    parts.push("engraph".to_string());
    parts.push("run".to_string());
    for a in argv {
        parts.push(shell_words::quote(a).into_owned());
    }
    parts.join(" ")
}

/// Rewrite a command containing shell metacharacters. We only touch a *pure
/// pipeline* (`producer | sink | …` — no `;`, `&&`/`||`, `&`, redirects, or
/// command substitution), and only when the producer has a filter and every
/// downstream stage is a display sink (head/tail/less/cat/more/bat). In that
/// case we wrap just the producer and keep the pipe intact, so engraph still
/// compresses the bulky output while the user's window is preserved.
///
/// Anything else passes through untouched: feeding engraph's lossy,
/// sentinel-decorated output into a byte *consumer* (grep/wc/awk/jq/…) would
/// silently change that consumer's result, which is worse than not compressing.
fn rewrite_pipeline(command: &str) -> RewriteOutcome {
    let segments = match split_pipeline(command) {
        Some(s) if s.len() >= 2 => s,
        _ => return RewriteOutcome::Passthrough,
    };
    let (prefix, argv, filter_id) = match classify_for_wrap(segments[0].trim()) {
        Some(x) => x,
        None => return RewriteOutcome::Passthrough,
    };
    if !segments[1..].iter().all(|s| is_display_sink(s)) {
        return RewriteOutcome::Passthrough;
    }
    let mut new_command = build_wrapped(&prefix, &argv);
    for seg in &segments[1..] {
        new_command.push_str(" | ");
        new_command.push_str(seg.trim());
    }
    RewriteOutcome::Rewrite {
        new_command,
        filter_id,
    }
}

/// A pipeline stage that merely displays or pages bytes — safe to feed
/// engraph's filtered output into. Deliberately tight: anything not listed
/// (grep, wc, awk, jq, sort, …) consumes bytes semantically and must see the
/// raw stream, so it disqualifies the rewrite.
fn is_display_sink(segment: &str) -> bool {
    let argv = match shell_words::split(segment) {
        Ok(v) if !v.is_empty() => v,
        _ => return false,
    };
    let mut cmd = argv[0].clone();
    normalize_argv0(std::slice::from_mut(&mut cmd));
    matches!(
        cmd.as_str(),
        "head" | "tail" | "less" | "cat" | "more" | "bat"
    )
}

/// Split a pure pipeline into its verbatim stage substrings on unquoted single
/// `|`. Returns `None` if the command holds any other shell metacharacter
/// (`;`, `&&`/`&`, `||`, redirects, command substitution) or has unbalanced
/// quotes — shapes we can't rewrite safely, so the caller passes them through.
/// Quote/backslash tracking mirrors `has_unquoted_shell_meta`.
fn split_pipeline(command: &str) -> Option<Vec<String>> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            match b {
                // `||` is logical-or, not a pipe — bail out of the rewrite.
                b'|' if bytes.get(i + 1) == Some(&b'|') => return None,
                b'|' => {
                    segments.push(command[start..i].to_string());
                    start = i + 1;
                }
                b';' | b'&' | b'<' | b'>' | b'`' => return None,
                b'$' if bytes.get(i + 1) == Some(&b'(') => return None,
                _ => {}
            }
        }
        i += 1;
    }
    if in_single || in_double {
        return None;
    }
    segments.push(command[start..].to_string());
    Some(segments)
}

/// Detects `<<TAG`/`<<-TAG` (heredoc) outside of single/double quotes.
/// Matches the same quote-tracking logic as `has_unquoted_shell_meta`.
pub(crate) fn has_heredoc(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i + 1 < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double && b == b'<' && bytes[i + 1] == b'<' {
            return true;
        }
        i += 1;
    }
    false
}

/// Peels `sudo`/`env`/`FOO=bar` prefix tokens off argv. Returns the peeled
/// tokens (to be re-emitted ahead of `engraph run`), or `None` if the
/// command shape can't be safely rewritten (sudo, `env`, or an env value
/// containing whitespace that would need fragile re-quoting).
pub(crate) fn strip_command_prefix(argv: &mut Vec<String>) -> Option<Vec<String>> {
    if argv.is_empty() {
        return Some(Vec::new());
    }
    // sudo / env would run engraph in a different environment ($HOME, $USER)
    // and we'd lose the SQLite path. Bail and let the original run.
    if argv[0] == "sudo" || argv[0] == "env" {
        return None;
    }
    let mut prefix = Vec::new();
    while !argv.is_empty() && is_env_assignment(&argv[0]) {
        let tok = &argv[0];
        let eq = tok.find('=').expect("is_env_assignment guarantees '='");
        let value = &tok[eq + 1..];
        // Whitespace in the value means shell_words::split has already merged
        // tokens (`MSG='hello world'` becomes one element `MSG=hello world`).
        // Re-quoting that for the rewritten command is fragile; passthrough.
        if value.chars().any(char::is_whitespace) {
            return None;
        }
        prefix.push(argv.remove(0));
    }
    Some(prefix)
}

/// `/usr/bin/grep` → `grep`. Pure normalization so absolute-path invocations
/// reach the same filter as the bare name.
pub(crate) fn normalize_argv0(argv: &mut [String]) {
    let Some(first) = argv.first_mut() else {
        return;
    };
    if !(first.starts_with('/') || first.starts_with("./")) {
        return;
    }
    if let Some(base) = std::path::Path::new(first.as_str())
        .file_name()
        .and_then(|s| s.to_str())
    {
        *first = base.to_string();
    }
}

/// Drop git's global options (`-C path`, `-c k=v`, `--git-dir=...`,
/// `--work-tree=...`) so `git -C /tmp status` classifies as `git status`.
/// Stops at the first non-flag token (the subcommand).
fn strip_git_global_opts(argv: &mut Vec<String>) {
    if argv.first().map(String::as_str) != Some("git") {
        return;
    }
    // The global-option set itself lives in engraph-compress so this rewrite
    // path and `filters::pick` agree on where the subcommand starts.
    let n = filters::git_global_opt_len(&argv[1..]);
    argv.drain(1..1 + n);
}

fn is_env_assignment(tok: &str) -> bool {
    // Identifier=anything → env-var prefix. Match shell rule for variable names.
    let mut chars = tok.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    let mut saw_eq = false;
    for c in chars {
        if c == '=' {
            saw_eq = true;
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    saw_eq
}

/// Scan for shell operators outside of single/double quotes. Tracks backslash
/// escapes and `$(...)` / backtick command substitutions. False positives are
/// fine — they fall back to deny+suggest, which is safe. False negatives are
/// the concern; we err conservative.
pub(crate) fn has_unquoted_shell_meta(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            match b {
                b'|' | b';' | b'&' | b'<' | b'>' | b'`' => return true,
                b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'(' => return true,
                _ => {}
            }
        }
        i += 1;
    }
    false
}
