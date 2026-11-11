# Plan 01: Code graph bugs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- The error message of a failed indexer shows the error, also when the
  indexer writes it to stdout.
- The hook rewrites the usual test commands of Gradle, uv, pnpm, and npx.
- A new untracked file starts `--if-changed`.
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
| 6. Indexer errors on stdout are not shown | The error message of a failed index run is empty |
| 7. The hook does not rewrite some test commands | `./gradlew test`, `uv run pytest`, and other commands are not compressed |
| 8. New untracked files do not start `--if-changed` | The graph does not include a new file until a different change occurs |
| 10. The textual fallback name splits inside a generic type (**optional**) | An id that the SCIP grammar rejects can give a name such as ``E>`]unwrap`` |

### Bug 6: indexer errors on stdout are not shown

When an indexer stops with an error, engraph shows only the end of its stderr
(`crates/engraph-codegraph/src/index.rs:572-576`):

```rust
anyhow::bail!(
    "{} exited with {}\nstderr (tail):\n{}",
    drv.name(),
    output.status,
    bazel::tail_lines(&String::from_utf8_lossy(&output.stderr), 25)
);
```

scip-typescript writes its errors to stdout (tested on `ky-typescript`:
`error TS6053: File '@sindresorhus/tsconfig' not found` is on stdout, and stderr
is empty). Thus, engraph shows `stderr (tail):` with no text after it. The same
format is in `bazel_symbols.rs:308`, `bazel_symbols.rs:375`, and
`bazel.rs:66`.

### Bug 7: the hook does not rewrite some test commands

The pre-bash hook does not rewrite these commands (tested):

| Command | Cause |
|---|---|
| `./gradlew test` | `normalize_argv0` (`crates/engraph-cli/src/rewrite.rs:259-271`) changes `./gradlew` to `gradlew` before `filters::pick`. The filter arm matches only `"./gradlew"` (`crates/engraph-compress/src/filters/mod.rs:126`). |
| `uv run pytest`, `python -m pytest` | The first word is `uv` or `python`. `filters::pick` has no arm for `("uv", "run")` or `("python", "-m")`. |
| `pnpm test` | `filters::pick` has an arm for `("npm", "test")`, but not for `("pnpm", "test")`. |
| `npx vitest run`, `npx tsc --noEmit` | The first word is `npx`. |

The hook rewrites `gradle test`, `pytest`, `vitest run`, and `tsc --noEmit`.

### Bug 8: new untracked files do not start `--if-changed`

`--if-changed` examines each subtree with
`git diff --quiet <indexed-commit> -- <subtree>` (`subtree_changed`, called from
`changed_subtrees`, `crates/engraph-cli/src/divergence.rs:160`). `git diff`
does not include untracked files. Thus, a new file that is not in `git add`
does not start a new index (tested on a small Cargo project). The recorded commit
(`git stash create`) also does not include untracked files.

`index_state` has the column `indexed_at`, the time of the index
(`crates/engraph-core/src/schema.rs:276`).

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

### Part 6: indexer errors (bug 6)

1. Write a test with a fake indexer that writes an error to stdout, writes
   nothing to stderr, and stops with exit status 1. The error message of the
   run must include the stdout text.
2. In `run_driver_to_scip`, show the end of stderr. If stderr is empty, show
   the end of stdout, with the label `stdout (tail):`.
3. Make the same change in `bazel_symbols.rs:308`, `bazel_symbols.rs:375`, and
   `bazel.rs:66`.

Validation of part 6:

1. Do the preparation for `ky-typescript`. Do not do preparation step 7. The
   setup (`npm install`) prevents the error.
2. Do `engraph index .`. Result: the command stops with an error.
3. Examine the output. Result: it has the line `stdout (tail):`, and after it
   the line `error TS6053: File '@sindresorhus/tsconfig' not found.`
4. Do `npm install --ignore-scripts`. Then do `engraph index .` again.
   Result: the last line starts with `indexed`.

### Part 7: test commands (bug 7)

1. Write rewrite tests for each command in the bug 7 table. Each command must
   become `engraph run <command>`. Write `engraph run` tests that make sure
   that each command uses the correct filter.
2. In `filters::pick`, match `gradlew` and `./gradlew`.
3. Add a function that removes a launcher before `filters::pick` selects the
   filter: `uv run`, `python -m` and `python3 -m`, `npx`, and `pnpm exec`.
   Use this function in the hook and in `engraph run`. The wrapped command
   keeps the launcher. For example, `uv run pytest` becomes
   `engraph run uv run pytest`, and `engraph run` uses the `pytest` filter.
4. In `filters::pick`, match `pnpm test` and `yarn test` with the filter of
   `npm test`.

Validation of part 7:

This table gives the command, the project, and the filter that `engraph run`
must use:

| Command | Project | Filter |
|---|---|---|
| `./gradlew test` | `jsonpath-java` | `gradle` |
| `uv run pytest` | `click-python` | `pytest` |
| `python -m pytest` | `click-python` | `pytest` |
| `pnpm test` | `hono-typescript` | `npm_test` |
| `npx tsc --noEmit` | `hono-typescript` | `tsc` |
| `npx vitest run` | `hono-typescript` | `js_test` |

1. Do the preparation for `jsonpath-java`, `click-python`, and
   `hono-typescript`. Do the setup of each project (preparation step 7). For
   `click-python`, also do `uv pip install pytest`.
2. For each row of the table, go to the copy of the project. Do this command,
   with the command of the row:
   `echo "{\"tool_input\":{\"command\":\"<command>\"},\"cwd\":\"$PWD\"}" | engraph hook pre-bash`.
   Result: the output has `"updatedInput":{"command":"engraph run <command>"}`.
3. For each row, do `engraph run <command>`.
4. After each `engraph run`, do this query:
   `q "select filter_id from events order by seq desc limit 1"`.
   Result: the filter of the row.
5. For each row, compare the output of step 3 with the output of the command
   without `engraph run`. Result: the output of step 3 is smaller, and it
   keeps the summary line of the tests.
6. Do steps 2 to 4 for `gradle test`, `pytest`, `vitest run`, and
   `tsc --noEmit`. Result: the same results as before the change.

### Part 8: untracked files (bug 8)

1. Write a test: index a repo, add a new untracked file in a subtree, and do
   `--if-changed`. The subtree must be in the result. A second `--if-changed`
   must show `index up to date`.
2. In `subtree_changed`, also do
   `git ls-files --others --exclude-standard -- <subtree>`. The subtree has a
   change if a listed file was changed after `indexed_at`.
3. Find out the time of this step on a large repo. If it is more than 100 ms,
   do it only when `git diff --quiet` finds no change.

Validation of part 8:

1. Do the preparation for `cobra-go`. Do `engraph index .`.
2. Make a new untracked file with a new function:
   `printf 'package cobra\n\nfunc UntrackedHelper() {}\n' > untracked_helper.go`
3. Do `engraph index --if-changed --dry-run .`. Result:
   `dry-run: would re-index <path of the copy>`.
4. Do `engraph index --if-changed .`. Result: the last line starts with
   `indexed <path of the copy>`.
5. Do `engraph subgraph UntrackedHelper`. Result: the first line is
   ``## Symbol `UntrackedHelper` (defined in untracked_helper.go:3)``.
6. Do `engraph index --if-changed .` again. Result: the output starts with
   `index up to date`. The untracked file does not start a second index.
7. Do the preparation for `hono-typescript`. Do `engraph index .`.
8. Do `time engraph index --if-changed .`. Result: `index up to date`. Record
   the time in the pull request (correction step 3).

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

## Tests

- A failed indexer that writes only to stdout gives an error message with the
  stdout text (part 6).
- Each command in the bug 7 table is rewritten and uses the correct filter
  (part 7).
- A new untracked file starts `--if-changed` one time (part 8).
- `fallback_display_name_textual` gives the last descriptor name of an id with
  a space in a descriptor (part 10, optional).

## Risks

None. The parts that had a risk are complete, and their risks are recorded in
their pull requests.

## Dependencies

None.

## Effort

Parts 6 and 8 each change one or two functions and add one or two tests.
Part 7 is medium: it changes the hook and `engraph run`. Part 10 is optional
and changes one function.

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
