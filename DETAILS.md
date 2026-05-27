# Engraph — Architecture & Algorithms

This document is the technical companion to `README.md`. Where the README
says *what* you can do, this file says *how* each piece is built and *why*
the design landed where it did. Read alongside the source.

File paths use `crate/path:line` form so you can jump straight to the code.

---

## 1. Guiding principles

These are the constraints every feature is measured against. They explain
why the codebase is shaped the way it is.

- **Local-first.** Storage is one SQLite file. No daemon, no cloud
  dependency, no per-machine schema bootstrap. The full session memory and
  telemetry are inspectable with `sqlite3` from any shell.
- **Deterministic where possible.** The compressor, the per-command
  filters, and the retrieval scoring are all deterministic given the
  same input. This is what lets us write fixed-point ("idempotent") and
  byte-exact golden snapshot tests.
- **Idempotency over integrity.** Compressing already-compressed text is a
  no-op (sentinel fast-path), not an error. The system is built to be
  called twice; double-application never corrupts state.
- **No bypass paths.** When a wrapped command exists, the PreToolUse hook
  rewrites Claude's raw command to route through `engraph run`. Claude
  can't accidentally "forget" to use the wrapper — the hook substitutes
  silently. The escape hatch (`engraph run` already in the command, env
  prefix, compound shell) is detected explicitly and falls back cleanly.
- **Telemetry pays for itself.** Every compression, retrieval, and wrapped
  command writes a row to `events` with input/output tokens. `engraph
  gain` reports the running savings; nothing is asked to be trusted on
  faith.
- **Cross-platform Rust.** Unix-only APIs (file inode, POSIX signals) are
  `#[cfg(unix)]` gated. Everything else is portable; CI runs on Linux,
  macOS, and Windows.

---

## 2. Architecture

### 2.1 Crate layout

```
crates/
├── engraph-core/          schema, db pool, telemetry, budget, tokens, embedding trait
├── engraph-compress/      F6 compressor + per-command filters (git, cargo, npm, …)
├── engraph-retrieve/      FTS+scoping+KG, hybrid (feature-gated)
├── engraph-ingest/        JSONL → SQLite, rotation guard, compress-existing sweep
└── engraph-cli/           the `engraph` binary (subcommands + hooks)
```

The dependency graph is strictly downward: `engraph-cli` consumes everything,
`engraph-ingest` and `engraph-retrieve` consume `engraph-core` and
`engraph-compress`. There are no cycles. `engraph-core` has no
dependencies on the other engraph crates — it's where shared types
(database pool, telemetry, error type, token counter, embedding trait) live.

### 2.2 Runtime data flow

```
                ┌─────────────────────────────┐
                │  Claude Code session         │
                └─────┬───────────────────────┘
                      │ stdin JSON
   SessionStart hook ─┤                     ┌──── stdout JSON
                      ▼                     │     (additionalContext)
              ┌─────────────────┐           │
              │ engraph hook    ├───────────┘
              │   session-start │
              └─────────────────┘
                      │
                      │ reads context_items, bugs, do_not_repeat,
                      │ session_budget; emits ≤ 2KB markdown brief
                      ▼
                ┌─────────────────────────────┐
                │ ~/.local/share/engraph/     │
                │   engraph.db (SQLite WAL)   │
                └─────┬─────────────┬─────────┘
                      ▲             ▲
                      │             │
   PreToolUse(Bash) ──┤             │── F6 sweep, recall, telemetry
                      │             │
                      ▼             │
              ┌─────────────────┐   │
              │ engraph hook    │   │
              │   pre-bash      │   │
              └─────┬───────────┘   │
                    │ stdout JSON   │
                    │ (allow/deny/  │
                    │  updatedInput)│
                    ▼               │
              ┌─────────────────┐   │
              │ Claude Code     │   │
              │ runs wrapped:   │   │
              │ `engraph run …` │───┘
              └─────────────────┘
                    │
                    │ child process output → per-command filter → compressed text
                    ▼
                stdout to Claude
```

The arrows make explicit the two transactional surfaces:

1. **PreToolUse(Bash)**: Claude proposes a command; engraph rewrites it
   (or denies with a suggestion). Output going back into Claude's
   transcript is compressed.
2. **SessionStart**: Claude opens a session; engraph injects a brief.

Everything else (ingest, compress-existing sweep, recall) is initiated
manually or via additional hooks the user wires up.

### 2.3 Storage: SQLite WAL, schema versioning

One database, opened by `db::open_pool` (`engraph-core/src/db.rs`).

- **WAL mode** set once per DB; multiple readers + one writer without
  blocking. `synchronous = NORMAL` is the WAL-appropriate durability /
  speed tradeoff.
- **r2d2 connection pool** (`max_size = 4`). Per-call `busy_timeout = 5s`
  absorbs incidental contention.
- **Schema migrations** are a `&[&str]` array; each entry is one SQL batch
  applied inside its own transaction and recorded in `migrations`
  (`engraph-core/src/schema.rs`). `SCHEMA_VERSION` is a compile-time
  constant; `check_drift` refuses to run if the on-disk schema is *newer*
  than the binary expects.
- The path defaults to `dirs::data_local_dir()/engraph/engraph.db`,
  overridable with `ENGRAPH_DB_PATH`.

#### Why one file, not many

Two reasons. First, FTS5 + a knowledge graph + telemetry + session memory
all want the same connection so they can be queried as one transaction.
Second, distribution is brutal: shipping one SQLite file makes the
backup-and-restore story trivial.

#### Schema highlights

| Table | Role |
|---|---|
| `messages`, `sessions` | The JSONL ingest output. Messages have `content_compressed` + `content_hash` so a compressed message stays auditable. |
| `messages_fts`, `context_items_fts` | FTS5 external-content indexes. `INSERT`/`DELETE` triggers keep them aligned; `UPDATE` triggers were intentionally dropped in v5 so `compress-existing` doesn't overwrite the index with compressed text (see §6.4). |
| `embeddings` | `(target_kind, target_id, model_id)` PK so vectors under different model versions coexist; cosine search ignores wrong-model rows. |
| `events`, `session_budget` | The telemetry + budget surface. |
| `entities`, `relations` | Knowledge-graph tables with `provenance ∈ {extracted, inferred, ambiguous, generated}`. F2 codegraph populates these from SCIP per-language indexers; `entities` carries `file_path`, `line_range`, `signature` (v6). |
| `scopes`, `scope_members` | Mempalace-style hierarchical scoping (project / topic / time-window / custom). Used by recall to restrict results to a `cwd` or named scope. |
| `ingestion_log` | One row per JSONL path with `(last_offset, last_inode, last_size, last_mtime)` so we can detect rotation/truncation (§5.1). |

---

## 3. The compression pipeline (F6)

Entry: `compress(CompressInput)` → `CompressResult`
(`engraph-compress/src/lib.rs:77`).

Six steps, in strict order. Each step is independently testable.

### 3.1 Step 1 — sentinel fast-path

```
text.starts_with("<<engraph:v1:compressed>>") ? return as-is
```

This is the idempotency guarantee (`sentinel.rs:4`). Compressing a
compressed string is the literal-bytes identity. The cost: a 25-byte
string compare. The benefit: callers never need to track whether a row
has been compressed before, and accidental double-compression in a sweep
is a no-op rather than a destructive re-paraphrase.

We do *not* validate integrity in the fast-path: anything that happens
to start with the sentinel round-trips. Integrity belongs in
`content_hash` adjacent to the stored text (see schema), where the caller
can verify against the original-bytes SHA-256 if they care.

### 3.2 Step 2 — whitespace normalization

`normalize_whitespace` (`lib.rs:141`): collapse runs of spaces/tabs to one
space per line, trim trailing whitespace, drop trailing empty lines.
Paragraph breaks (single `\n\n`) are preserved.

This is a determinism gate. Without it, two inputs that differ only in
trailing whitespace would produce different sentinel'd output, breaking
re-ingest idempotency on transcripts written by editors with different
newline conventions.

### 3.3 Step 3 — per-kind preprocessing

`preprocess::apply` (`preprocess.rs:5`) dispatches on `CompressKind`:

| Kind | Preprocessing |
|---|---|
| `ToolOutput` | strip ANSI escapes → drop progress lines (carriage-return-overwrite + `[#=>\-_.\d%/|]*$`) → dedupe consecutive identical lines (`a\na\na\n` → `a (x3)\n`) |
| `SessionMessage` | strip `{"type":"tool_use"/...}` envelope lines → truncate long base64/hex blobs to `head…[NB]…tail` |
| `ProjectNotes` | strip HTML comments → collapse blank-line runs |
| `Generic` | no-op |

Each is an additive filter chain over `&str`. Per-kind separation means
the ranking step (next) operates on already-denoised content; the same
ranking math behaves well across very different content classes.

#### Why dedupe consecutive instead of global

Two reasons. First, "global dedupe" loses structural meaning: a tool-output
log that repeats `OK` 50 times across different stages is meaningfully
different from one that repeats `OK` 50 times in a row. Second, consecutive
dedupe is O(n) without auxiliary state. The `(x3)` annotation preserves
count for diagnosability.

### 3.4 Step 4 — extractive sentence ranking

`rank::extract` (`rank.rs:10`).

The math:

```
for each non-stopword term t in document:
    freq[t] = count of t across whole doc

for each sentence s:
    ws = non-stopword tokens in s
    if ws is empty: drop sentence entirely
    score(s) = sum_{t in ws}(freq[t]) / sqrt(|ws|)

sort sentences by score desc, original-index asc for ties
greedy-fill until cumulative tokens ≥ target_tokens
emit kept sentences in original document order
```

Why this exact shape:

- **Sum of term frequencies, not TF-IDF.** TF-IDF needs a corpus; we
  have one document at compression time. Plain TF gives a robust
  document-internal salience signal: if a term keeps reappearing, it's
  probably what the document is about.
- **Divide by `sqrt(|ws|)`.** Plain sum-of-frequencies massively favors
  long sentences (more terms, more frequency mass). `/ sqrt(|ws|)` is
  the BM25-flavored length normalization that prevents that, without
  overcorrecting into a length-anti-preference like `/|ws|` would.
- **Drop stopword-only sentences instead of zero-scoring them.** A
  zero-scoring sentence ties against any other low-score sentence in
  the original-index tiebreaker, and could be picked when it carries
  zero information. Dropping them is the simpler, correcter behavior.
- **Tie-break on original index ascending.** Determinism plus: when two
  sentences score identically, the earlier one wins. Combined with the
  "emit in original document order" final step, this preserves
  narrative flow.
- **Greedy fill with token check.** `target_tokens` is a soft floor —
  the algorithm stops once cumulative ≥ target, with a minimum of 32
  tokens (`lib.rs:117`) so very short inputs aren't compressed below
  intelligibility.

### 3.5 Step 5 — caveman brevity (opt-in)

`brevity::strip_fillers` (`brevity.rs:20`). Removes articles (`a`, `an`,
`the`) and a fixed list of filler words (`just`, `really`, `very`,
`actually`, `basically`, `literally`, `simply`, `quite`, `rather`,
`somewhat`, `perhaps`, `maybe`).

Opt-in per call, never per-kind default. Articles carry meaning in prose
("the foo" vs "a foo" is sometimes the whole point); we only apply this
to inputs where verbatim grammar isn't preserved anyway, like noisy tool
output.

### 3.6 Step 6 — sentinel stamp

`sentinel::stamp` (`sentinel.rs:13`) prepends `<<engraph:v1:compressed>>\n`.
No trailer: an in-band hash trailer would be indistinguishable from
arbitrary content, breaking the "anything starting with sentinel
round-trips" contract. Provenance (`original_hash`, `original_tokens`,
`compressed_tokens`, `algorithm_id`) lives in `CompressResult` and is
persisted alongside the row by the caller.

### 3.7 Idempotency: the fixed-point property

Compose the six steps and you get a function `f` such that `f(f(x)) =
f(x)` for all `x`. The sentinel fast-path makes this trivially true on
the second invocation; the normalization steps before it are what make
the first invocation produce a stable output that the fast-path will
recognize. The unit test `idempotent_on_second_pass`
(`engraph-compress/src/lib.rs:203`) pins this.

This is why `compress-existing` is safe to run repeatedly: every row
either passes the sentinel check (no-op) or gets compressed once and
sentinel'd, after which it joins the no-op group.

---

## 4. Per-command output filters (F1)

`engraph run <cmd> [args...]` spawns the wrapped command, picks a filter
based on `(cmd, first arg)`, runs it on the captured `(stdout, stderr,
exit_code)`, prints the filtered text, and exits with the child's exit
code (`engraph-cli/src/main.rs` `Cmd::Run` branch).

### 4.1 Dispatch

`filters::pick(cmd, args)` (`engraph-compress/src/filters/mod.rs:37`) is a
single big `match` returning `(FilterFn, &'static str)`. The unknown
fallback is `(generic::filter, "generic")`.

Each `FilterFn` has signature
`fn(&FilterCtx) -> FilterOutput` where `FilterCtx` carries the four
fields the filter needs (`cmd`, `args`, `stdout`, `stderr`, `exit_code`).
This keeps filters trivially testable: no DB, no IO, no global state.

The filter ID returned by the picker and the `filter_id` stamped on the
`FilterOutput` must agree — a regression test pins every cargo and git
arm to enforce this (`tests/filter_ratios.rs::picker_and_filter_output_agree_on_filter_id`).
The mismatch we paid for once was `cargo check` (picker said
`cargo_check`, but the underlying `cargo::build` stamped `cargo_build`);
the v2.1 fix introduced a thin `cargo::check` wrapper.

### 4.2 Filter taxonomy

Each filter has the same broad shape:

```
combined = stdout + stderr        (filters that care about both)
out = String::with_capacity(...)
for line in combined.lines():
    if line matches "noise pattern": skip
    else: out.push_str(line); out.push('\n')
out.push_str("[engraph: <counts>, exit <code>]\n")
```

Specific noise patterns by family:

| Family | Drop | Keep | Summarize |
|---|---|---|---|
| cargo (build/check/clippy/doc) | `^\s*(Compiling\|Checking\|Downloading\|Updating\|Fresh\|Finished\|...)\b` | warnings, errors | counts |
| cargo test (libtest) | `^test \S+ \.\.\. (ok\|ignored)\b` | `---- foo stdout ----` headers, failure panics, `test result:` summary | passed/failed/ignored counts |
| cargo test (nextest) | `^PASS \[\s*\d+\.\d+s\]` | `^FAIL \[\s*\d+\.\d+s\]` lines, summary | counts |
| git log | full commit blocks | one line per commit: `<7-char hash> <subject>` | total commit count; `--oneline` passes through |
| git diff | non-stat hunks summarized as `+X -Y :: first changed line` | `diff --git`, `--- ` / `+++ ` headers, `@@` hunks | hunk-level adds/removes |
| tree / fd / ls | depth-truncated; long flat lists capped | first N entries; depth limit | `truncated N more entries` |
| rg / grep | flat match lists capped | first N matches | `truncated N more matches` |
| docker / kubectl | annotations / spec blobs | events, container/pod summaries | row caps |
| npm / pnpm / yarn | per-package "added N" progress | totals, vulnerability count | summary |
| pytest / pip / go test | pass-progress lines | failed test output, summary | counts |
| cat / bat / less (whole-file read) | line comments by extension; runs of blank lines | first 400 lines + last 100 lines if file > 500 lines | `[engraph: omitted N middle lines]` |
| head / tail (user-windowed read) | line comments by extension; runs of blank lines | every line the user asked for | none — no re-windowing on top of the user's `-n` |

### 4.3 Why per-command not generic

A single generic compressor running over `git log` would extract whatever
TF-IDF said was salient and lose the structure ("which commits exist in
what order"). Per-command filters know that `git log` is N commit
blocks and can collapse each to its subject without losing the count.
The ratio gains are dramatic: `git_log_under_quarter`
(`tests/filter_ratios.rs:25`) demonstrates < 0.3 on 50 commits.

The unknown-command fallback (`generic::filter`) routes through
`compress(CompressKind::ToolOutput)`, so users still get *some* savings
even on tools we haven't written a filter for.

### 4.4 Golden snapshot pinning

`tests/golden_fixtures.rs` walks `tests/fixtures/<name>.in.txt` →
`<name>.expected.txt` pairs and asserts byte-exact match after running
the right filter. Three pinned today (`git_log_basic`,
`cargo_check_basic`, `cargo_test_nextest`). Output-format drift in a
filter shows up as a diff in the golden assertion, not as a subtle
behavior change. A negative test ensures missing fixtures panic rather
than silently passing.

### 4.5 The read bucket — file-content filters

`crates/engraph-compress/src/filters/read.rs` covers `cat`, `bat`, and
`less` (whole-file reads) and `head` / `tail` (user-windowed reads).
The two functions share three building blocks:

- **Language-aware comment strip.** `comment_markers_for(ext)` maps
  the file extension (extracted from the last non-flag, non-numeric
  arg) to a slice of line-comment prefixes — `#` for Python, `//` for
  Rust / Go / JS / TS / JSX / TSX. Lines whose `trim_start()` starts
  with a marker are dropped. Block comments (`/* */`, multi-line
  docstrings) are intentionally NOT handled — a regex-only stripper
  can't safely cross newlines, and the savings on file-scoped reads
  don't justify a full lexer per language. Conservative wins.
- **Blank-line collapse.** Within a single pass over the surviving
  lines, runs of blank lines fold to one.
- **Head + elided + tail windowing (cat/bat/less only).** If the
  filtered text exceeds `CAT_HEAD_LINES + CAT_TAIL_LINES` (400 + 100
  by default), keep the first 400 and last 100, emit
  `[engraph: omitted N middle lines]` in between. `head` and `tail`
  skip this step: the user already chose a window via `-n`, and
  re-windowing on top of that is more confusing than helpful.

**Empty-filter fallback.** If language stripping accidentally empties
a non-empty input (all-comments file, comment-only header, etc.),
`fallback_if_emptied` returns the raw text prefixed by
`[engraph: filter emptied input; raw follows]`. Claude never sees a
blank file by mistake — a class of failure rtk reported and engraph
adopts as a hard guarantee from day one.

**Unknown extensions** pass through unstripped. That keeps `.csv`,
`.json`, `.md`, and anything with `#` or `//` as legitimate data
characters from getting silently corrupted.

---

## 5. Tool-use hooks: PreToolUse and PostToolUse

Three Claude Code lifecycle events get engraph handlers:
`PreToolUse(Bash)` (`run_pre_bash_hook` at
`engraph-cli/src/main.rs:998`) rewrites or denies bash commands;
`PreToolUse(Grep)` (`run_pre_grep_hook` at `:1179`) redirects symbol
lookups to the codegraph; `PostToolUse(Read)` (`run_post_read_hook` at
`:1206`) appends an indexed-symbol map to file reads. All three read
their tool-input JSON from stdin, decide an outcome, emit the
appropriate JSON on stdout, and exit 0.

### 5.1 PreToolUse(Bash) decision tree

```
command empty? → Passthrough
command starts with "engraph " or contains " engraph run "? → Passthrough (recursion guard)
has_heredoc(command)? → Passthrough (rewriting would corrupt the body)
shell_words::split → argv
strip_command_prefix(argv):
    argv[0] in {sudo, env}? → Passthrough (different privilege / non-trivial flag parsing)
    peel leading `FOO=bar` tokens into `prefix`
    (whitespace inside a value → Passthrough; re-quoting would be fragile)
normalize_argv0(argv): /usr/bin/grep → grep
strip_git_global_opts(argv): drop -C/-c/--git-dir=/--work-tree= when argv[0] == "git"
try_subgraph_redirect_for_bash(argv, conn):
    argv[0] in {rg, grep} AND first non-flag arg resolves to 1-3 indexed entities
        → DenySuggest pointing at `engraph subgraph <pattern>`
command has unquoted shell meta (|, ;, &, <, >, `, $())?
    → scan argv for any wrappable token
    → first match → DenySuggest with engraph run hint
    → no match    → Passthrough
otherwise:
    pick(argv[0], argv[1..])
    filter_id == "generic"? → Passthrough
    else → Rewrite: `<prefix...> engraph run <argv...>` with shell-words-quoted
            args (prefix tokens are emitted verbatim — they were validated
            shape-safe during peeling, and shell_words::quote would
            mis-quote `KEY=value` as a literal command name)
```

Three outcomes, returned as a `RewriteOutcome` enum
(`engraph-cli/src/main.rs:734`), each mapping to a Claude Code hook response:

- **Rewrite**: `permissionDecision: "allow"` + `updatedInput.command =
  "engraph run …"`. Claude Code substitutes the new command before
  executing. The rewrite is invisible to Claude's reasoning loop — Claude
  sees the wrapped command run, with the compressed output, in the
  transcript.
- **DenySuggest**: `permissionDecision: "deny"` +
  `permissionDecisionReason` containing the wrappable subcommand or
  the subgraph hint. Used when the command is too complex to rewrite
  safely (compound pipelines) or when engraph has a much smaller
  structured answer available (subgraph redirect).
- **Passthrough**: no JSON, exit 0. Claude Code treats this as "no
  decision," runs the original command unchanged.

### 5.2 Why PreToolUse to rewrite, PostToolUse only to augment

Claude Code's PostToolUse hooks **cannot replace** the tool's output —
they can only append to it (via `hookSpecificOutput.additionalContext`).
To get the compressed output into Claude's transcript, the command
itself must be rewritten before execution. The PreToolUse
`updatedInput.command` is the only path for that. The same property
in reverse is why PostToolUse is the right surface for the
codegraph **augment** in §5.6 — appending an indexed-symbol map after
a Read doesn't need to (and couldn't) displace the Read's actual
content.

### 5.3 Quote-aware shell-meta detection

`has_unquoted_shell_meta` (`engraph-cli/src/main.rs:965`) tracks single quotes, double
quotes, backslash escapes, and `$(...)` substitutions while scanning for
meta characters. False positives are tolerated (the fallback is the safe
DenySuggest); false negatives would be catastrophic (rewriting a
compound pipeline that the wrapper doesn't actually handle), so the
detection errs conservative.

`has_heredoc` (`:831`) reuses the same quote-tracking loop to spot
`<<TAG` outside quotes. Heredoc detection short-circuits the rest of
the decision tree to Passthrough: any rewrite that re-shelled the
command would terminate the heredoc body at the wrong place or pull
the body lines into argv.

`is_env_assignment` (`:938`) is a tight identifier-equals-anything
check matching POSIX variable-name rules. It's used both by the
prefix-peeling logic and (legacy) directly in the compound scan.

### 5.4 Parser-shape normalizers

The rewrite pass would misroute several common command shapes without
explicit normalization. Each is a single helper called from the head
of `try_auto_rewrite` (`:746`):

- `strip_command_prefix` (`:864`) peels `sudo` / `env` (by bailing
  out — different privilege boundary or non-trivial flag parsing)
  and leading `FOO=bar` env assignments (peeled and re-emitted ahead
  of `engraph run` so the child inherits them). Values containing
  whitespace bail to Passthrough — re-quoting `MSG='hello world'` for
  the rewrite is fragile, and Passthrough at least runs the original
  command correctly.
- `normalize_argv0` (`:891`) maps absolute and `./`-prefixed argv[0]
  values through `Path::file_name` so `/usr/bin/grep` and
  `./bin/git` classify the same as the bare names. Pure
  normalization — does nothing to non-path argv[0].
- `strip_git_global_opts` (`:909`) walks argv when argv[0] == "git"
  and drops the global options `-C path`, `-c key=value`,
  `--git-dir=…`, `--work-tree=…`. Without this, `git -C /tmp status`
  classifies as `generic` (because `pick("git", ["-C", ...])` falls
  through), losing all of the git-specific filter savings.

### 5.5 Subgraph redirect on `rg` / `grep`

`try_subgraph_redirect_for_bash` (`:1134`) runs after the parser
normalizers but before the compound-shell and filter-pick branches.
It uses the same normalized argv: argv[0] must be `rg` or `grep`
(so `/usr/bin/rg` is in scope thanks to `normalize_argv0`), and the
first non-flag arg is the candidate pattern.

Shared with the native `PreToolUse(Grep)` path:

- `is_symbol_lookup` (`:1087`) requires bareword shape
  `^[A-Za-z_][A-Za-z0-9_]*$` and length ≥ 3. Regex metachars and
  short tokens (`if`, `id`) skip the redirect.
- `try_subgraph_redirect` (`:1107`) does a
  `SELECT COUNT(*) FROM (SELECT 1 FROM entities WHERE name = ?1
  OR id = ?1 LIMIT 4)`. Only counts 1–3 trigger DenySuggest; 0 is a
  non-indexed name, 4+ would resolve as Ambiguous in
  `subgraph_for` (subgraph.rs:80) and force a retry anyway.

The gate matters: a permissive heuristic would deny on common
identifiers like `new` / `run` / `parse` (any of which has dozens of
entities in any real codebase) and Claude would burn turns hitting
deny → subgraph → ambiguous → retry. The 1–3 cap correlates the
redirect with cases where subgraph actually has a useful neighborhood
to show.

### 5.6 PostToolUse(Read) codegraph augment

`run_post_read_hook` (`:1206`) is the answer to "what about file
reads?" PreToolUse on Read could only rewrite `tool_input.file_path`
(the path, not the content), and PostToolUse can't replace the tool
result — so neither hook can *compress* Claude Code's native Read
output. PostToolUse **can** append, and that's enough leverage.

Flow:

1. Parse `tool_input.file_path` from the stdin JSON. Empty path →
   Passthrough.
2. `engraph_codegraph::subgraph::entities_in_file(conn, path, 30)` —
   sibling of `query_siblings` (subgraph.rs:141), same predicate
   `WHERE file_path = ?1 ORDER BY line_range LIMIT ?2` but without
   the self-id exclusion. Empty result → Passthrough (file isn't
   indexed).
3. `build_read_context` (`:1254`) renders each entity as
   `` - `name` @ line_range — `signature` `` (signatureless entities
   skip the em-dash + backticks block to avoid `— \`\``).
4. `truncate_to_bytes(out, MAX_BRIEF_BYTES)` caps the addition at
   the same 2KB ceiling the SessionStart brief uses, so a single
   read of a large file can't blow the per-tool-result budget.
5. Emit `hookSpecificOutput.additionalContext` with
   `hookEventName: "PostToolUse"`. Claude Code appends it to the
   Read's tool-result block.

Telemetry charges the augment its output token count so
`engraph gain` shows the cost, not just the savings.

Tests in `crates/engraph-cli/tests/pre_bash_hook.rs`,
`pre_grep_hook.rs`, and `post_read_hook.rs` pin each hook's branches
(rewrite, deny, passthrough, augment) plus tricky cases like
`git log --grep='foo && bar'` (meta inside single quotes — must not
deny), heredoc shapes, env-prefix re-emission, absolute-path argv[0]
classification, and entities without signatures rendering cleanly.

---

## 6. JSONL ingest pipeline

`engraph_ingest::ingest_file(conn, path)` is the entry point
(`engraph-ingest/src/lib.rs:194`). It walks the Claude Code transcript
file at `path` and inserts new messages into the DB, tracking offsets so
re-ingest is incremental.

### 6.1 Rotation / truncation / partial-line detection

Three failure modes for naïve "last offset" tracking:

1. **Rotation**: log rotated; new inode at same path.
2. **Truncation**: writer opened with `O_TRUNC`; same inode, smaller size.
3. **Partial trailing line**: a writer flushed mid-line; our `read_line`
   returns the partial bytes without a newline terminator.

The fix uses the fingerprint `(last_offset, last_inode, last_size)` stored
in `ingestion_log`:

```
prev_inode mismatches OR
file_size < prev_size (shrunk) OR
file_size < last_offset (truncated)
    → re-ingest from offset 0
otherwise → resume from last_offset

inside the read loop:
    line = reader.read_line()
    if line is empty (EOF): break
    if line doesn't end with '\n': partial — break WITHOUT advancing offset
    else: advance offset, process line
```

The partial-line fix is critical (regression test
`ingest_holds_offset_when_trailing_line_is_partial`,
`engraph-ingest/src/lib.rs`). Without it, a writer flushing mid-line
would commit our offset past the unparsed bytes, permanently skipping
that line on the next ingest.

### 6.2 Sidechain filtering

Claude Code's sub-agent feature emits transcript events with
`isSidechain: true`. `RawEvent::is_sidechain` (`engraph-ingest/src/lib.rs:42`)
catches them at parse time and skips them — those events would otherwise
pollute the main session memory with sub-agent chatter.

### 6.3 Per-file transactional commit

The previous auto-commit path issued ~3 statements per message
(`upsert_session`, `INSERT` into `messages`, scope membership). On a
5k-message transcript that's ~15k fsyncs in WAL mode. The current
implementation wraps the whole file's writes in a single SQL transaction
via a `TxGuard` RAII helper (`engraph-ingest/src/lib.rs:58`).

```rust
struct TxGuard<'a> {
    conn: &'a PooledConn,
    finished: bool,
}
impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        if !self.finished { let _ = self.conn.execute_batch("ROLLBACK"); }
    }
}
```

The guard exists because raw `BEGIN`/`COMMIT` via a pooled connection
would, on any `?` error in the loop, return the connection to the pool
with an open transaction. The RAII rollback closes that hole. We don't
use `rusqlite::Transaction` directly because that would require either
changing every downstream function signature from `&PooledConn` to
`&Connection`, or sprinkling `&*tx` at every call site — both noisier
than the small guard.

Expected throughput improvement is 5–10× on real transcripts (3
statements per message → 1 fsync for the whole file).

### 6.4 Compress-existing sweep + FTS retention

`compress_existing(conn, batch)` (`engraph-ingest/src/lib.rs:100`) walks
`messages` and `context_items`, compresses any row whose
`content_compressed = 0` and whose token count exceeds the threshold,
and writes the compressed text back. Idempotent: the sentinel fast-path
makes a re-run a no-op even if `content_compressed` wasn't updated;
rows are also marked `content_compressed = 1` to skip re-tokenization
on the next sweep.

#### The FTS retention problem

Original schema: `messages_au` AFTER UPDATE trigger fires on every
content change, deletes the old FTS row, inserts the new one. After a
sweep, FTS would index the *compressed* text. Recall against the user's
original phrasing would degrade silently.

v5 migration drops `messages_au` and `context_items_au`. The INSERT
trigger still indexes new rows; the DELETE trigger still removes them.
UPDATE no longer touches FTS. Since SQLite's rowid is immutable across
UPDATEs when the primary key (TEXT) is unchanged, the FTS row remains
anchored to the same message after compression.

Tested by `compress_existing_keeps_fts_pointed_at_original` which seeds
a distinctive phrase, runs the sweep, and asserts FTS recall still
hits the original phrase against the compressed message.

---

## 7. Retrieval (F3)

`engraph_retrieve::search(conn, &Query)` returns `Vec<Hit>` sorted by
score (`engraph-retrieve/src/lib.rs:69`).

### 7.1 Targets

A `Query` can request any combination of:

- `Target::Messages` (FTS5 over `messages_fts`, BM25-ranked)
- `Target::ContextItems` (FTS5 over `context_items_fts`)
- `Target::Bugs` (substring LIKE on summary/content)
- `Target::Entities` (substring LIKE on name)

Messages/ContextItems are full-text-indexed; Bugs/Entities use a simpler
substring filter because their content is typically short and structured.

### 7.2 FTS5 query sanitization

`sanitize_fts(s)` (`engraph-retrieve/src/lib.rs:272`) strips FTS5
meta-characters (`"`, `*`, `(`, `)`, `:`), then quotes each remaining
word and joins with whitespace (implicit AND). The empty case yields
`"\"\""`, a valid no-match query — avoids a syntax error when sanitizing
strips everything.

This means user-supplied text like `auth: token?` is rendered as
`"auth" "token?"`, AND-ed, with FTS5 quote semantics for each word.

### 7.3 Scoping

`ScopeFilter::All` runs unfiltered. `ScopeFilter::Project(name)`
resolves to the set of `scope_members` whose scope has
`kind = 'project' AND name = ?`. `ScopeFilter::Scope(id)` is a single
scope. Resolution happens once per query (`scope::resolve`,
`engraph-retrieve/src/scope.rs:8`).

The SQL is built dynamically because the placeholder count varies with
the number of scope IDs (`?{i}` per ID, then the FTS query, then the
limit). The `limit` is bound as `Value::Integer`, not `Value::Text`, so
SQLite doesn't type-coerce.

### 7.4 BM25 ranking

FTS5's `bm25(messages_fts)` returns a *negative-is-better* number
(SQLite convention so an `ORDER BY rank` ascending matches relevance
descending). `Hit::score` flips the sign so all consumers can sort
descending uniformly.

### 7.5 The hybrid path (Reciprocal Rank Fusion)

Behind the `embeddings` Cargo feature, `Strategy::Hybrid` fuses three
sources via RRF (`engraph-retrieve/src/hybrid.rs:53`):

```
                ┌────────────────────────────┐
                │  FTS5 candidate pool        │
                │  (limit * CANDIDATE_MULT)   │   ┌── ranks by BM25
                └─────┬──────────────────────┘   │
                      │                          │
                      ▼                          ▼
              ┌─────────────────┐
              │ embed query     │
              │ (one call)      │
              └────┬────────────┘
                   │
                   ▼ for each candidate, fetch stored vector
              ┌─────────────────┐
              │ rank by cosine  │── ranks by semantic similarity
              └────┬────────────┘
                   │
                   ▼ sort candidates by ts desc
              ┌─────────────────┐
              │ rank by ts      │── ranks by recency
              └────┬────────────┘
                   │
                   ▼
              ┌─────────────────────────────────────────────────────┐
              │  rrf(d) = w_lex/(k+lex_rank(d))                      │
              │        + w_sem/(k+sem_rank(d))                       │
              │        + w_rec/(k+rec_rank(d))                       │
              │  missing source → 0 contribution                     │
              └─────────────────────────────────────────────────────┘
```

#### Why not weighted-sum of scores

The natural-looking `α·BM25 + β·cosine` is scale-broken. BM25 is
unbounded positive (typically 0–20+); cosine sits in `[-1, 1]`. The
larger-scale source dominates regardless of weights, no matter what `α`
and `β` you pick. Min-max normalization fixes the scales but is sensitive
to outliers and varies per query.

#### Why ranks, not scores

Ranks are unitless. Combining ranks is composable: a new source slots in
as one more term in the sum without reweighting anyone else. Adding the
recency source (v2.1) literally added one line to the RRF accumulator.

#### Constants

| Constant | Value | Rationale |
|---|---|---|
| `K_RRF` | `60.0` | Standard value from the original RRF paper (Cormack/Clarke/Büttcher SIGIR 2009). Larger `k` flattens the top-of-list weighting. |
| `W_LEXICAL` | `1.0` | Equal-weight with semantic — the two are co-equal content signals. |
| `W_SEMANTIC` | `1.0` | See above. |
| `W_RECENCY` | `0.5` | Half-weight — freshness is a tiebreaker, not a primary criterion. |
| `CANDIDATE_MULT` | `4` | The FTS stage pulls `q.limit * 4` candidates so the reranker has headroom. |

Max achievable score: `(1 + 1 + 0.5) / (60 + 1) ≈ 0.041`. The bound is
checked in the hybrid test suite.

#### Recency wiring

RFC3339 ISO strings sort chronologically as lexicographic strings (this
is the standard's point). Candidates without a `ts` are absent from
the recency list — a contribution of zero rather than a synthesized
worst-rank. This matters: an unsorted "no ts" message shouldn't be
penalized by a content-orthogonal signal it never had.

Tested in `hybrid_recency_tiebreaks_toward_newer`: two messages with
identical content but different `ts` — the newer ranks first, by a
margin that's exactly `W_RECENCY/(K+1) - W_RECENCY/(K+2)` ≈ 0.000132.
Without recency wired (`W_RECENCY = 0`), the alphabetical `target_id`
tiebreak would put the older message first. The test discriminates the
wire-up by setting `target_id` to alphabetically disagree with `ts`.

### 7.6 Embedding provider trait

`EmbeddingProvider` (`engraph-core/src/embedding.rs:10`):

```rust
pub trait EmbeddingProvider: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
}
```

Three methods. `embed` produces the vector. `model_id` is stored in the
`embeddings` table alongside each row; queries filter by it so a model
upgrade can detect stale rows ("vectors under `bge-small-en-v1.5`" vs
"vectors under `next-model-v2`"). `dim` exists for schema validation
hooks.

Default implementation (`fastembed_provider::FastEmbedProvider`,
`engraph-core/src/embedding.rs:41`) wraps `fastembed-rs` with the
`bge-small-en-v1.5` model (~130MB on disk, 384 dims). The `Mutex` around
the underlying `TextEmbedding` matches fastembed's `&mut self`
requirement on `embed`; for our workload (compress-existing sweep +
ad-hoc recall) the lock contention is negligible.

A cloud provider — call it `OpenAIEmbeddingProvider` — would slot in by
implementing the same trait. No call site changes needed.

---

## 8. SessionStart brief hook (F4)

`run_session_start_hook` (`engraph-cli/src/main.rs:421`). Reads Claude's
SessionStart JSON from stdin, produces a markdown brief, emits it as
`hookSpecificOutput.additionalContext`.

### 8.1 What goes in

For the resolved `cwd`:

- `## do-not-repeat` — top 5 rules from `do_not_repeat WHERE project = ?`.
- `## open bugs` — top 5 unresolved rows from `bugs`.

For the resolved `session_id`:

- `## budget` — if usage > 0 or limits diverge from defaults.

Empty briefs are emitted as the empty string. A truly fresh project
costs zero injected tokens. The cap is `MAX_BRIEF_BYTES = 2048`;
overflow is truncated with a `…[truncated]` marker preserving UTF-8
boundaries (`truncate_to_bytes`, `engraph-cli/src/main.rs:516`).

### 8.2 Why this exact set

The brief is the per-session opening cost. Every byte injected costs
context Claude could spend on the actual task, so the bar is high:
each section must surface information Claude would otherwise have to
hunt for. Do-not-repeat rules are obvious wins (the user explicitly
told us to avoid these); open bugs come second (the user expects
continuity across sessions). Decisions/notes were *also* in this
brief originally, but the query was dead (no producer ever wrote those
rows) — fixed in the v2.1 polish by deleting the query rather than
building the producer (F8 deferred).

---

## 9. Budget enforcement (F11)

`engraph-core/src/budget.rs`. Per-session token budget with escalation
levels.

### 9.1 Schema

```
session_budget(session_id PK, soft_limit, hard_limit, used_tokens, escalation_level, updated_at)
```

`get_or_init` creates a row with defaults
(`DEFAULT_SOFT_LIMIT = 100_000`, `DEFAULT_HARD_LIMIT = 150_000`) on first
read. `add_used` increments `used_tokens` and recomputes the escalation
level atomically inside the UPDATE statement.

### 9.2 Escalation levels

```
used ≥ hard            → level 3
used ≥ soft            → level 2
used ≥ soft / 2        → level 1
otherwise              → level 0
```

The intent: filters and retrieval can read the level and apply stricter
caps when the budget is tight (e.g., shorter `tree` depth, fewer recall
candidates). The level is *advisory* — nothing in the runtime hard-stops
on it today. Adaptive compression based on the level is a deferred
roadmap item.

### 9.3 Atomicity

The level computation lives inside the SQL `UPDATE` so concurrent
`add_used` calls don't race. Tested by
`add_used_is_atomic_under_threads` which fires 16 threads × 25
increments and verifies the final sum is exactly 400.

### 9.4 When charges happen

`Cmd::Run` (`engraph-cli/src/main.rs`) calls `budget::add_used` with
the post-filter `output_tokens` if `CLAUDE_SESSION_ID` is set in the
environment. The post-filter count is what actually lands in Claude's
context; the pre-filter input is recorded for telemetry but never gets
sent. No session id means the budget path is skipped — the CLI still
works for standalone invocations.

---

## 10. Telemetry

`events` table (one row per significant operation), `engraph gain`
report.

### 10.1 Event shape

```
events(seq INT PK, id UUIDv7, session_id, kind, feature, filter_id,
       input_tokens, output_tokens, latency_ms, ts)
kind ∈ {compress, retrieve, hook, wrapped_cmd}
```

UUIDv7 IDs are time-ordered for natural chronological indexing without
needing a separate `created_at` column.

`record_event` (`engraph-core/src/telemetry.rs:15`) is the single insert
point. Every feature that compresses, retrieves, hooks, or runs a
wrapped command writes a row.

### 10.2 Savings semantics

`saved_tokens = input - output` is only meaningful for kinds where the
input is the pre-compression size and the output is the post-compression
size: `compress` and `wrapped_cmd`. `retrieve` and `hook` have no
savings semantic (input is zero or doesn't represent the same thing), so
`GainRow::saved_tokens = None` for them, rendered as `-` in the table.

This is the contract `engraph gain` is built around. The `TOTAL_SAVED`
row at the bottom sums only the rows that contributed a numeric
`saved_tokens`.

---

## 11. Cross-platform

### 11.1 Unix-only code paths

Exactly two places.

- **File inode** (`engraph-ingest/src/lib.rs:414`): used in the rotation
  fingerprint. On non-Unix the function returns `None`, and the rotation
  detection relies entirely on `(size, mtime)` — slightly weaker but
  functional.
- **Signal handling** (`engraph-cli/src/main.rs:791`): `tokio::signal::unix`
  installs no-op handlers for SIGINT/SIGTERM during `engraph run` so
  the parent doesn't die before draining the child's output. The block
  is `#[cfg(unix)]`; on Windows the terminal Ctrl-C still reaches the
  child via its own console handling.

Everything else — SQLite, FTS5, tokio process spawning, `bundled`
rusqlite, regex — is portable.

### 11.2 Tokio runtime in the CLI

`Cmd::Run` is the only async surface. The CLI builds a single-threaded
tokio runtime *inside* the synchronous main loop (`run_wrapped_command`,
`engraph-cli/src/main.rs`). This keeps the rest of the CLI sync and
avoids paying tokio's startup cost for subcommands that don't need it
(`gain`, `recall`, `compress`, `ingest`, `budget`, …).

Tokio's job inside that runtime:

- `tokio::process::Command` spawns the child with inherited stdin (so
  pagers, interactive commands work).
- `child.wait_with_output()` drains stdout and stderr concurrently, so
  neither can deadlock by filling its 64KB pipe buffer while we wait on
  the other.
- The `tokio::signal::unix` handlers keep the parent alive while the
  child processes terminal signals on its own.

Tested by `wrapped_run_inherits_stdin` (positive: round-trips a phrase
through `engraph run cat`) and
`wrapped_run_drains_large_concurrent_output_without_deadlock` (negative:
200KB on each of stdout/stderr from a shell command, well past the pipe
buffer).

---

## 12. Schema migrations & drift detection

`engraph-core/src/schema.rs`.

`MIGRATIONS: &[&str]` — each entry is one SQL batch. On open,
`run_migrations` applies any whose index ≥ current version, inside a
transaction, and records `INSERT INTO migrations (version)`.

`check_drift(expected)` runs after migrations. If the on-disk schema
version is *higher* than the binary's `SCHEMA_VERSION`, it returns
`Error::SchemaDrift` — running an older binary against a newer DB
risks corrupting tables the binary doesn't know about, so we refuse.
A lower version after a migration run is impossible by construction
but logs a warning if it ever happens.

### 12.1 Migration history

| v | What |
|---|---|
| 1 | Foundation: `sessions`, `messages`, `events`, `session_budget`. |
| 2 | Retrieval + ingest: `scopes`/`scope_members`/`context_items`/`bugs`/`do_not_repeat`/`ingestion_log`/`entities`/`relations`, plus FTS5 virtual tables + their INSERT/DELETE/UPDATE triggers. |
| 3 | `embeddings` table (always present; feature-gated only at query time). |
| 4 | `ingestion_log` rotation fingerprint columns (`last_inode`, `last_size`). |
| 5 | Drop `messages_au` / `context_items_au` triggers so `compress-existing` doesn't overwrite the FTS index (§6.4). |
| 6 | F2 codegraph: add `file_path`, `line_range`, `signature` columns to `entities` plus `idx_entities_file_path`. `relations.kind` is validated in Rust (`RelationKind` enum) rather than via DB CHECK; SQLite can't `ALTER ADD CHECK` and rebuilding the FK-referenced table risked dropping rows. |

---

## 13. Test coverage map

Where each behavior is pinned. Use this as the navigation index.

| Behavior | Test |
|---|---|
| Schema migrations idempotent | `engraph-core/src/schema.rs::tests::migrations_apply_idempotently` |
| Schema drift refused | `db::tests::open_pool_creates_and_migrates` |
| Budget escalation thresholds | `budget::tests::escalation_thresholds` |
| Budget atomic under threads | `budget::tests::add_used_is_atomic_under_threads` |
| Compress idempotent (fixed-point) | `engraph-compress/src/lib.rs::tests::idempotent_on_second_pass` |
| Compress sentinel marker | `tests::sentinel_marker_present_after_compress` |
| Compress ratio < 1 on long input | `tests::ratio_under_one_for_long_input` |
| Ranking determinism | `rank::tests::deterministic` |
| Brevity drops articles | `brevity::tests::drops_articles_and_fillers` |
| Per-filter ratio gates | `engraph-compress/tests/filter_ratios.rs` (one test per filter) |
| Picker / FilterOutput id agreement | `picker_and_filter_output_agree_on_filter_id` |
| Shared util: strip_ansi, dedup_consecutive, truncate_lines, tail_lines, drop_matching | `engraph-compress/src/filters/util.rs::tests` |
| Golden snapshot fixtures | `engraph-compress/tests/golden_fixtures.rs` |
| cargo-nextest format | `filters::cargo::tests::nextest_failures_counted_and_pass_lines_dropped` |
| Pre-bash auto-rewrite branches + parser shapes (sudo/env/abs-path/git -C/heredoc) | `engraph-cli/tests/pre_bash_hook.rs` |
| Pre-grep subgraph redirect gate (1–3 match, regex, short, ambiguous) | `engraph-cli/tests/pre_grep_hook.rs` |
| Post-read augment shape (additionalContext, signatureless entity, unindexed file passthrough) | `engraph-cli/tests/post_read_hook.rs` |
| Read-bucket filter: per-language comment strip, head/tail no-rewindow, empty-filter fallback | `engraph-compress/tests/read_filter.rs` |
| Session-start brief content | `engraph-cli/tests/session_start_hook.rs` |
| `engraph run` budget tracking | `engraph-cli/tests/run_budget.rs::wrapped_run_charges_session_budget` |
| `engraph run` stdin inheritance | `wrapped_run_inherits_stdin` |
| `engraph run` no pipe-buffer deadlock | `wrapped_run_drains_large_concurrent_output_without_deadlock` |
| Ingest M2 partial trailing line | `ingest_holds_offset_when_trailing_line_is_partial` |
| Ingest rotation/truncation replay | `ingest_detects_truncation_and_replays` |
| Ingest sidechain skip | `ingest_skips_sidechain_events` |
| Sweep preserves FTS recall | `compress_existing_keeps_fts_pointed_at_original` |
| Sweep recoverability hash | `compress_existing_preserves_recoverability_hash` |
| FTS query sanitization | `engraph-retrieve/src/lib.rs::tests::sanitize_quotes_words` |
| Recall + scoping end-to-end | `engraph-retrieve/tests/end_to_end.rs` |
| Hybrid RRF reordering | `hybrid_path.rs::hybrid_reorders_vs_fts` |
| Hybrid recency tiebreak | `hybrid_recency_tiebreaks_toward_newer` |
| Hybrid handles missing embeddings | `hybrid_handles_unembedded_candidates` |
| Embedding upsert idempotent | `upsert_is_idempotent` |
| Cosine basics + length mismatch | `embedding::tests::cosine_basics` / `cosine_handles_mismatched_lengths` |
| Schema v6 entity columns present | `schema::tests::migrates_through_v6_entity_columns` |
| SCIP loader two-pass emits CALLS edges | `engraph-codegraph/tests/loader_unit.rs::loader_emits_entities_and_a_calls_edge` |
| SCIP loader idempotency (re-load is a no-op) | `loader_unit.rs::reloading_same_bytes_is_idempotent` |
| SCIP loader scopes DELETE to project | `loader_unit.rs::loader_scopes_deletes_to_project` |
| Subgraph markdown shape + sections | `subgraph::tests::subgraph_returns_calls_called_by_and_siblings` |
| Subgraph ambiguity disambiguation | `subgraph::tests::ambiguous_name_emits_disambiguation` |
| Subgraph byte-cap truncation | `subgraph::tests::byte_cap_truncates_with_marker` |
| Driver file-probe detection | `engraph-codegraph/tests/drivers_detect.rs` (one per build system) |
| Driver live end-to-end (per language) | `engraph-codegraph/tests/drivers_live.rs` (soft-skips when the indexer or build tool is absent) |
| Suffix-fallback kind classifier | `scip_loader::tests::suffix_kind_classifies_descriptors` |
| Cross-repo subgraph annotation | `subgraph::tests::cross_repo_edge_gets_repo_annotation` |
| Workspace discovery (single root, children, exclusions) | `engraph-codegraph/tests/workspace_cross_repo.rs::discover_*` |
| Workspace cross-repo CALLS edge end-to-end | `workspace_cross_repo.rs::workspace_links_app_b_caller_to_lib_a_foo` |
| Bazel detection markers | `bazel::tests::detect_bazel_recognizes_markers` |
| Bazel JSON-proto parser | `bazel::tests::parse_jsonproto_lines_extracts_rules` |
| Bazel location string parser | `bazel::tests::parse_location_handles_file_line_col` |
| Bazel target display name | `bazel::tests::target_display_name_strips_pkg_prefix` |
| Bazel discovery as workspace root | `engraph-codegraph/tests/bazel_live.rs::discover_recognizes_workspace_file` |
| Bazel target-level index end-to-end | `bazel_live.rs::target_level_index_creates_targets_and_deps` (soft-skips when bazel/bazelisk absent) |
| Bazel re-index idempotency | `bazel_live.rs::reindex_is_idempotent` |
| SCIP loader preserves co-resident `BAZEL_DEPENDS_ON` edges | `loader_unit.rs::loader_preserves_bazel_depends_on_edges` |
| Symbol-level SCIP byte-stream merge | `bazel_symbols::tests::merge_scip_bytes_preserves_documents_and_externals` / `merge_scip_bytes_empty_input_is_valid_empty_index` / `merge_scip_bytes_skips_empty_byte_blobs` |
| Symbol-level bazel-query target probe | `bazel_symbols::tests::label_kind_nonempty_detects_lines` |
| Symbol-level `LangStatus` display formatting | `bazel_symbols::tests::lang_status_display_mentions_binary` / `lang_status_display_failed_message` |
| Symbol-level Bazel Java end-to-end | `bazel_symbols_live.rs::symbol_level_java_indexes_and_preserves_target_level` (triple-gated on bazel + scip-java + `ENGRAPH_LIVE_BAZEL_SYMBOLS=1`) |

---

## 14. Codegraph (F2 Phases 2.1 + 2.2 + 2.3)

`engraph index <repo>` runs an external SCIP indexer for the detected
language, decodes the resulting `index.scip` protobuf, and writes
symbols and references into the existing `entities`/`relations`
tables. `engraph subgraph <symbol>` then returns a 2-hop markdown
neighborhood that typically compresses 100× against the file-Read +
grep loop Claude would otherwise run.

### 14.1 Crate boundary

`crates/engraph-codegraph/` is its own workspace member. Five modules:

| Module | Role |
|---|---|
| `driver.rs` | `trait Driver { name, detect, command, output_path }` + five impls (`RustAnalyzer`, `ScipPython`, `ScipGo`, `ScipTypescript`, `ScipJava`) + `registry()` |
| `relation_kind.rs` | `enum RelationKind { Defines, References, Calls, Implements, Extends, Imports }` — the in-Rust validator that replaces a DB-level CHECK on `relations.kind` |
| `scip_loader.rs` | SCIP protobuf bytes → entity/relation rows, two-pass (§14.3) |
| `index.rs` | `index_repo()` orchestrator: pick driver → spawn → load |
| `subgraph.rs` | `subgraph_for()` + `format_markdown()` (§14.4) |

The crate depends on `scip = "0.5"` and `protobuf = "3.7"` (rust-protobuf;
scip 0.5 ships pre-generated message code so no `protoc` is needed at
build time). Bumping to scip 0.7 would require raising the workspace
MSRV from 1.75 to 1.81.

### 14.2 Driver registry

Each driver is a small adapter: file-probe `detect()` + argv-builder
`command()`. Engraph shells out to the language-specific indexer rather
than re-implementing type resolution per language — there is no single
crate that does cross-file `who-calls` across Rust/Python/Go/TS/Java.

| Driver | `detect()` trigger | `command()` |
|---|---|---|
| `RustAnalyzer` | `Cargo.toml` | `rust-analyzer scip <repo> --output <repo>/index.scip` (chdir into repo) |
| `ScipPython` | `pyproject.toml` or `setup.py` | `scip-python index --cwd <repo> --output <repo>/index.scip --project-name <basename>` |
| `ScipGo` | `go.mod` | `scip-go --module-root <repo>` (chdir into repo) |
| `ScipTypescript` | `package.json` + `tsconfig.json` | `scip-typescript index` (chdir into repo) |
| `ScipJava` | `pom.xml` / `build.gradle*` / `build.sbt` / `build.sc` | `scip-java index --output <repo>/index.scip` (chdir into repo) |

scip-java's `index` subcommand auto-detects `{Maven, Gradle, sbt, mill}`
when invoked standalone (the path covered by `drivers_live.rs`).
On a Bazel workspace the same binary is driven by the symbol-level
Bazel pathway (§14.9), where scip-java materializes its own aspect and
orchestrates Bazel internally — engraph does not reimplement aspect
dispatch.

`scripts/install-scip-indexers.sh` is the companion installer: idempotent,
per-toolchain prerequisite checks, surfaces WSL-specific failure modes
(Windows npm prefix on `/mnt/c` is invisible to Linux PATH, etc.).

### 14.3 SCIP loader — two-pass

SCIP's `SymbolRole` is a bit-flag enum: `Definition | Import |
WriteAccess | ReadAccess | Generated | Test | ForwardDefinition`. There
is no "Call" bit — `CALLS` vs `REFERENCES` must be disambiguated using
the *target* symbol's `SymbolInformation.kind`, which can live in any
document of the Index.

**Pass 1** walks every `Document.symbols[*]` (and `Index.external_symbols`)
and builds `HashMap<String, Kind>`. **Pass 2** walks
`Document.occurrences[*]` and decides:

- `Definition` role set → upsert an entity row with `file_path =
  Document.relative_path`, `line_range = "{start+1}:{end+1}"` (SCIP
  ranges are 0-based; engraph stores 1-based for editor compatibility),
  `signature` from `SymbolInformation.signature_documentation.text`.
- `Import` role set → `IMPORTS` edge.
- Non-definition, target kind ∈ {Function, Method, Constructor,
  StaticMethod, AbstractMethod, Macro} → `CALLS`.
- Non-definition, target kind ∈ {Class, Struct, Interface, Trait, Enum,
  TypeAlias, Type, Protocol} → `REFERENCES`.
- Target kind is a local (Variable, Parameter, Field, …) or unknown →
  edge is **skipped entirely**. Recording these would flood the
  subgraph with un-navigable noise (e.g. `Called by conn in budget.rs`).
- `SymbolInformation.relationships[].is_implementation` → `IMPLEMENTS`.
- `SymbolInformation.relationships[].is_reference` (and not also
  is_implementation) → `EXTENDS`.
- `Generated` role on the definition → `provenance = 'generated'`;
  otherwise `'extracted'`.

**Anchor heuristic.** Occurrences don't carry their enclosing function
directly. The loader anchors each non-definition occurrence to the
nearest preceding definition *whose kind is anchorable* (function,
method, class, struct, etc.) — never to a local variable or field.
This is what makes `engraph subgraph run_migrations` produce real
callers (`open_pool` etc.) instead of nearby variable names.

**Idempotent atomic swap.** A `TxGuard` (same pattern as
`engraph-ingest`) wraps the whole load:

1. `DELETE FROM relations WHERE src_entity IN (SELECT id FROM entities
   WHERE project = ?) AND kind != 'BAZEL_DEPENDS_ON'` — narrowed in
   two steps from the v2.1 original. v2.2 dropped the `dst_entity`
   variant (cross-repo: deleting inbound edges from *other* projects
   would silently break app_b → lib_a::foo when lib_a re-indexes) and
   stopped deleting `entities` entirely (an in-flight referrer's
   placeholder must not get FK-collapsed during a producer re-index).
   v2.3 #2 added the `kind != 'BAZEL_DEPENDS_ON'` clause so a SCIP
   load running under the same project as a prior target-level Bazel
   pass (§14.8) doesn't wipe its target-level edges. The `!=` form is
   future-proof: a new SCIP-derived kind (e.g. OVERRIDES) auto-cycles
   on re-index; an explicit IN-list would silently leak unknown kinds
   across runs.
2. Bulk re-insert from the current SCIP run. Entities are upserted;
   `file_path` / `line_range` / `signature` are refreshed on conflict
   so a real definition takes ownership from any earlier placeholder.
3. `COMMIT`.

Readers see the prior snapshot until commit. Staging-tables-then-swap
(the heavier pattern ROADMAP.md mentions) is overkill below ~10M
edges; a single `BEGIN…COMMIT` is sufficient at SQLite scale. Stale
entities from removed source code accumulate; a future GC pass can
prune them.

Entity-ID collision across the target-level (§14.8) and symbol-level
(§14.9) Bazel layers is ruled out by inspection: `bazel_target` IDs
are Bazel labels (`//foo:bar`); `symbol` IDs are SCIP monikers
(scheme-prefixed). Disjoint by construction, so both layers coexist
under one project key without ID clashes.

Regression test `loader_preserves_bazel_depends_on_edges`
(`tests/loader_unit.rs`) seeds a `BAZEL_DEPENDS_ON` row + `bazel_target`
entity under project X, runs `load(X, ...)`, asserts the row
survives.

**ID strategy.** `entities.id` is the raw SCIP moniker
(`rust-analyzer cargo engraph-core 0.1.0 schema/run_migrations().`).
Cross-machine moniker normalization has a wire-in hook
(`scip_loader::normalize_moniker`) which is a pass-through today; see
§14.7 for the Phase 2.2 stitching that works against
canonically-published deps without rewrites and the deferred polish
items (per-indexer rules, symbol-stability test suite).

### 14.4 Subgraph query + markdown

`subgraph_for(conn, symbol, max_nodes)` returns a `Neighborhood`:

1. **Resolve** name → entity row(s) via `WHERE name = ?1 OR id = ?1`.
   More than one match → emit a disambiguation block, no edges.
   No "best guess".
2. **Outgoing** (CALLS / REFERENCES / IMPORTS): join `relations` and
   `entities` on `dst_entity`, filter `r.src_entity = ?` and
   `r.valid_to IS NULL`, limit `max_nodes / 2`.
3. **Incoming** (Called by): same shape, swap src/dst.
4. **Siblings** (same file): `WHERE file_path = ? AND id != ?
   ORDER BY line_range LIMIT 10`.

`format_markdown()` lays out the four sections, dedupes by
`(kind, target_id)` for outgoing and by `target_id` for incoming
(rust-analyzer emits one occurrence per call site, which would
otherwise list `migrations_apply_idempotently` twice), filters out
empty-name entries, and stops appending lines once cumulative byte
size hits `DEFAULT_BYTE_CAP = 8192`. The roadmap's goal example is
matched exactly. `engraph subgraph <sym> --json` emits the raw
`Neighborhood` struct for programmatic callers.

### 14.5 Telemetry

`Index` events: `kind = WrappedCmd, feature = "F2", filter_id =
<driver name>, input_tokens = scip_bytes, output_tokens = 0`. The
driver name (`"rust-analyzer"`, `"scip-go"`, …) lets `engraph gain`
slice per-language indexer cost.

`Subgraph` events: `kind = Retrieve, feature = "F2", filter_id =
"subgraph", output_tokens = tokens::count(&markdown)`. Token cost is
the bytes-out into Claude's context — comparable to a `recall`
event for savings accounting.

### 14.6 Coverage on this machine

| Driver | Status |
|---|---|
| `rust-analyzer` | Real end-to-end pass (`drivers_live.rs`); 2400+ entities + 1200+ relations indexed against engraph itself |
| `scip-python` | Real end-to-end pass |
| `scip-go` | Real end-to-end pass |
| `scip-typescript` | Real end-to-end pass |
| `scip-java` | `drivers_live.rs` soft-skips unless `mvn` or `gradle` is on PATH (build-tool dependency). `detect()` + argv covered by `drivers_detect.rs`. |

### 14.7 Phase 2.2 — cross-repo stitching

Cross-repo is mostly free because `entities.id` *is* the SCIP moniker.
When two repos both reference the same fully-qualified symbol (e.g.
`rust-analyzer cargo lib_a 0.0.1 mod/lib_foo().`), they collapse onto
the same row automatically. The work in 2.2 is the surrounding plumbing
that makes this robust under realistic indexing patterns.

**Loader change: drop the entity DELETE.** v2.1's loader did
`DELETE FROM entities WHERE project = ?` on every re-index. That's safe
in a single-repo setting but breaks cross-repo: lib_a re-indexing would
delete lib_a's symbols, and any CALLS edge from app_b pointing to one
of them would be deleted to satisfy the FK, silently destroying app_b's
graph. v2.2 narrows the cleanup to *outgoing* relations only
(`WHERE src_entity IN (… project = ?)`) and never deletes entities.
Stale defs from removed source accumulate; a future GC pass can prune
them. The UPSERT now also updates `project` on conflict so a real
definition takes ownership from any earlier placeholder a referrer had
planted.

**Anchor heuristic — kind specificity.** With cross-crate references in
play, multiple definitions often share the same `start_line` (typically
the file's `crate/` module def sits at line 0 alongside the first
function def). v2.1's "nearest preceding by line" picked arbitrarily on
ties and would attribute calls to `crate/` instead of the actual
enclosing function. v2.2 ranks anchor candidates by kind specificity
(Function/Method = 100, Class/Struct/Enum = 50, Module/Namespace =
10) and breaks line ties in favor of the more specific kind. This is
what makes the workspace test pin `app_caller → lib_foo` instead of
`crate/ → lib_foo`.

**Suffix-fallback kind detection.** When the indexer is pointed at only
the consumer crate, the producer's symbols arrive as occurrences with
no matching `SymbolInformation` — `target_kind` is therefore unknown
and the v2.1 loader dropped the edge. v2.2 falls back to parsing the
SCIP descriptor suffix when kind is missing: `()` / `().` → CALLS,
`#` → REFERENCES, anything else (locals, terms) → skip. Without this
fallback every cross-repo call goes missing.

**`engraph index --workspace <dir>`.** New subcommand. If `<dir>` itself
carries a build manifest, indexes just `<dir>`. Otherwise, walks
immediate children and indexes each whose `Driver::detect()` matches.
Per-repo failures are reported but don't abort the run; the CLI prints
per-repo results plus a workspace summary. Deeper recursion is
intentionally out of scope; tracked as a follow-up in §15.

**Cross-repo subgraph annotation.** `format_markdown` now compares the
queried entity's `project` with each related entity's. When they
differ, the location is prefixed with `repo:<basename>` so the user
sees `lib_foo (repo:lib_a :: src/lib.rs:1)` instead of a bare
`src/lib.rs:1` that could be ambiguous across repos.

**Moniker normalization.** `scip_loader::normalize_moniker` is wired in
as the hook for per-indexer rewrite rules (the scip-go pre-0.7
absolute-path strip the roadmap calls out, etc.) but is a pass-through
today. Real-world stitching of canonically-published deps (Cargo, npm,
PyPI, Go modules) works without any rewrites because their monikers
are already machine-stable.

**End-to-end test.** `tests/workspace_cross_repo.rs` builds a two-crate
fixture (`lib_a` + `app_b` with a `path = "../lib_a"` dep), runs
`index_workspace`, and asserts (1) both repos land in `entities` with
their canonical projects, (2) a `CALLS` edge exists from `app_caller`
to `lib_foo` with `src.project != dst.project`, and (3) the rendered
markdown contains the `repo:lib_a` annotation. Soft-skips when
rust-analyzer is absent.

**What Phase 2.2 explicitly does NOT do.** The roadmap names two items
this session leaves for later: per-driver moniker normalization rules
(only the hook is in place; no rewrites land today) and a
symbol-stability test suite (a snapshot of 50 known monikers across
indexer versions). Both pay back only once there's drift in practice —
add them when telemetry shows a real failure mode.

### 14.8 Phase 2.3 #1/#3 — target-level Bazel via `bazel query`

`bazel.rs` is the target-level layer of Phase 2.3: one `bazel query
--output=streamed_jsonproto 'kind(rule, //...)'` invocation enumerates
every rule target with its direct deps, and the loader writes them as
`bazel_target` entities plus `BAZEL_DEPENDS_ON` edges. No build runs;
no per-language SCIP indexer is involved. This is intentionally the
language-agnostic, deterministic, fast cut, and it fires automatically
on any Bazel workspace. The symbol-level layer (§14.9) opt-in via
`--bazel-symbols` adds per-language SCIP on top.

**Detection precedence.** `detect_bazel(repo)` checks for `WORKSPACE`,
`WORKSPACE.bazel`, or `MODULE.bazel` at the root. Phase 2.3 routes
Bazel workspaces away from the SCIP driver registry: in `index_repo`
the Bazel path takes precedence over the per-language drivers when no
`--scip` or `--lang` override is passed, because polyglot Bazel
monorepos commonly have a `Cargo.toml` / `pyproject.toml` / `package.json`
in some sub-directory for IDE integration that would otherwise mislead
the single-language drivers. `discover_workspace_repos` mirrors the
precedence so `engraph index --workspace <bazel-root>` picks the Bazel
path automatically.

**Output base placement.** Bazel keeps its analysis cache in an
`output_base` directory. Engraph defaults that to
`~/.cache/engraph/bazel-out/<sha-of-workspace-path>` so:

- The cache lives outside the workspace (Bazel placing it inside
  creates a self-referencing symlink loop that breaks subsequent
  `bazel query` invocations).
- Engraph's runs don't churn the user's own `~/.cache/bazel`.
- Re-runs against the same workspace reuse the same cache (fast).
- Override via `ENGRAPH_BAZEL_OUTPUT_BASE` (used by the live tests so
  per-test caches stay tempdir-scoped).

**Streamed JSON proto.** `bazel query --output=streamed_jsonproto`
emits one JSON object per line per target. Engraph parses with serde,
decoding only the fields it consumes (`name`, `ruleClass`, `location`,
`ruleInput`) — the full attribute list is dozens of fields per target
and would be wasted parsing. The loader skips non-`RULE` entries
(`SOURCE_FILE`, `PACKAGE_GROUP`, …).

**Edge filtering.** A target's `ruleInput` list typically contains
both first-party deps (`//foo:foo`) and Bazel-internal deps
(`@bazel_tools//tools/genrule:genrule-setup.sh`). The loader only
emits a `BAZEL_DEPENDS_ON` edge when the dep is itself a target in
the current workspace (i.e. appears as a `name` in the JSON stream).
Cross-workspace edges to `@external_repo//...` targets are skipped;
a future mode (not yet planned) would let users register external
workspaces and stitch them like Phase 2.2's cross-repo path does.

**Idempotent re-index.** Following the Phase 2.2 pattern, the Bazel
loader deletes only its own outgoing `BAZEL_DEPENDS_ON` edges
(`WHERE kind = 'BAZEL_DEPENDS_ON' AND src_entity IN (SELECT id FROM
entities WHERE project = ? AND kind = 'bazel_target')`) before
re-inserting. Entities are upserted with `ON CONFLICT(id) DO UPDATE
SET project, file_path, line_range`.

**Subgraph rendering.** `format_markdown` gives `BAZEL_DEPENDS_ON`
edges their own `**Bazel deps**` line rather than folding them into
`**References**`. Same `repo:<basename>` cross-project annotation as
v2.2 still applies if a Bazel monorepo and a downstream Cargo
workspace both live under the same `--workspace`.

**Bazel binary resolution.** The loader prefers `bazel` over
`bazelisk` (so a user with bazelisk symlinked as `bazel` gets
per-workspace version pinning via `.bazelversion`), and reports a
clear error if neither is on PATH. The companion install script
ships `bazelisk` via `go install`.

**End-to-end test.** `tests/bazel_live.rs` writes a two-genrule
fixture (no external rules), runs `index_repo`, and asserts both
targets land as `bazel_target` entities with the expected
`BAZEL_DEPENDS_ON` edge between them and repo-relative `file_path`s.
A separate test pins idempotency under re-index. Soft-skips when no
`bazel`/`bazelisk` is on PATH.

**What Phase 2.3 target-level explicitly does NOT do.**
- **Cross-workspace `@external_repo//...` edges**: today these are
  filtered. A future mode would let users register external
  workspaces and stitch them like Phase 2.2's cross-repo path does
  for non-Bazel projects.

### 14.9 Phase 2.3 #2 — symbol-level Bazel via per-language SCIP indexers

`bazel_symbols.rs` adds the symbol-level layer on top of §14.8. With
`engraph index --bazel-symbols` set (off by default), after the
target-level pass writes `bazel_target` entities and
`BAZEL_DEPENDS_ON` edges, this module drives `scip-java` / `scip-go` /
`scip-typescript` against the same Bazel workspace, merges their SCIP
outputs in memory, and loads the result under the same project key.
The end result: `engraph subgraph <name>` carries both a **Calls /
References** symbol section (from SCIP) and a **Bazel deps** section
(from `bazel query`) in one neighborhood rendering.

**Why opt-in.** Toolchain downloads and full Bazel builds make
`--bazel-symbols` heavy (5s warm to minutes cold per language). The
target-level pass remains the fast deterministic default that fires
automatically on any Bazel workspace; symbol-level is the opt-in
follow-up for when you want navigability.

**Per-language pipeline.** Three steps per language, in order:

1. **Bazel-query probe.** `bazel query 'kind(java_library, //...)
   union kind(java_binary, //...) union kind(java_test, //...)'
   --output=label_kind`, with the same `--output_base` as the
   target-level pass (so the analysis cache stays warm). Empty
   stdout → `LangStatus::SkippedNoTargets`. Non-empty → continue.
   Per-language rule classes: Java (`java_library`/`java_binary`/
   `java_test`), Go (`go_library`/`go_binary`/`go_test`), TS
   (`ts_project`/`ts_library`).
2. **Indexer binary probe.** `which scip-<lang>` (implemented as
   `Command::new(bin).arg("--version").status()`). Missing →
   `LangStatus::SkippedNoIndexer { binary }`. Present → continue.
3. **Spawn the indexer.** All three indexers run with
   `current_dir(repo)`, stdout/stderr piped. SCIP output lands at
   `~/.cache/engraph/bazel-scip-out/<sha-of-workspace-path>/index-<lang>.scip`
   (keeps it out of the user's repo and out of Bazel's own
   `output_base`; stable across runs so re-runs overwrite).

| Language | Command | Rationale |
|---|---|---|
| Java | `scip-java index --output <path>` | scip-java auto-detects Bazel via its bundled `BazelBuildTool` — materializes its own `aspects/scip_java.bzl`, runs `bazel build --aspects=...%scip_java_aspect --output_groups=scip //...`, harvests `bazel-out/**/*.scip`, merges. We do **not** reimplement aspect dispatch. |
| Go | `scip-go --module-root . --output <path>` | scip-go has no first-party Bazel aspect (per its README, Bazel is "supported via the Go Packages Driver Protocol"). MVP requires a `go.mod` at the workspace root; multi-`go.mod` Bazel-go monorepos surface as `SkippedNoTargets`. |
| TS | `scip-typescript index --output <path>` | scip-typescript has no Bazel aspect either; reads `tsconfig.json` directly. `rules_ts` users may need a prior `bazel build //...` to populate `bazel-bin/<pkg>/node_modules` symlinks — documented limitation. |

**Merge-in-memory, single load.** `scip_loader::load`'s DELETE is
per-project (§14.3). Calling it once per language under the same
project would have language 2 wipe language 1's edges, then language
3 wipe language 2's. The fix: `bazel_symbols::merge_scip_bytes`
concatenates `Index.documents` and `Index.external_symbols` vectors
across each language's SCIP output (both are `Vec<...>` in the
generated protobuf — `Vec::extend`), then reserializes. The merged
blob is loaded once. Metadata from the first non-empty input wins.

**Soft-skip semantics.** A failing language doesn't abort the rest of
the symbol-level pass. The `LangStatus` enum has four variants —
`Indexed`, `SkippedNoTargets`, `SkippedNoIndexer { binary }`,
`Failed(stderr_tail)` — and each language reports independently. The
`BazelSymbolStats` return value carries per-language results plus
aggregate `entities_inserted` / `relations_inserted` / `scip_bytes_total`.

**CLI surface.** `--bazel-symbols` on `Cmd::Index` (composes with
`--workspace`). Threaded as `bazel_symbols: bool` through
`index_repo` and `index_workspace`. On a Bazel-detected repo with
the flag set and no `--scip` override, the symbol-level pass chains
after the target-level pass; `driver_name` reports
`"bazel-query+symbols"` (vs the default `"bazel-query"`).

**End-to-end test.** `tests/bazel_symbols_live.rs` is triple-gated:
`bazel`/`bazelisk` on PATH, `scip-java` on PATH, and
`ENGRAPH_LIVE_BAZEL_SYMBOLS=1` env var set. Default `cargo test`
doesn't pay the 2–5 min cold-cache cost. Java-only by design: that's
the language whose Bazel orchestration (scip-java's bundled aspect) is
the real risk; Go and TS are simple shell-outs covered by unit tests
(`merge_scip_bytes_*`, `label_kind_nonempty_detects_lines`,
`lang_status_display_*`).

**Helpers shared with §14.8.** Three private helpers in `bazel.rs`
(`bazel_binary`, `output_base_for`, `tail_lines`) were promoted to
`pub(crate)` so this module reuses them without restructuring the
target-level module into a directory.

**Known limitations / followups** (documented in `ROADMAP.md`,
deferred until a real workload trips them):
- **scip-go multi-`go.mod` Bazel monorepos**: gazelle-managed repos
  sometimes carry one `go.mod` per package. Today the symbol-level
  path requires a single `go.mod` at the root; multi-mod would need
  per-package enumeration + merging.
- **scip-typescript + rules_ts node_modules**: cold runs may fail
  until a prior `bazel build //...` populates the package-local
  `node_modules` symlinks.
- **scip-java on large monorepos**: 1000+ Java targets can OOM or
  exceed 30 min. Reserved env var
  `ENGRAPH_BAZEL_SCIP_JAVA_TARGETS` for a future `--targets <expr>`
  pass-through, not implemented.
- **Bazel server isolation gap (Java path)**: §14.8 pins
  `--output_base` into engraph's cache, but scip-java invokes Bazel
  internally without a startup-options pass-through we can plumb.
  With `--bazel-symbols`, scip-java's Bazel build lands in the
  user's default `~/.cache/bazel`.

---

## 15. Forward pointers

The roadmap (`ROADMAP.md`) tracks features that aren't built. F2
Phase 2.3 is fully shipped (target-level §14.8, symbol-level §14.9);
the largest remaining items are:

- **Auto-trigger indexing.** Today `engraph index` is manual. Wiring
  it into the SessionStart hook (or an MCP tool surface) so a fresh
  Claude session re-indexes the workspace automatically — with
  stale-detection so cold scip-java doesn't fire every session — is
  the most user-visible follow-up.
- **Deep workspace discovery.** `discover_workspace_repos` walks
  immediate children only; nested polyrepo layouts need explicit
  per-repo invocations. Recursion + a depth-limit / `.gitignore`
  respect would close this.
- **F2 polish items deferred from 2.2**: per-driver moniker
  normalization rules (the rewrite hook in
  `scip_loader::normalize_moniker` is a no-op today) and the
  50-symbol stability test suite (snapshot known monikers, regression
  on every loader change). Both pay back only once indexer-version
  drift causes a real failure in practice.
- **MCP server** wrapping `engraph recall` / `engraph subgraph`:
  worthwhile once there are 5+ tools to expose. With F2 fully
  shipped, that threshold is met.

Everything else listed under "Shipped in the v2.1 polish pass" in the
roadmap is in this document under the corresponding feature section.
