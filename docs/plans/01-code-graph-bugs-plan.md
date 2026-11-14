# Plan 01: Code graph bugs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- When a configured command does not run because the count of targets is 0,
  the symbol pass says so.
- The warning for a failed scip-go module shows the error, also when scip-go
  writes it to stdout.
- A load removes the local symbols of a document that has no global symbol.
- The JavaScript test filters drop the pass lines of a colored vitest run.
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
| 12. A configured command does not run for rule classes that engraph does not count | `skipped (no targets in this workspace)`, also when `ENGRAPH_BAZEL_SCIP_JAVA_CMD` is set |
| 13. A failed scip-go module shows only its stderr, and only in the log | The warning is empty when scip-go writes the error to stdout |
| 14. Local symbols of a document with no global symbol are not removed | The rows stay after the file changes or is deleted |
| 15. The JavaScript test filters do not see through ANSI color codes | A vitest run of 157 KB passes through with `dropped 0 pass lines` |

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

### Bug 12: a configured command does not run for rule classes that engraph does not count

**Found on 2026-11-11**, in the review of pull request #20. It was not seen on
a real repo.

`run_delegated` counts the targets of `spec.rule_kinds` before it reads the
env var of the command (`crates/engraph-codegraph/src/bazel_symbols.rs:364-367`):

```rust
let targets = bazel_count_targets(bazel, repo, spec.rule_kinds)?;
if targets == 0 {
    return Ok(skip(LangStatus::SkippedNoTargets));
}
```

The rule kinds of Java are `java_library`, `java_binary`, and `java_test`. If
a workspace builds its Java with other rule classes, for example a custom
Starlark rule, the count is 0. Then the pass shows
`skipped (no targets in this workspace)`, and it does not run the command of
`ENGRAPH_BAZEL_SCIP_JAVA_CMD`, also when the user set it. The native Go pass
and the Go command have the same count.

The count before the command is necessary. The README tells the user to export
the variable, and without the count, a `--workspace` run starts the Java command
in each repo that has no Java. Thus the correction keeps the count, and changes
the message.

### Bug 13: a failed scip-go module shows only its stderr, and only in the log

`run_go_modules` continues when one module fails
(`bazel_symbols.rs:587-594`):

```rust
if !output.status.success() {
    failed += 1;
    tracing::warn!(
        module = %module_root.display(),
        "scip-go failed (isolated): {}",
        tail_lines(&String::from_utf8_lossy(&output.stderr), 20)
    );
    continue;
}
```

The status shows only the count, for example
`indexed 2 go.mod modules of 9 go targets, 1 failed`. The reason is in a
`WARN` line, and that line has only stderr. Part 6 changed the other failure
sites to `failure_tail` (stderr, or stdout when stderr is empty), but not this
one.

### Bug 14: local symbols of a document with no global symbol are not removed

**Found on 2026-11-09**, in the review of plan 01 part 4 (pull request #17).

A local symbol has no scheme in its id. The loader gives it the scheme of its
document, from the first global symbol of the document
(`crates/engraph-codegraph/src/scip_loader.rs:614-620`). A document with no
global symbol in its symbols and its occurrences has no scheme, so its locals
get `scheme` NULL.

A load removes a local only when its scheme is a scheme of the load
(`scip_loader.rs:63`):

```rust
OR (id LIKE 'local %' AND scheme IN (SELECT s FROM current_scip_schemes)))
```

NULL is never `IN` a list. Thus a load never removes these locals, also not
after the file is deleted. The 8 test projects had no such row (2026-11-09).

### Bug 15: the JavaScript test filters do not see through ANSI color codes

**Found on 2026-11-13**, in the validation of part 7 on `hono-typescript`.

`js_test` drops a pass line when the line starts with `✓`, `PASS `, or `ok `
(`crates/engraph-compress/src/filters/js.rs:49-52`):

```rust
if t.starts_with('\u{2713}')
    || t.starts_with('✓')
    || t.starts_with("PASS ")
    || t.starts_with("ok ")
```

`npm::test` has the same test (`crates/engraph-compress/src/filters/npm.rs:48`).
vitest writes color codes also when its output goes to a pipe: `npx vitest
run` wrote 1,508 lines with an escape sequence. A pass line starts with
`ESC[32m✓`, not with `✓`. Thus the test fails for each pass line, and the
filter keeps all of them. The wrapped output was 157,472 bytes, the raw
output 157,425 bytes, and the last line was
`[engraph: js_test dropped 0 pass lines, exit 1]`. `pnpm test` gave the same
result with `npm_test` (157,542 bytes).

`util::strip_ansi` removes escape sequences
(`crates/engraph-compress/src/filters/util.rs:11`). The bazel and git filters
use it before their line scan. The two JavaScript test filters do not.

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

### Part 12: the skip message of a command that does not run (bug 12)

1. Find out if a real Bazel repo builds Java or Go with rule classes that are
   not in `rule_kinds`. Record the repo and its rule classes in the pull
   request.
2. Write a test with a fake `bazel` that reports 0 targets, and with
   `ENGRAPH_BAZEL_SCIP_JAVA_CMD` set. Hold `cmd_env_lock` in the test. The
   status must name the rule kinds and the variable, for example
   `skipped (no java_library, java_binary, java_test targets; ENGRAPH_BAZEL_SCIP_JAVA_CMD not run)`.
3. In `run_delegated`, when the count is 0 and the variable is set, give that
   status. When the variable is not set, keep `SkippedNoTargets`.
4. If step 1 found a repo, add its rule classes to `rule_kinds`.

Validation of part 12:

1. Do the preparation for `go-tutorial-go-bazel`. Do
   `ENGRAPH_BAZEL_SCIP_JAVA_CMD=true engraph index --bazel-symbols .`.
   Result: `symbol[java]: skipped (no java_library, java_binary, java_test targets; ENGRAPH_BAZEL_SCIP_JAVA_CMD not run)`.
2. Do the command again without the variable. Result:
   `symbol[java]: skipped (no targets in this workspace)`.

### Part 13: the scip-go module error (bug 13)

1. Find out where scip-go writes the error for a module that does not compile:
   stdout or stderr. Record the result in the pull request.
2. A test needs a fake `scip-go` in `PATH`, and `PATH` is process-global. If
   a test is not practical, record the reason in the pull request.
3. In `run_go_modules`, use `failure_tail(&output.stderr, &output.stdout, 20)`
   in the warning.

Validation of part 13:

1. Do the preparation for `go-tutorial-go-bazel`. In the copy, make a module
   that does not compile: `bad/go.mod` with `module example.com/bad`, and
   `bad/bad.go` with `package bad` and `func (`.
2. Do `engraph index --bazel-symbols .`. Result: the `symbol[go]:` line ends
   with `1 failed`, and a `WARN` line for the module `bad` shows the error of
   scip-go after `stderr (tail):` or `stdout (tail):`.

### Part 14: locals of a document with no global symbol (bug 14)

1. Write a loader test: load an index with a document that has only local
   symbols. Load the index again without that document. The locals of the
   document must not be in `entities`.
2. When a document has no global symbol, give its locals the scheme of the
   load, if the load has only one scheme. If the load has more than one
   scheme, record in the pull request how the correction selects one.

Validation of part 14:

1. No test project has such a document. Do `./et.sh smoke all` with the
   binary of `main` and with the binary of the branch. Result: the count of
   `local` entities of each project is the same.

### Part 15: color codes in the JavaScript test filters (bug 15)

1. Write a test for `js_test` and a test for `npm::test`. The input has pass
   lines that start with `\x1b[32m✓\x1b[39m` and one failure line. Each
   filter must drop the pass lines, keep the failure line, and report the
   count of dropped lines.
2. Make sure that the results of the tests are incorrect before you change
   the code.
3. In `js_test` and `npm::test`, apply `strip_ansi` to the combined text
   before the line scan.

Validation of part 15:

1. Do the preparation for `hono-typescript`, with `pnpm install` as the setup.
2. Do `npx vitest run > raw.out 2>&1` and
   `engraph run npx vitest run > wrapped.out 2>&1`. Result: `wrapped.out` is
   smaller than `raw.out`, its last line shows a dropped count that is more
   than 0, and it keeps the `Test Files` and `Tests` summary lines. Record
   both sizes in the pull request. The sizes on 2026-11-13, before the
   change, were 157,425 bytes raw and 157,472 bytes wrapped.
3. Do the same with `pnpm test`. Result: the same, with the `npm_test` filter.

## Tests

- `fallback_display_name_textual` gives the last descriptor name of an id with
  a space in a descriptor (part 10, optional).
- A configured command with 0 targets gives a status that names the rule
  kinds and the variable (part 12).
- A second load without a document removes the locals of that document, also
  when the document has no global symbol (part 14).
- `js_test` and `npm_test` drop pass lines that start with an escape sequence
  (part 15).

## Risks

None. The parts that had a risk are complete, and their risks are recorded in
their pull requests.

## Dependencies

None.

## Effort

Part 10 is optional
and changes one function.
Parts 12, 13, and 14 each change one function and add one test, after the
"Find out" steps. Part 15 changes two functions and adds two tests.

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
