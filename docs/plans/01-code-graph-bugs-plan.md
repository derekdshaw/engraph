# Plan 01: Code graph bugs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- `engraph subgraph` and the grep redirect find Rust and Python types and
  methods by their usual names, for example `Config` and `full_path_base`.
- Python graphs have `CALLS` and `REFERENCES` relations.
- A sub-project index writes only the files of that sub-project.
- SCIP local symbols do not replace each other between files.
- The Bazel Go message shows the Go targets that engraph did not index.
- The error message of a failed indexer shows the error, also when the
  indexer writes it to stdout.
- The hook rewrites the usual test commands of Gradle, uv, pnpm, and npx.
- A new untracked file starts `--if-changed`.

## Condition at this time (2026-11-03)

The test projects in `~/r/engraph-test-repos` showed these bugs (refer to the
`README.md` of that directory). Each bug has an `engraph bug` record in the
engraph database.

| Bug | Record | Effect |
|---|---|---|
| 1. Names keep SCIP descriptor characters | `01a19e8e-2a00-7812-b3ee-aa973f0949da` | `subgraph Config` finds no Rust type |
| 2. Python graphs have no `CALLS` and no `REFERENCES` | `01a19e8e-2a68-7360-aae4-eb4f28aaa3ba` | `subgraph` shows no callers for Python |
| 3. A Gradle sub-project index writes incorrect paths | `01a19ebc-d2f3-7372-ab8f-038bcf62877e` | Files of other modules get paths that do not exist |
| 4. SCIP local symbols replace each other | `01a19ed8-9290-7061-a707-118c2738a30c` | Local symbols show the name of one file and the location of a different file |
| 5. The Bazel Go message is not correct | — | The message says that there are no Go targets, but there are |
| 6. Indexer errors on stdout are not shown | — | The error message of a failed index run is empty |
| 7. The hook does not rewrite some test commands | — | `./gradlew test`, `uv run pytest`, and other commands are not compressed |
| 8. New untracked files do not start `--if-changed` | — | The graph does not include a new file until a different change occurs |

### Bug 1: names keep SCIP descriptor characters

rust-analyzer and scip-python do not set `SymbolInformation.display_name`.
Thus, the loader makes the name from the symbol id with
`fallback_display_name` (`crates/engraph-codegraph/src/scip_loader.rs:569`):

```rust
last.trim_end_matches('.')
    .trim_end_matches(')')
    .trim_end_matches('(')
    .rsplit('/')
```

This code does not remove `#`, and it does not split at `#`. Thus:

- the type `…/Config#` gets the name `Config#`,
- the method `…/Config#full_path_base().` gets the name
  `Config#full_path_base`.

`subgraph` and the grep redirect compare the full name
(`crates/engraph-codegraph/src/subgraph.rs:110` and
`crates/engraph-cli/src/redirect.rs:128`):

```sql
WHERE name = ?1 OR id = ?1
```

Tested on `fd-rust`: `engraph subgraph Config` shows `No symbols matched.`, and
`engraph subgraph 'Config#'` shows the struct. The engraph index has the same
problem (`IndexScope`).

The problem is not only in Rust and Python. Some TypeScript and Java symbols
also have no `display_name`. Names with `#` (tested): 2,618 in
`hono-typescript` (for example `InfoStatusCode#`) and 669 in `jsonpath-java`
(for example `WriteContext#` and `JsonLateBindingValue#get`). Functions are
not affected, because their ids have no `#`.

The upsert of an entity does not change `name` when the row exists
(`scip_loader.rs:263-267`):

```sql
ON CONFLICT(id) DO UPDATE SET
   project = excluded.project,
   file_path = COALESCE(excluded.file_path, entities.file_path),
   line_range = COALESCE(excluded.line_range, entities.line_range),
   signature = COALESCE(excluded.signature, entities.signature)
```

Thus, a new index of an existing project keeps the incorrect names.

### Bug 2: Python graphs have no `CALLS` and no `REFERENCES`

On `click-python` and `requests-python`, the only relations are `IMPLEMENTS`
(267 and 64). A virtualenv does not change this result.

The loader connects each occurrence to the nearest definition above it. That
definition must have a kind that `anchor_specificity` accepts
(`scip_loader.rs:491`): a function, a method, a class, or a module. All other
kinds give 0, and the loader does not use that definition.

scip-python 0.6.8 does not set `SymbolInformation.kind`. A decode of the SCIP
index of click shows 9,371 symbols, all with kind 0 (unspecified), and none
with `display_name`. Thus, no Python definition is an anchor, and the loader
makes no `CALLS` and no `REFERENCES`.

The symbol ids contain the kind. For example,
``scip-python python click 0 `examples.aliases.aliases`/Config#add_alias().`` is
a method, and `Config#` is a type.

For the target of a relation, the loader already uses the end of the id when
the kind is not known (`suffix_kind`, `scip_loader.rs:533`). This function
classifies a parameter as a call, because the id of a parameter also ends
with `)` (for example `Config#add_alias().(self)`):

```rust
let trimmed = symbol.trim_end_matches('.');
if trimmed.ends_with(')') {
    Some(RelationKind::Calls)
```

scip-python also writes `enclosing_range` on 2,024 of 38,304 occurrences. This
plan does not use it.

### Bug 3: a Gradle sub-project index writes incorrect paths

`engraph index json-path-assert` (in `jsonpath-java`) starts scip-java in
`json-path-assert/`. Gradle also compiles the module `json-path`, because
`json-path-assert` has `implementation project(':json-path')`. scip-java
writes the documents of the two modules with paths relative to the Gradle
root, and sets `project_root` to `json-path-assert/`. engraph adds the subtree
to each path that does not start with it (`bazel_symbols.rs:663-666`). The
result is 1,981 entities with paths such as
`json-path-assert/json-path/src/main/java/…`.

A correction is on the branch `fix-subtree-sibling-docs` (not merged). It adds
`rebase_subtree_documents`, which removes the documents that are not files in
the subtree directory, but are files at the repo root.

### Bug 4: SCIP local symbols replace each other

SCIP local symbols (`local 0`, `local 1`, …) have an identity only in one
document. engraph uses the symbol id as the primary key of `entities` for all
of a project (`crates/engraph-core/src/schema.rs:123`, `id TEXT PRIMARY KEY`).
The loader writes each `SymbolInformation` of each document as an entity
(`scip_loader.rs:269`). Thus, each document writes over the `local N` rows of
the document before it. Because the upsert keeps `name` (refer to bug 1), a row
has the name from the first document, and the location and signature from the
last document.

Example from the post-read hook on `bazel_symbols.rs`:

```
- `path` @ 1009:1009 — `let got: Vec<String>`
- `root` @ 1011:1011 — `p: &PathBuf`
```

The names and the signatures are from different files.

Local symbols are not only variables. In TypeScript, many functions are local
symbols, and they are anchors for many relations. Counts from a whole-repo
index (tested):

| Project | `local` entities | Relations from a local anchor | Relations to a local symbol |
|---|---|---|---|
| `hono-typescript` | 2,818 | 3,679 of 7,432 `CALLS`; 6,849 of 12,752 `REFERENCES` | 20 `CALLS`; 1,703 `REFERENCES` |
| `jsonpath-java` | 161 | 163 `CALLS`; 116 `REFERENCES`; 63 `IMPLEMENTS` | 18 `CALLS` |
| `fd-rust` | 224 | 0 | 0 |

Thus, to remove the local symbols is not a satisfactory correction. In
`hono-typescript`, approximately half of the relations would move to an
incorrect anchor.

### Bug 5: the Bazel Go message is not correct

`go-tutorial-go-bazel` has 3 Go targets and no `go.mod`.
`engraph index --bazel-symbols .` shows
`symbol[go]: skipped (no targets in this workspace)`. The native Go pass gives
the same status when there are targets but no `go.mod`
(`crates/engraph-codegraph/src/bazel_symbols.rs:520-526`):

```rust
let modules = discover_go_modules(repo);
if modules.is_empty() {
    // go_library targets exist but no go.mod anywhere (a pure gazelle repo):
    // scip-go has no module root to index from. Reported via the targets
    // count so the gap isn't silent.
    return Ok(skip(LangStatus::SkippedNoTargets));
}
```

The comment says that the status shows the target count, but
`SkippedNoTargets` has no count. Its text is
`skipped (no targets in this workspace)` (`bazel_symbols.rs:136`).

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

## Steps

Do each part on its own branch. The parts do not depend on each other.

### Part 1: names (bug 1)

1. Write a unit test for `fallback_display_name` with these ids and results:
   - `rust-analyzer cargo fd 0.1.0 config/Config#` → `Config`
   - `rust-analyzer cargo fd 0.1.0 config/Config#full_path_base().` →
     `full_path_base`
   - ``scip-python python click 0 `examples.aliases.aliases`/Config#add_alias().``
     → `add_alias`
   - `rust-analyzer cargo engraph-core 0.1.0 schema/run_migrations().` →
     `run_migrations` (the current test)
2. In `fallback_display_name`, use `scip::symbol::parse_symbol` (crate `scip`
   0.7.1, `symbol.rs:171`). Use the name of the last descriptor. If
   `parse_symbol` gives an error, use the current method.
3. Write a test that loads the same symbol two times with different names, and
   makes sure that the second load changes the name.
4. Add `name = excluded.name` to the `ON CONFLICT` clause of the entity upsert.
   Do not change the name of a placeholder row from a definition row: the
   placeholder insert (`scip_loader.rs:461`) is a different statement.

Steps 5 to 9 decrease risk 1 (refer to "Risks").

5. Before you change the code, record the number of names with more than 8
   entities in each test project. Use this query:
   `select count(*) from (select name from entities group by name having count(*) > 8)`.
6. Add the column `qualified_name` to `entities` (schema v12). It contains the
   descriptors after the namespace, with `.` between them, for example
   `Config.full_path_base` and `Config`. Make it from `parse_symbol` in the
   same function as the name.
7. In `subgraph` (`resolve_matches`) and in the grep redirect, compare the
   input also with `qualified_name`. Before the comparison, change `::` and `#`
   in the input to `.`. Thus, `Config::full_path_base`, `Config#full_path_base`,
   and `Config.full_path_base` find the same symbol.
8. In `extract_symbol` (`crates/engraph-cli/src/redirect.rs:94-96`), keep the
   full path (for example `Config::new`), not only the last segment. Compare
   the full path with `qualified_name` first. If there is no match, compare the
   last segment with `name`.
9. In the `## Multiple matches` list of `subgraph`, show `qualified_name` and
   the file. Put the symbols in this sequence: symbols of the current project,
   symbols in files that are not tests, types before members.

### Part 2: Python relations (bug 2)

1. Write a loader test with a document in which no symbol has a kind:
   - a class `pkg/C#`,
   - a method `pkg/C#m().`, with its parameter `pkg/C#m().(self)`,
   - a function `pkg/f().`,
   - an occurrence of `pkg/f().` in the range of `pkg/C#m().`.

   The test must find the relation `CALLS` from `pkg/C#m().` to `pkg/f().`. It
   must find no relation from the parameter, and no relation to the parameter.
2. In `anchor_specificity`, if the kind is not set, use the suffix of the last
   descriptor (`parse_symbol`): method 100, type 50, namespace or package 10,
   all other suffixes 0.
3. In `suffix_kind`, use the suffix of the last descriptor in the same way:
   method gives `CALLS`, type gives `REFERENCES`, all other suffixes give no
   relation. Thus, a parameter is not a call.

### Part 3: sub-project paths (bug 3)

1. Merge the branch `fix-subtree-sibling-docs`. It contains the test
   `rebase_subtree_documents_drops_sibling_files`.
2. Do the validation in the next section.
3. Close the record: `engraph bug --resolve 01a19ebc-d2f3-7372-ab8f-038bcf62877e`.

### Part 4: local symbols (bug 4)

1. Write a loader test with two documents. Each document has `local 0` with a
   different name, and a function `pkg/f().` that has an occurrence in the
   range of `local 0`. The test must find:
   - two different entities, one for each `local 0`, each with the name and
     the file of its document,
   - a `CALLS` relation from each of the two entities to `pkg/f().`.
2. Before the load, change each local symbol id to an id that includes the
   document path: `local <n>` becomes `local <n>@<relative_path>`. Change the
   id in `SymbolInformation.symbol`, in `Occurrence.symbol`, and in
   `Relationship.symbol` of the same document. Make the name from the original
   id. Keep the prefix `local `, so that `suffix_kind` continues to skip local
   symbols without a kind.

   Do not remove local symbols. Refer to the table in bug 4: in
   `hono-typescript`, approximately half of the relations have a local anchor.
3. Write a test for the post-read list and the sibling list: a local symbol
   with no relation from it (a variable or a parameter) is not in the list. A
   local symbol with relations from it (a local function) is in the list.
4. Make that change in `entities_in_file` (post-read hook) and in the sibling
   query of `subgraph`.
5. Find out if a new index removes the old `local <n>` rows of an existing
   project. If it does not, add a migration that removes the rows with an id
   that has no `@`.

### Part 5: Bazel Go message (bug 5)

1. Write a test for the display of a new status `SkippedNoGoModules { targets }`.
   The text must include the target count and the name of the variable
   `ENGRAPH_BAZEL_SCIP_GO_CMD`. An example:
   `skipped (3 go targets, no go.mod; set ENGRAPH_BAZEL_SCIP_GO_CMD to index them)`.
2. Add the status. In `run_go_modules`, use it when `targets > 0` and there is
   no `go.mod`.

### Part 6: indexer errors (bug 6)

1. Write a test with a fake indexer that writes an error to stdout, writes
   nothing to stderr, and stops with exit status 1. The error message of the
   run must include the stdout text.
2. In `run_driver_to_scip`, show the end of stderr. If stderr is empty, show
   the end of stdout, with the label `stdout (tail):`.
3. Make the same change in `bazel_symbols.rs:308`, `bazel_symbols.rs:375`, and
   `bazel.rs:66`.

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

### Part 8: untracked files (bug 8)

1. Write a test: index a repo, add a new untracked file in a subtree, and do
   `--if-changed`. The subtree must be in the result. A second `--if-changed`
   must show `index up to date`.
2. In `subtree_changed`, also do
   `git ls-files --others --exclude-standard -- <subtree>`. The subtree has a
   change if a listed file was changed after `indexed_at`.
3. Find out the time of this step on a large repo. If it is more than 100 ms,
   do it only when `git diff --quiet` finds no change.

### End of each part

Close the `engraph bug` record of the bug with `engraph bug --resolve <id>`.
When all parts are complete, remove this plan and item 1 of the roadmap
(rule 5 of the general information).

## Tests

- `fallback_display_name` gives the name of the last descriptor (part 1).
- A second load of a symbol changes its name (part 1).
- `Config::full_path_base`, `Config#full_path_base`, and
  `Config.full_path_base` find the same symbol in `subgraph` and in the grep
  redirect (part 1).
- A document without kinds gives `CALLS` from a method to a function, and no
  relation for a parameter (part 2).
- A subtree load removes the documents of other modules
  (`rebase_subtree_documents_drops_sibling_files`, part 3).
- Two documents with the same local symbol give two entities, each with its
  own relations (part 4).
- The post-read list and the sibling list do not show local variables, but
  show local functions (part 4).
- The Go status without `go.mod` shows the target count (part 5).
- A failed indexer that writes only to stdout gives an error message with the
  stdout text (part 6).
- Each command in the bug 7 table is rewritten and uses the correct filter
  (part 7).
- A new untracked file starts `--if-changed` one time (part 8).

## Validation with the test projects

Obey the rules of `~/r/engraph-test-repos/README.md`: use a copy, make a git
baseline, and use a different database. Use the engraph binary of the branch.

- **Part 1**, on `fd-rust`, after `engraph index .`:
  - `engraph subgraph Config` shows `pub struct Config`.
  - `engraph subgraph full_path_base` shows the method.
  - The pre-grep hook stops a Grep for `Config` (F2 of the README).
  - On a database that had the old names, a new `engraph index .` changes
    `Config#` to `Config`.
  - `engraph subgraph Config::new` finds only the `new` of `Config`.
  - On `fd-rust`, `hono-typescript`, and `jsonpath-java`, compare the number
    of names with more than 8 entities with the number from step 5. Record the
    two numbers in the pull request.
- **Part 2**, on `click-python` and `requests-python`, after `engraph index .`:
  - The relations include `CALLS` and `REFERENCES`.
  - `engraph subgraph get_best_encoding` (click) shows **Called by**.
  - The counts of `CALLS` and `REFERENCES` on `fd-rust`, `cobra-go`, and
    `jsoup-java` do not change.
- **Part 3**, on `jsonpath-java`, with Gradle in `PATH`:
  - `engraph index json-path-assert` writes no path that starts with
    `json-path-assert/json-path/`.
  - After `engraph index json-path`, the counts of each module (2,963 and
    292) and the 161 relations from `json-path-assert` to `json-path` are the
    same as the counts of `engraph index .`.
  - `engraph index crates/ignore` on `ripgrep-rust` gives 2,654 entities, all
    in `crates/ignore/`.
- **Part 4**, on `hono-typescript`, `jsonpath-java`, and `fd-rust`, after
  `engraph index .`:
  - No entity has an id `local <n>` without `@`.
  - Each local entity has the file of its own document. On `fd-rust`, the
    post-read hook on `src/main.rs` shows no `let` lines with a name from a
    different file.
  - The counts of `CALLS` and `REFERENCES` are approximately the same as the
    counts in the bug 4 table. They can increase a small quantity, because
    relations to different local symbols no longer use one row.
- **Part 5**, on `go-tutorial-go-bazel`: `engraph index --bazel-symbols .`
  shows `symbol[go]: skipped (3 go targets, no go.mod; …)`.
- **Part 6**, on `ky-typescript` without `npm install`: `engraph index .` shows
  `stdout (tail):` and `error TS6053: File '@sindresorhus/tsconfig' not found`.
- **Part 7**: do F3 and F5 of the test project README with each command in the
  bug 7 table. Use `jsonpath-java` for `./gradlew test`, `click-python` for
  `uv run pytest`, and `hono-typescript` for `pnpm test`.
- **Part 8**, on `cobra-go`: do F6 of the README, but add a new untracked file
  with a new function. `--if-changed --dry-run` shows the project, and after
  the index, `engraph subgraph <new-function>` finds the function.

## Risks

- **Part 1: more matches for a name.** More symbols have short names. A name
  such as `new` then matches more than 8 symbols. The grep redirect then lets
  the search continue, and `subgraph` shows `## Multiple matches` more
  frequently. An estimate from the current graphs (a simple parser, not
  `parse_symbol`) of the names with more than 8 entities: `fd-rust` 2 → 2,
  `hono-typescript` 29 → 54, `jsonpath-java` 12 → 41.

  Mitigation (part 1, steps 5 to 9):
  - The qualified name (`Config.new`) stays available. A search for
    `Config::new` finds one symbol, also when `new` has many matches. Today,
    `extract_symbol` removes `Config::` and searches only for `new`.
  - The `## Multiple matches` list shows qualified names in a useful sequence.
    Thus, the agent can select the correct symbol in one more step.
  - The pull request records the numbers before and after the change.
- **Part 2:** Python graphs become larger. The index time and the size of the
  database increase.
- **Part 4: more entities.** Each document keeps its own local symbols.
  Thus, the number of `local` entities increases. Each document that uses
  `local 0` adds one row. The database becomes larger.

  Mitigation (part 4, steps 3 and 4):
  - The relations do not change. The local symbols stay, so no relation moves
    to an incorrect anchor. This is the reason not to remove local symbols.
  - The post-read list and the sibling list do not show local variables and
    parameters. Thus, the additional rows add no tokens to the agent context.
  - The validation of part 4 records the number of `local` entities before
    and after the change.

## Dependencies

None. Part 3 depends on the branch `fix-subtree-sibling-docs`.

## Effort

Parts 2, 5, 6, and 8 each change one or two functions and add one or two
tests. Part 3 is complete except for the merge. Part 1 is the largest: it adds
a schema version and changes `subgraph` and the grep redirect. Parts 4 and 7
are medium: part 4 changes the loader and two lists, and part 7 changes the
hook and `engraph run`.
