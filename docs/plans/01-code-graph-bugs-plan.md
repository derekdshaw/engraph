# Plan 01: Code graph bugs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- A failed driver of a multi-manifest repo is in the summary, with its
  reason.
- The JavaScript test filters collapse the repeated log lines of a test run.
- The result of a wrapped command has none of the progress text a tool
  overwrote in place (`gradle --console=rich`, forced-color cargo and pip).
- **Optional:** the textual fallback name does not split inside an escaped
  descriptor.

## Condition at this time (2026-11-03)

The test projects in the `engraph-test-repos` directory showed these bugs
(refer to the `README.md` of that directory).

A corrected bug is removed from this plan, and the numbers of the other bugs do
not change. Thus this list does not start at bug 1 (refer to "Plan management"
at the end).

| Bug | Effect |
|---|---|
| 10. The textual fallback name splits inside a generic type (**optional**) | An id that the SCIP grammar rejects can give a name such as ``E>`]unwrap`` |
| 16. A failed driver of a multi-manifest repo is not in the summary | `indexed … driver=scip-typescript`, as if the repo had no Python; the reason only in a `WARN` line |
| 17. The JavaScript test filters keep the repeated log lines of a test run | 1,508 timestamped `[…-interceptor] constructing the interceptor...` lines, 100 KB of a 130 KB result |
| 18. Cursor-movement sequences leave progress fragments on one line | `gradle --console=rich`: `<-------------> 0% CONFIGURING [88ms]> root project<-------------> 6% EXECUTING …` survives, `dropped 0 progress lines` |

### Bug 10: the textual fallback name splits inside a generic type (optional)

**Found on 2026-11-11. This correction is optional.** Refer to "Reason to
wait".

`fallback_display_name_textual`
(`crates/engraph-codegraph/src/scip_loader.rs:870`) finds the descriptors with
`rsplit_once(' ')`:

```rust
moniker
    .rsplit_once(' ')
    .map(|(_, last)| {
```

This code uses the last space in the id. A SCIP id separates the scheme, the
package manager, the package name, and the package version with spaces. Thus
the last space is the correct position only when the descriptors contain no
space. But rust-analyzer writes the generic parameters of a type in an escaped
descriptor, with a comma and a space between them
(``impl#[`Result<T, E>`]unwrap().``). The space in the descriptor is one
character, because the grammar doubles a space only in the first four fields
(`accept_space_escaped_identifier`, `scip/src/symbol.rs`). Thus the split
occurs inside the type, and the name keeps the last part of the type:

| Id | Name from this function |
|---|---|
| ``…core result/impl#[`Result<T, E>`]unwrap().`` | ``E>`]unwrap`` |
| ``…core result/impl#[`Result<T, E>`][Try]branch().`` | ``E>`][Try]branch`` |
| ``…alloc vec/impl#[`Vec<T, A>`]push().`` | ``A>`]push`` |

The function also keeps `#` and `]`, as its doc comment says
(`Config#full_path_base`, ``impl#[`Pool<M>`]get``).

### Reason to wait

This function operates only when `parse_symbol` gives an error, or when the
last descriptor has an empty name (`scip_loader.rs:785` and
`scip_loader.rs:793`). An index of this repo on 2026-11-11 had 514 rows with a
descriptor character in the name. A test of `parse_symbol` with the 14 id
shapes of those rows gave a result for each shape. Thus this function was not
their cause: the cause was `ensure_placeholder_entity`, which wrote with
`INSERT OR IGNORE` and thus never corrected the name of a row that exists. The
correction of that statement removed all 514 names.

No known id gets to this function. Do this correction only to decrease the
risk if an indexer writes an id that the grammar rejects. First, make sure that
the count of incorrect names is `0`:

```sql
select count(*) from entities where name like '%#%' or name like '%]%' or name like '%`%'
```

If the count is more than `0`, this bug is not optional: find the ids that
`parse_symbol` rejects, and record them in the pull request.

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

### Bug 17: the JavaScript test filters keep the repeated log lines of a test run

**Found on 2026-11-14**, in the validation of part 15 on `hono-typescript`.

`npx vitest run` on `hono-typescript` writes 1,670 lines. 1,508 of them are
debug lines of the request interceptors its tests install, one per test
file and interceptor, equal except for the timestamp:

```
10:23:21:255 [client-request-interceptor] constructing the interceptor...
10:23:21:278 [xhr-interceptor] constructing the interceptor...
10:23:21:278 [fetch-interceptor] constructing the interceptor...
```

vitest 5's default reporter writes no `✓` line per test or file when its
output is a pipe, so the pass-line test of `js_test`
(`crates/engraph-compress/src/filters/js.rs:49-52`) and `npm::test`
(`filters/npm.rs:48`) has nothing to drop, and the filters keep every other
line. After the strip of part 15 the result is 129,883 bytes of a 157,342
byte run; the log lines are about 100 KB of it. The `ToolOutput`
preprocessor collapses only consecutive *identical* lines, and these differ
in the timestamp.

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

### Part 10: the textual fallback name (bug 10, optional)

Do this part only for the reason in "Reason to wait" of bug 10.

1. Write a unit test for `fallback_display_name_textual` with these ids and
   results. Give each id a scheme that the grammar rejects, to make the
   function operate:
   - ``x cargo core 1.0 result/impl#[`Result<T, E>`]unwrap().`` → `unwrap`
   - ``x cargo alloc 1.0 vec/impl#[`Vec<T, A>`]push().`` → `push`
   - `x cargo fd 0.1.0 config/Config#full_path_base().` → `full_path_base`
     (this result does not change)
2. Make sure that the results of the first two tests are incorrect before you
   change the code.
3. In `fallback_display_name_textual`, do not use `rsplit_once(' ')`. Find the
   end of the version field: count four fields from the start of the id. Two
   spaces together inside one of these four fields are one literal space
   (`accept_space_escaped_identifier`). Use the text after the fourth field.
4. Keep the rest of the function. It removes the trailing `().` punctuation and
   the namespace path, and it keeps `#` and `!`, because it cannot find a
   suffix without the grammar.
5. Update the doc comment of the function. Give the reason for the count of
   four fields.

Validation of part 10:

1. In this repo, do `engraph index .`. Do the count query of "Reason to wait"
   of bug 10. Result: `0`. This part changes no name of a known id.

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

### Part 17: the repeated log lines of a JavaScript test run (bug 17)

1. Find out which library writes the interceptor lines and whether a
   vitest or environment setting silences them. Record the result in the
   pull request. If a setting does, the hook can set it for the wrapped
   command; then do only step 2 as a safety net.
2. In `js_test` and `npm::test`, collapse a run of lines that are equal
   after a leading timestamp (`HH:MM:SS`, with an optional `:mmm` or
   `.mmm`) is removed, to the first line and `[engraph: + N more like it]`.
3. Write a test for each filter: three lines equal except for the
   timestamp, then one different line. The result has the first line, the
   count, and the different line.

Validation of part 17:

1. Do the preparation for `hono-typescript`, with `pnpm install` as the
   setup. Do `engraph run npx vitest run > wrapped.out 2>&1`. Result:
   `wrapped.out` is much smaller than 129,883 bytes (2026-11-14), and it
   keeps the `Test Files` and `Tests` lines.

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

## Tests

- `fallback_display_name_textual` gives the last descriptor name of an id with
  a space in a descriptor (part 10, optional).
- A failed driver of a multi-manifest repo is named in the summary with its
  reason (part 16).
- `js_test` and `npm_test` collapse lines equal except for a leading
  timestamp (part 17).
- `strip_ansi` removes the text that a cursor movement, a line erase, or a
  `\r` overwrote, and keeps the lines a terminal shows at the end (part 18).

## Risks

Part 18: a tool that moves the cursor to lay out text it means to keep (a
table that it draws in place) loses that text. The wrapped commands are
build, test, and query tools, which draw only progress in place; the
validation steps check cargo, gradle, and pnpm. The other parts have no
risk; the parts that had one are complete, and their risks are recorded in
their pull requests.

## Dependencies

None.

## Effort

Part 10 is optional and changes one function. Part 16 adds a field to
`IndexStats`, one printer, and an integration test. Part 17 changes two
functions and adds two tests, after its "Find out" step. Part 18 adds
one private function in `util.rs` behind `strip_ansi`, and its tests.

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
