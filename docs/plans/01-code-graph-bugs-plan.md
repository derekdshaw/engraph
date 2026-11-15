# Plan 01: Code graph bugs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- A failed driver of a multi-manifest repo is in the summary, with its
  reason.
- The result of a wrapped command collapses log lines that repeat with only
  their timestamp changed, for every filter.
- The result of a wrapped command has none of the progress text a tool
  overwrote in place (`gradle --console=rich`, forced-color cargo and pip).
- An event-stream result (a test, build, lint, or log command) collapses
  identical repeated lines; a content result (a file read, a diff, a search)
  is never collapsed, not even its timestamped lines.

## Condition at this time (2026-11-03)

The test projects in the `engraph-test-repos` directory showed these bugs
(refer to the `README.md` of that directory).

A corrected bug is removed from this plan, and the numbers of the other bugs do
not change. Thus this list does not start at bug 1 (refer to "Plan management"
at the end).

| Bug | Effect |
|---|---|
| 16. A failed driver of a multi-manifest repo is not in the summary | `indexed … driver=scip-typescript`, as if the repo had no Python; the reason only in a `WARN` line |
| 17. Log lines that repeat with only their timestamp changed are all kept | With `DEBUG=1` in the environment, 1,508 `[…-interceptor] constructing the interceptor...` lines, 100 KB of a 130 KB vitest result |
| 18. Cursor-movement sequences leave progress fragments on one line | `gradle --console=rich`: `<-------------> 0% CONFIGURING [88ms]> root project<-------------> 6% EXECUTING …` survives, `dropped 0 progress lines` |
| 19. Identical repeated lines survive, and content can be collapsed | 290 copies each of 7 stack-frame lines (672 KB); and `cat` of a log fixture would lose lines to the part 17 collapse |

### Bug 16: a failed driver of a multi-manifest repo is not in the summary

**Found on 2026-11-14**, in the review of part 13.

`index_repo` runs every driver whose manifest the repo root has. When one
driver fails, the run continues with the other drivers, and the reason goes
to a `WARN` line only (`crates/engraph-codegraph/src/index.rs:402-406`):

```rust
tracing::warn!(
    driver = drv.name(),
    error = %e,
    "driver failed; continuing with remaining drivers for this repo",
);
```

`IndexStats` has no field for a failed driver, and `driver_name` has only
the drivers that succeeded: one name, or `multi` (`index.rs:429-433`). Thus,
when scip-python fails and scip-typescript succeeds, the summary is
`indexed … driver=scip-typescript` — the same as for a repo with no Python —
and a detached SessionStart refresh drops a language from the graph with no
sign in the summary. The Bazel symbol pass reports each language on its own
`symbol[…]` line; the driver pass reports only the survivors. When all
drivers fail, the error `all N driver(s) failed` has no reasons
(`index.rs:409-414`).

### Bug 17: log lines that repeat with only their timestamp changed are all kept

**Found on 2026-11-14**, in the validation of part 15 on `hono-typescript`.
**Corrected on 2026-11-14** after the "find out" step of part 17.

`npx vitest run` on `hono-typescript` wrote 1,670 lines, 1,508 of them
debug lines of the request interceptors its tests install, equal except for
the timestamp:

```
10:23:21:255 [client-request-interceptor] constructing the interceptor...
10:23:21:278 [xhr-interceptor] constructing the interceptor...
10:23:21:278 [fetch-interceptor] constructing the interceptor...
```

The "find out" step showed their cause: msw writes them through
`@open-draft/logger`, which writes only when the `DEBUG` environment
variable is set (`@open-draft/logger/lib/index.js:59`), and the shell of the
validation had `DEBUG=1`. Without `DEBUG`, the same run writes 10,140 bytes
in 162 lines, with no interceptor line. Thus the size of the first report
was an effect of the environment of the test, not of hono or of the
filters. `engraph run` must not remove `DEBUG` from the environment of the
command: that changes what the user's command does.

The pattern is real all the same: application code that logs in each test
writes the same line with a new timestamp, under any test runner. No filter
collapses it. The `ToolOutput` preprocessor collapses only consecutive
*identical* lines, and `engraph run` does not use it. With `DEBUG=1` the
vitest result after part 15 was 129,883 bytes, about 100 KB of it these
lines.

### Bug 18: cursor-movement sequences leave progress fragments on one line

**Found on 2026-11-14**, in the validation of part 15 with color forced on.

`engraph run` removes every CSI escape sequence before the filter
(`util::strip_ansi`, `crates/engraph-compress/src/filters/util.rs:11`; part
15). A rich console does not write a progress message per line: it writes
the message, then moves the cursor back and overwrites it. Gradle with
`--console=rich` wrote this (one line of `./gradlew test` on
`jsonpath-java`, control sequences shown):

```
ESC[2A ESC[1m < ESC[0;1m -------------> 0% CONFIGURING [109ms] ESC[m ESC[38D ESC[1B ESC[1m > root project ESC[m ESC[14D ESC[1B ESC[2A …
```

`ESC[2A` (cursor up), `ESC[38D` (cursor left), `ESC[1B` (cursor down),
`ESC[0K` and `ESC[2K` (erase line) position the next fragment over the last
one. The strip removes the sequences and keeps the text, so the fragments
join on one line:

```
<-------------> 0% CONFIGURING [88ms]> root project<-------------> 6% EXECUTING [188ms]> :json-path:processResources
```

The gradle filter drops a line that matches `^> Task :|^Downloading https?://`
(`crates/engraph-compress/src/filters/jvm.rs:32`). The joined line matches
neither, so with `--console=rich` the filter reports
`dropped 0 progress lines` and the result is 714 bytes, against 497 bytes
and `dropped 15` with the plain console. The escapes themselves are
removed (0 remain). A user sets `--console=rich` in `gradle.properties`
(`org.gradle.console=rich`) or on the command line; the hook then wraps the
command as given. Every tool that draws progress in place (cargo's and
pip's progress bars, Bazel's UI) writes the same shapes when color is
forced.

### Bug 19: identical repeated lines survive, and content can be collapsed

**Found on 2026-11-14**, in the validation of part 17 on `hono-typescript`.

Two halves, one boundary.

**Identical lines are not collapsed.** A vitest run that hit a coverage
error 145 times wrote 682,214 bytes in 4,084 lines. Each error block
repeats the same 7 stack-frame lines:

```
 ❯ writeCoverageFile node_modules/.pnpm/@vitest+coverage-v8@5.0.1_…/dist/commands-wCiafWY1.js:…
 ❯ TestRunner.onAfterRunFiles node_modules/…
```

290 copies of each survived: the lines have no timestamp, so part 17's
collapse does not key them, and the repeats are a block apart, so a
consecutive-identical rule (`dedup_consecutive`, which only `compress()`
uses) would not catch them either.

**Content must never be collapsed.** An identical line is noise in an
*event stream* (a test, build, lint, or log run writes it again and again)
but *data* in content. `engraph run cat src/parser.py` flows through the
same path, and a source file repeats lines legitimately (`});`,
`    return None`); a diff repeats the same added line in two hunks; `rg`
repeats the same match text. Worse, the part 17 collapse is unconditional
today, so `engraph run cat test/fixtures/app.log` — a fixture of
timestamped lines — would lose lines.

The boundary is the filter, and it is declared where the filter is chosen,
not in a second list. `pick` returns a descriptor instead of a tuple:

```rust
/// What a command's output is. Decides the generic passes `engraph run`
/// applies before the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// A test, build, lint, install, or log run. Lines are events: an
    /// identical repeated line is noise, as is one that repeats with only
    /// its timestamp changed.
    Event,
    /// A file read, a diff, search matches, a listing, a table. Every
    /// line is data; nothing is collapsed.
    Content,
}

#[derive(Debug, Clone, Copy)]
pub struct Filter {
    pub id: &'static str,
    pub kind: OutputKind,
    pub run: FilterFn,
}
```

Each `pick` arm declares the kind beside the command it classifies, so a
new filter cannot be added without the decision, and `cat` is protected by
construction — the collapse never runs for `Content`:

```rust
("cargo", "test")           => Filter::event("cargo_test", cargo::test),
("pytest", _)               => Filter::event("pytest", python::pytest),
("docker", "logs")          => Filter::event("docker_logs", docker::logs),
("rg" | "grep", _)          => Filter::content("rg", search::rg),
("cat" | "bat" | "less", _) => Filter::content("read_cat", read::cat),
_                           => Filter::content("generic", generic::filter),
```

```rust
// engraph run, after strip_ansi:
let filter = filters::pick(&command, &args);
let stdout = match filter.kind {
    OutputKind::Event => filters::util::collapse_repeated_lines(&stdout),
    OutputKind::Content => stdout,
};
let result = (filter.run)(&FilterCtx { .. });
```

The same gate corrects part 17: its timestamped collapse must not run for
content either.

`is_display_sink` (`crates/engraph-cli/src/rewrite.rs`) stays where it is.
It looks like the same list, but it answers a different question about a
different role: not "what is this command's *output*" (producer) but "is
this command a safe pipeline *target* for filtered bytes" (consumer).
`cat` is `Content` as a producer and a display sink as a consumer at the
same time; `rg` is `Content` but **not** a sink (it computes over its
input), which shows the two axes are independent and cannot be one field.
The `kind` field is deliberately the only axis the struct carries.

## Steps

Do each part on its own branch. The parts do not depend on each other. Each
part has correction steps and validation steps. Do the validation steps after
the correction steps, before the pull request.

### Preparation for a validation

The validation steps use the test projects in the `engraph-test-repos`
directory. Do this procedure for each project that a validation step names.
In these steps:

- `<project>` is the directory name of the project, for example `fd-rust`.
- `$TEST_REPOS` is the path of the `engraph-test-repos` directory.
- `$WORK` is a work directory for the copies and the databases.

1. In the engraph repo, on the branch of the part, build the binary:
   `cargo build -p engraph-cli`
2. Put the binary first in `PATH`: `export PATH="$PWD/target/debug:$PATH"`
3. Use a new database for the project:
   `export ENGRAPH_DB_PATH="$WORK/<project>.db" && rm -f "$ENGRAPH_DB_PATH"`
4. Make a new copy of the project:
   `mkdir -p "$WORK" && rm -rf "$WORK/<project>" && cp -r "$TEST_REPOS/<project>" "$WORK/"`
5. Go to the copy: `cd "$WORK/<project>"`
6. Make a git baseline:
   `git init -q && git add -A && git -c user.name=t -c user.email=t@t commit -qm baseline`
7. Do the setup that the section of the project in `$TEST_REPOS/README.md`
   gives. Some projects have no setup.
8. Define the query function `q`. It shows the result of an SQL query on the
   database:

   ```sh
   q() { python3 -c 'import sqlite3,os,sys; [print(*r) for r in sqlite3.connect(os.environ["ENGRAPH_DB_PATH"]).execute(sys.argv[1])]' "$1"; }
   ```

Some validation steps record a value with the `main` binary before the change.
For these steps, do steps 1 and 2 on the `main` branch, and then steps 3 to 8.

### Part 16: the failed drivers of a multi-manifest repo (bug 16)

1. Add the failed drivers to `IndexStats`: the driver name and the failure
   message. The message already carries the `failure_tail` of the indexer.
2. Print one `driver[<name>]: failed: …` line per failed driver under the
   repo summary, the tail indented — the shape of `format_symbol_langs`
   (part 11). `--workspace` and the single-repo path use the same printer.
3. Put the per-driver reasons in the `all N driver(s) failed` error.
4. Write an integration test: a repo with `pyproject.toml` and
   `package.json` + `tsconfig.json`, and fake `scip-python` and
   `scip-typescript` binaries first in the `PATH` of the spawned `engraph`
   process. One writes a small SCIP index; the other exits 2 with text on
   stderr. The summary must name the failed driver and show the text.

Validation of part 16:

1. The integration test covers the behavior. No test project has two
   manifests with one broken toolchain; a manual check can copy
   `hono-typescript`, add a `pyproject.toml`, and put a failing
   `scip-python` shim first in `PATH`.

### Part 17: repeated log lines in `engraph run` (bug 17)

The correction knows no library, test runner, or language: a line that
starts with a timestamp and repeats with only the timestamp changed is a
repeated log line. It runs once in `engraph run`, after `strip_ansi` and
before the filter, so every filter gets it (the shape of part 15).

1. Write tests for a new `util::collapse_repeated_lines`:
   - three lines equal except for a leading timestamp, then one different
     line: the result has the first line, then
     `[engraph: + 2 more lines like the one above]`, then the different
     line;
   - the timestamp shapes `10:23:21:255`, `10:23:21.255`, `2026-11-14
     10:23:21`, `2026-11-14T10:23:21.255Z`, and `[10:23:21]` all count;
   - timestamped lines with different text, and lines with no timestamp,
     are unchanged, also when equal;
   - text with no repeated line is returned borrowed, unchanged.
2. Make sure that the results of the tests are incorrect before you change
   the code.
3. Write `util::collapse_repeated_lines`: the key of a line is its text
   after a leading timestamp; the first line of a key stays, the next lines
   of the key go, and one `[engraph: + N more lines like the one above]`
   follows the first line. Kept lines do not change, so a filter's prefix
   test sees them as before. Lines with no leading timestamp are not keys.
4. Use it in `engraph run` after `strip_ansi`. Change no filter.
5. Write an `engraph run` integration test with a fake command that writes
   three repeated timestamped lines and a summary line. The result has one
   of the lines, the count, and the summary.

Validation of part 17:

1. Do the preparation for `hono-typescript`, with `pnpm install` as the
   setup. Do `DEBUG=1 engraph run npx vitest run > wrapped.out 2>&1` (the
   `DEBUG=1` makes the interceptor lines; without it there is nothing to
   collapse). Result: `wrapped.out` is much smaller than 129,883 bytes
   (2026-11-14), has each interceptor line one time with a count after it,
   and keeps the `Test Files` and `Tests` lines. Record the size in the pull
   request.
2. Do `engraph run npx vitest run` without `DEBUG`. Result: the same size
   as before the change (10,140 bytes on 2026-11-14, plus the filter's
   summary line).

### Part 18: overwritten text in `strip_ansi` (bug 18)

A progress bar is text that the tool overwrites: a `\r`, a cursor movement,
or an erase sequence puts the next write over it, and what a terminal shows
at the end has none of it. The correction acts on the control sequences,
not on the text, so it needs no knowledge of a tool. It goes inside
`strip_ansi`, which stays the one public function: every caller — `engraph
run` (part 15), the git, search, and bazel filters, and
`preprocess::tool_output` — gets it with no call-site change. The collapse
must run before the sequences are removed: the sequences are the evidence
of the overwrite.

1. Write tests for `strip_ansi`:
   - the gradle line of bug 18 gives only the text a terminal shows at the
     end: no `<---` fragment, and the lines after it (`Deprecated Gradle
     features…`, `BUILD SUCCESSFUL in 1s`, `9 actionable tasks: 9
     up-to-date`) unchanged;
   - `a\rb\n` gives `b`; `line ESC[0K\n` gives `line` (an erase to the end
     of the line keeps the text before it); `x ESC[2K y\n` gives `y` (an
     erase of the whole line removes the text before it); `one ESC[1A two\n`
     gives `two`;
   - text with only SGR sequences gives the text without them; plain text is
     unchanged.
2. Make sure that the results of the tests are incorrect before you change
   the code (`strip_ansi` keeps the fragments).
3. Add a private `collapse_overwrites` in `util.rs`, and call it from
   `strip_ansi` before the sequence removal:
   - a segment of text that a cursor movement (`ESC[…A` to `ESC[…H`), a
     whole-line erase (`ESC[1K`, `ESC[2K`), or a `\r` with more text after
     it follows is overwritten: remove it;
   - a segment that `\n` ends is a line: keep it (`ESC[K` or `ESC[0K`
     before the `\n` keeps the text);
   - collapse the empty lines this makes.
   Keep the fast path: text with no `ESC` and no `\r` is returned borrowed,
   unchanged.
4. Remove the `\r` rule that `drop_progress_lines`
   (`crates/engraph-compress/src/preprocess.rs:49-55`) now duplicates — it
   runs after `strip_ansi`, which leaves no `\r` overwrite to handle — and
   update any golden snapshot whose fixture has overwritten text. Change no
   filter and no call site.

Validation of part 18:

1. Do the preparation for `jsonpath-java`, and do `./gradlew test` one time
   so the build is warm. Do `engraph run ./gradlew test --console=rich` and
   `engraph run ./gradlew test`. Result: no `<---` fragment and no escape in
   either result, `BUILD SUCCESSFUL` in both, and the rich result is not
   larger than the plain result. Record both sizes in the pull request
   (2026-11-14, before the change: 714 and 497 bytes).
2. In this repo, do `touch crates/engraph-core/src/lib.rs`, then
   `engraph run cargo build --color always`. Result: each `Compiling` line
   is on its own line, and no progress bar text (`[===>    ] 3/9`) is in the
   result.
3. In the copy of `hono-typescript`, do `engraph run pnpm install` (pnpm
   redraws its progress in place). Result: no joined progress fragments.

### Part 19: identical repeated lines, for event streams only (bug 19)

1. Write tests:
   - `pick("pytest", …).kind` and `pick("gradle", …).kind` are
     `OutputKind::Event`; `pick("cat", …)`, `pick("git", ["diff"])`,
     `pick("rg", …)`, and an unknown command (`generic`) are
     `OutputKind::Content`;
   - `collapse_repeated_lines`: a line with no timestamp that repeats 3 or
     more times collapses to the first and
     `[engraph: + N more lines like the one above]`, also when the repeats
     are apart; 2 repeats stay (the threshold keeps a coincidental repeat
     of real output); a blank line never collapses; the timestamped rule of
     part 17 is unchanged;
   - an `engraph run` integration test: `cat` of a fixture whose lines
     repeat — with and without timestamps — returns the content unchanged,
     byte for byte, and a fake test runner with 7 frame lines repeated 3
     times collapses them.
2. Make sure that the results are incorrect before you change the code.
3. Add `OutputKind` and `Filter { id, kind, run }` to `filters/mod.rs`, and
   make `pick` return `Filter`: every arm declares `Filter::event(…)` or
   `Filter::content(…)`, and the fallback is
   `Filter::content("generic", …)`, so an unknown command can never be
   collapsed. The call sites (`main.rs`, `rewrite.rs`, the tests) take
   `.id` and `.run` in place of the tuple.
4. Gate the whole collapse on `filter.kind` in `engraph run` — the
   timestamped rule of part 17 too.
5. In `collapse_repeated_lines`, also key a line with no timestamp by its
   full trimmed text, and collapse it only when its count is 3 or more and
   it is not blank.
6. Record in the pull request the kind of every arm, and why `docker_logs`
   and `kubectl_logs` are `Event` (a repeated identical log line is an
   event; the count carries the information).
7. **Optional follow-on, its own change:** `FilterOutput.filter_id`
   repeats `Filter.id` in 46 filter functions, guarded by
   `picker_and_filter_output_agree_on_filter_id`
   (`crates/engraph-compress/tests/filter_ratios.rs:566`). With the id on
   the descriptor, `engraph run` does not need the stamp: the field and the
   agreement test can retire.

Validation of part 19:

1. The integration tests cover the behavior. As a real check: in a copy of
   `hono-typescript`, make vitest fail its coverage write 145 times (remove
   the `coverage` directory while the run starts, as the 2026-11-14 run
   did), or use the fake-runner test; and do
   `engraph run cat src/index.ts` and compare with `cat src/index.ts` —
   identical, apart from the read filter's own windowing.

## Tests

- A failed driver of a multi-manifest repo is named in the summary with its
  reason (part 16).
- `collapse_repeated_lines` keeps the first of lines equal except for a
  leading timestamp and counts the rest; `engraph run` applies it before
  every filter (part 17).
- `strip_ansi` removes the text that a cursor movement, a line erase, or a
  `\r` overwrote, and keeps the lines a terminal shows at the end (part 18).
- Every `pick` arm declares an `OutputKind`; `Content` filters (`read_cat`,
  `git_diff`, `rg`, `generic`) get their text untouched, and an `Event`
  filter collapses identical lines that repeat 3 or more times (part 19).

## Risks

Part 18: a tool that moves the cursor to lay out text it means to keep (a
table that it draws in place) loses that text. The wrapped commands are
build, test, and query tools, which draw only progress in place; the
validation steps check cargo, gradle, and pnpm. Part 19: the
kind of each arm is judgment; an arm wrongly declared `Event` would
collapse real repeated output — the threshold of 3 and the
first-line-plus-count shape keep the information, and the count line says
that lines were folded. The other parts have no
risk; the parts that had one are complete, and their risks are recorded in
their pull requests.

## Dependencies

None.

## Effort

Part 16 adds a field to
`IndexStats`, one printer, and an integration test. Part 17 adds one
function in `util.rs`, one call in `engraph run`, and its tests. Part 18 adds
one private function in `util.rs` behind `strip_ansi`, and its tests. Part 19
adds `OutputKind` and the `Filter` struct in `filters/mod.rs`, a kind on
every `pick` arm, one gate in `engraph run`, one rule in
`collapse_repeated_lines`, and its tests.

## Plan management

This plan has independent parts, and each part is removed when it is complete.
These tasks replace rule 5 of the general information:

1. When the validation of a part passes, remove its bug and its part from this
   plan: the bug section, the part section, the table row, and the bullets in
   "Result", "Tests", and "Risks". Record the validation data in the pull
   request. The numbers of the other bugs do not change.
2. When all parts that are not optional are complete, remove item 1 from
   `docs/ROADMAP.md`, describe the new functions in `DETAILS.md`, and move this
   plan to `docs/plans/completed/`. An optional part that is not done moves
   with the plan.
