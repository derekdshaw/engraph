# Plan 01: Code graph bugs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- A failed driver of a multi-manifest repo is in the summary, with its
  reason.
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
| 15. The JavaScript test filters do not see through ANSI color codes | A vitest run of 157 KB passes through with `dropped 0 pass lines` |
| 16. A failed driver of a multi-manifest repo is not in the summary | `indexed … driver=scip-typescript`, as if the repo had no Python; the reason only in a `WARN` line |

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

## Tests

- `fallback_display_name_textual` gives the last descriptor name of an id with
  a space in a descriptor (part 10, optional).
- `js_test` and `npm_test` drop pass lines that start with an escape sequence
  (part 15).
- A failed driver of a multi-manifest repo is named in the summary with its
  reason (part 16).

## Risks

None. The parts that had a risk are complete, and their risks are recorded in
their pull requests.

## Dependencies

None.

## Effort

Part 10 is optional and changes one function. Part 15 changes two functions
and adds two tests. Part 16 adds a field to `IndexStats`, one printer, and an
integration test.

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
