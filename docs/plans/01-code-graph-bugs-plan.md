# Plan 01: Code graph bugs

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work and the sequence
of the plans.

## Result

- **Complete (part 1, 2026-11-03):** `engraph subgraph` and the grep redirect
  find Rust and Python types and methods by their usual names, for example
  `Config` and `full_path_base`.
- **Complete (part 2, 2026-11-08):** Python graphs have `CALLS` and
  `REFERENCES` relations.
- A sub-project index writes only the files of that sub-project.
- **Complete (part 4, 2026-11-08):** SCIP local symbols do not replace
  each other between files.
- The Bazel Go message shows the Go targets that engraph did not index.
- The error message of a failed indexer shows the error, also when the
  indexer writes it to stdout.
- The hook rewrites the usual test commands of Gradle, uv, pnpm, and npx.
- A new untracked file starts `--if-changed`.

## Condition at this time (2026-11-03)

The test projects in the `engraph-test-repos` directory showed these bugs
(refer to the `README.md` of that directory).

| Bug | Effect |
|---|---|
| 1. Names keep SCIP descriptor characters (**corrected**, part 1) | `subgraph Config` finds no Rust type |
| 2. Python graphs have no `CALLS` and no `REFERENCES` (**corrected**, part 2) | `subgraph` shows no callers for Python |
| 3. A Gradle sub-project index writes incorrect paths | Files of other modules get paths that do not exist |
| 4. SCIP local symbols replace each other (**corrected**, part 4) | Local symbols show the name of one file and the location of a different file |
| 5. The Bazel Go message is not correct | The message says that there are no Go targets, but there are |
| 6. Indexer errors on stdout are not shown | The error message of a failed index run is empty |
| 7. The hook does not rewrite some test commands | `./gradlew test`, `uv run pytest`, and other commands are not compressed |
| 8. New untracked files do not start `--if-changed` | The graph does not include a new file until a different change occurs |

### Bug 1: names keep SCIP descriptor characters

**Corrected by part 1 (branch `plan01-part1-names`).** The text below gives
the condition before the correction.

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
`engraph subgraph 'Config#'` shows the struct. An index of the engraph repo has the same
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

This also makes the names change between runs. If a document that refers to a
symbol loads before the document that defines it, the loader writes a
placeholder row with the name from `fallback_display_name`. The definition does
not change that name. The sequence of the documents can change between runs:
scip-java converts SemanticDB files in parallel by default. Tested on
`jsonpath-java`, same snapshot, two runs: one run gave the name
`incrementPosition`, and the other gave `CharacterIndex#incrementPosition`
(`subgraph incrementPosition` showed `No symbols matched.`). Step 4 of part 1
corrects this also.

### Bug 2: Python graphs have no `CALLS` and no `REFERENCES`

**Corrected by part 2 (branch `plan01-parts-2-4`).** The text
below gives the condition before the correction.

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

The correction is merged (pull request #12). It adds
`rebase_subtree_documents`, which removes the documents that are not files in
the subtree directory, but are files at the repo root. The validation of part 3
is not done with the merged code.

### Bug 4: SCIP local symbols replace each other

**Corrected by part 4 (branch `plan01-parts-2-4`).** The text
below gives the condition before the correction. After part 1, the upsert
also replaces `name`, so the name and the signature of a row are from the
same document. The rows still replace each other (refer to part 4).

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

### Part 1: names (bug 1)

**Complete.** Steps 1 to 9 are done. Validation step 12, names with more than
8 entities (before → after): `fd-rust` 7 → 5, `hono-typescript` 57 → 86,
`jsonpath-java` 17 → 61. Names with `#` after the change: 0 in `fd-rust` and
`jsonpath-java`; 53 in `hono-typescript`, all real (52 JavaScript private
members such as `#path`, and one test string symbol). Two conditions found in
the validation are also corrected: global symbols shadow `local N` symbols of
the same name (rust-analyzer names local variables, which made
`full_path_base` ambiguous), and rust-analyzer impl blocks (`impl#[Batch]`)
give members the path `Batch.new` and are themselves named `impl Batch`. Note for step 7: the grep redirect accepts `Config.full_path_base`
as a path shape (identifiers joined by `.`), although `.` is a regex
character; the input is a path only when each segment is an identifier, so
`Config.*` and `foo.bar\b` still bypass the redirect. A `.`-only path that
matches no qualified name also continues as a text search (`Cargo.lock`,
`self.config`). Only a `::` or `#` path uses its last segment as a name.

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

Validation of part 1:

1. With the `main` binary, do the preparation for `fd-rust`,
   `hono-typescript`, and `jsonpath-java`. In each copy, do `engraph index .`.
2. In each copy, record the number of names with more than 8 entities:
   `q "select count(*) from (select name from entities group by name having count(*) > 8)"`
3. In the `fd-rust` copy, record the number of names with `#`:
   `q "select count(*) from entities where file_path is not null and name like '%#%'"`.
   Result: more than 0.
4. In the same `fd-rust` copy, put the binary of the branch first in `PATH`
   (preparation steps 1 and 2). Do `engraph index .` again.
5. Do the query of step 3 again. Result: `0`. The new index changed the old
   names.
6. Do `engraph subgraph Config`. Result: the first line is
   ``## Symbol `Config` (defined in src/config.rs:14)``.
7. Do `engraph subgraph full_path_base`. Result: the first line starts with
   ``## Symbol `full_path_base` ``.
8. Do `engraph subgraph Config::full_path_base`, then
   `engraph subgraph Config#full_path_base`, then
   `engraph subgraph Config.full_path_base`. Result: the three commands show
   the same symbol.
9. Do this command:
   `echo "{\"tool_input\":{\"pattern\":\"Config\"},\"cwd\":\"$PWD\"}" | engraph hook pre-grep`.
   Result: a `deny` decision with the reason
   `` `Config` is indexed in the engraph code graph ``.
10. Do `engraph subgraph new`. Result: `## Multiple matches`. Each line shows a
    qualified name (for example `<Type>.new`) and a file. The symbols in files
    that are not tests are first.
11. With the binary of the branch, do the preparation for `hono-typescript`
    and `jsonpath-java`. In each copy, do `engraph index .`.
12. In the three copies, do the query of step 2 again. Record the values of
    step 2 and step 12 in the pull request.

### Part 2: Python relations (bug 2)

**Complete.** Steps 1 to 3 are done. The validation (2026-11-08) used
scip-python 0.6.8, scip-typescript 0.6.0, scip-java 0.12.3, scip-go 0.2.7,
and rust-analyzer 1.99.1. A first validation (2026-11-06) used
scip-python 0.6.6, scip-typescript 0.4.0, and scip-java 0.10.3; its
TypeScript results do not apply to scip-typescript 0.6.0 (last item).

- Steps 4 and 8: `click-python` `CALLS` 3,599, `IMPLEMENTS` 267, and
  `REFERENCES` 4,455; `requests-python` `CALLS` 616, `IMPLEMENTS` 64, and
  `REFERENCES` 1,177. Steps 5 and 8: `0`. Steps 6 and 9: the output has
  `**Called by**` (`get_best_encoding`: 3 callers; `to_native_string`: 7).
- Step 11: `fd-rust` `CALLS` 2,943 and `REFERENCES` 2,353 with the two
  binaries. `cobra-go`: `CALLS` 4,686 and `IMPLEMENTS` 19 with the two
  binaries; `REFERENCES` 1,534 → 1,526. The 8 edges are edges of `local`
  symbols (21 → 13), a change of part 4. `jsoup-java` was not indexed
  again with scip-java 0.12.3. With 0.10.3: `IMPLEMENTS` 1,729 and
  `REFERENCES` 24,906 with the two binaries; `CALLS` 36,915 → 36,897, also
  from part 4 (refer to "Validation of parts 2 and 4 together").
- Many new Python callers are not correct. The loader gives each
  occurrence to the nearest anchor above it, so a decorator and
  module-level code go to the function above them. A check of the
  `CALLS` and `REFERENCES` edges against the Python AST: `requests-python`
  1,539 of 1,657 correct (92.9%), `click-python` 6,380 of 7,913 (80.6%).
  For example, `check_compatibility` at `src/requests/__init__.py:113`
  (module level) has the caller `_check_cryptography`. Corrected after
  part 2 (branch `scip-caller-scopes`): an occurrence after the end of an
  anchor's `enclosing_range` does not go to that anchor. Then 8,157 of
  8,157 (`click-python`) and 1,535 of 1,535 (`requests-python`) are
  correct; module-level Python code has no relation, because a Python
  module is not an anchor. For TypeScript, a module (kind `File`) is now
  an anchor, so module-level code goes to its file: `handleMiddleware`
  shows the caller `handler.test.ts` (the `describe` callbacks) and no
  longer `createEventContext`, and `hono-typescript` has `CALLS` 4,701 →
  11,309 and `REFERENCES` 9,609 → 19,571 (a load of one SCIP file). Of
  the edges before, 3 are not written and 5,918 move to another anchor;
  the others are new, mostly test code in `describe` callbacks, which
  had no anchor before.
- scip-typescript 0.6.0 sets a kind on every symbol, locals included, so
  part 2 does not change TypeScript graphs. scip-typescript 0.4.0 sets no
  kind: with it, the `main` binary gave `ky-typescript` and
  `hono-typescript` only `IMPLEMENTS`.

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

Validation of part 2:

1. With the `main` binary, do the preparation for `fd-rust`, `cobra-go`, and
   `jsoup-java`. In each copy, do `engraph index .`.
2. In each copy, record the relation counts:
   `q "select kind, count(*) from relations group by kind"`. The values on
   2026-11-02: `fd-rust` `CALLS` 2,943 and `REFERENCES` 2,353; `cobra-go`
   `CALLS` 4,686, `IMPLEMENTS` 19, and `REFERENCES` 1,534; `jsoup-java`
   `CALLS` 36,857, `IMPLEMENTS` 1,729, and `REFERENCES` 24,906.
3. With the binary of the branch, do the preparation for `click-python`. Do
   `engraph index .`.
4. Do `q "select kind, count(*) from relations group by kind"`. Result: there
   are rows for `CALLS` and `REFERENCES`, each with a count of more than 0.
5. Do `q "select count(*) from relations where src_entity like '%).(%' or dst_entity like '%).(%'"`.
   Result: `0`. No relation goes from a parameter or to a parameter.
6. Do `engraph subgraph get_best_encoding`. Result: the output has a line
   that starts with `**Called by**`.
7. With the binary of the branch, do the preparation for `requests-python`. Do
   `engraph index .`.
8. Do the queries of steps 4 and 5 again. Result: the same results as in
   steps 4 and 5.
9. Do `engraph subgraph to_native_string`. Result: the output has a line that
   starts with `**Called by**`.
10. With the binary of the branch, do the preparation for `fd-rust`,
    `cobra-go`, and `jsoup-java`. In each copy, do `engraph index .`.
11. In each copy, do the query of step 2. Result: the counts are the same as
    the counts of step 2.

### Part 3: sub-project paths (bug 3)

The correction is merged (pull request #12, with the test
`rebase_subtree_documents_drops_sibling_files`). Only the validation is
necessary. Use the binary of `main`.

Validation of part 3:

1. Make sure that `gradle --version` shows Gradle 8.5. If it does not, install
   Gradle with the commands in the `jsonpath-java` section of
   `$TEST_REPOS/README.md`.
2. Do the preparation for `jsonpath-java`.
3. Do `engraph index json-path-assert`. Result: the last line starts with
   `indexed` and ends with `driver=scip-java)`.
4. Do `q "select count(*) from entities where file_path like 'json-path-assert/json-path/%'"`.
   Result: `0`.
5. Do `engraph index json-path`.
6. Do this query:
   `q "select substr(file_path, 1, instr(file_path, '/') - 1), count(*) from entities where file_path is not null group by 1"`.
   Result: `json-path 2963` and `json-path-assert 292`.
7. Do this query:
   `q "select count(*) from relations r join entities s on s.id = r.src_entity join entities d on d.id = r.dst_entity where s.file_path like 'json-path-assert/%' and d.file_path like 'json-path/%'"`.
   Result: `161`.
8. Do the preparation for `ripgrep-rust`. Do `engraph index crates/ignore`.
   Result: the output shows `2654 entities`.
9. Do `q "select count(*) from entities where file_path is not null and file_path not like 'crates/ignore/%'"`.
   Result: `0`.
10. Remove bug 3 and part 3 from this plan.

### Part 4: local symbols (bug 4)

**Complete.** Steps 1 to 5 are done, with these changes to the steps:

- Step 2: the id is `local <n>@<project tag>:<relative_path>`. The tag is
  the first 16 hex digits of the SHA-256 of the project key. One database
  holds all projects, and without the tag two projects with the same file
  path (`src/main.rs`) have the same local ids: the load of the second
  project takes the rows and the edges of the first. Other possible
  corrections were a composite key `(project, id)` (rejected in plan 10) or
  an integer key; the two rebuild `entities` and `relations`. The tag makes
  local ids different on each computer. Nothing links local symbols across
  repos, so only a snapshot of ids (plan 05) must remove the tag.
- Step 5, result: a new index did not remove the old `local <n>` rows, and
  also did not remove the locals of a deleted file. A load replaces only
  the symbols of its own schemes (indexers), and a local id has no scheme.
  The migration (schema v13) removes the old rows and their edges, and
  clears the commit of driver-indexed subtrees, as v12 does. Also,
  `entities.scheme` (v13) records the scheme of the indexer that wrote each
  symbol: for a local, the scheme of its document. A load replaces the
  locals of its own schemes, and keeps those of other indexers (`--lang`
  runs, one prebuilt SCIP file for each language).
- Validation step 8 uses `substr(id, instr(id, '@') + 18)`: the path
  follows the `@`, the 16-digit tag, and `:`.

Validation (2026-11-08: scip-typescript 0.6.0, scip-java 0.12.3,
rust-analyzer 1.99.1 with `rust-src`):

| Project | Step 2 → step 9 (relations) | Step 3 → step 10 (`local` entities) |
|---|---|---|
| `fd-rust` | `CALLS` 2,943 → 2,943; `REFERENCES` 2,353 → 2,353 | 224 → 1,067 |
| `jsonpath-java` | `CALLS` 11,002 → 10,984; `IMPLEMENTS` 1,506 → 1,506; `REFERENCES` 8,888 → 8,888 | 161 → 3,376 |
| `hono-typescript` | `CALLS` 7,120 → 4,702; `IMPLEMENTS` 109 → 109; `REFERENCES` 12,401 → 9,609 | 2,826 → 18,478 |

- Steps 5, 7, and 8: `0` in the three copies, and also in `cobra-go`,
  `zap-go`, `ky-typescript`, `click-python`, and `requests-python`. The
  migration was also tested on a copy of a live v12 database: 281 old
  `local` rows → 0, and `PRAGMA foreign_key_check` gives no rows.
- Step 9 is not true for `hono-typescript`: the counts are less. The
  edges that the branch does not write are not correct. 3,407 `CALLS` and
  4,984 `REFERENCES` came from the old `local N` rows (one row with the
  edges of all documents), and 674 `CALLS` went to a parameter. The branch
  adds 655 `CALLS` and 2,083 `REFERENCES` from global anchors, and 1,039
  `CALLS` and 1,045 `REFERENCES` from the locals of each document. The
  symbols that have a caller: 155 → 150. Of the 9 that have no caller now,
  the 6 that were examined had only an incorrect caller, for example
  `local 195` in `src/utils/types.test.ts`.
- `ky-typescript` (not a step): `CALLS` 478 → 263. 216 of the 221 edges
  that the branch does not write went to a parameter. In `ky-typescript`,
  `jsonpath-java`, and `fd-rust`, no symbol loses its callers.
- `zap-go` and `cobra-go` (not a step): the counts are in part 2. Go ids
  include the commit of the baseline, so the edges of two indexes were
  not compared one by one.
- Step 11: `0`. Without steps 3 and 4, 12 of the 30 entries of that list
  are `let` variables. Also, the list leaves out a local that has only its
  number as a name: scip-typescript gives locals no display name, so a
  local function is `8`, and `subgraph 8` gives nothing useful.
- Also changed: the grep redirect. Each document has its own local rows,
  so a variable name in a few files has 1 to 8 matches, and the redirect
  denies the search; `subgraph` then shows only a list of locations.
  `MATCH_WHERE_SQL` now matches a local only when a relation goes from it.
  The names that only locals have and that have 1 to 8 matches: `fd-rust`
  44 (`main`) → 0, `cobra-go` 87 → 1, `zap-go` 44 → 4, `jsonpath-java`
  17 → 0. Without this change: 169, 345, 427, and 357.
- The database becomes larger: `hono-typescript` 15.3 MB → 23.3 MB,
  `jsonpath-java` 16.8 → 19.6, `zap-go` 6.7 → 8.9, `cobra-go` 5.0 → 6.3,
  `fd-rust` 4.0 → 4.5. Only 0% to 1.6% of the local rows have a relation.
- Not corrected: a document with only local symbols has no scheme, so its
  locals get `scheme` NULL, and a load never removes them (also not after
  the file is deleted). No such row was found in the test projects.
- `jsoup-java` (not a step, 2026-11-06, scip-java 0.10.3): with `main`,
  1,306 of 2,121 edges from a local anchor go to a name that is not in the
  file of the anchor. For example,
  `subgraph 'NodeEvaluator.MatchesValue.<init>'` shows the caller `tail`
  in `NodeTest.java:476`. With the branch: 79 of 3,341, all
  constructors (`<init>`, a name that is not in the source text), and the
  caller is `matches` in `QueryParser.java:734`.

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

Validation of part 4:

1. With the `main` binary, do the preparation for `hono-typescript`,
   `jsonpath-java`, and `fd-rust`. In each copy, do `engraph index .`.
2. In each copy, record the relation counts:
   `q "select kind, count(*) from relations group by kind"`.
3. In each copy, record the number of local entities:
   `q "select count(*) from entities where id like 'local %'"`.
4. In the `fd-rust` copy, put the binary of the branch first in `PATH`
   (preparation steps 1 and 2). Do `engraph index .` again.
5. Do `q "select count(*) from entities where id like 'local %' and id not like '%@%'"`.
   Result: `0`. The new index removed the old local rows (correction step 5).
6. With the binary of the branch, do the preparation for `hono-typescript`
   and `jsonpath-java`. In each copy, do `engraph index .`.
7. In the three copies, do the query of step 5. Result: `0`.
8. In the three copies, do this query:
   `q "select count(*) from entities where id like 'local %@%' and file_path != substr(id, instr(id, '@') + 1)"`.
   Result: `0`. Each local entity has the file of its own document.
9. In the three copies, do the query of step 2. Result: each count is equal
   to the count of step 2, or a small quantity more.
10. In the three copies, do the query of step 3. Record the values of step 3
    and step 10 in the pull request.
11. In the `fd-rust` copy, do this command:
    `echo "{\"tool_input\":{\"file_path\":\"$PWD/src/main.rs\"}}" | engraph hook post-read | grep -c 'let '`.
    Result: `0`. The list shows no local variables.

### Validation of parts 2 and 4 together

On 2026-11-06 (scip-typescript 0.4.0, scip-java 0.10.3), after the merge of
the two parts, all validation steps of
parts 2 and 4 were done again, in sequence, on new copies. The `main`
binary is the binary before the two parts. The results are the same as the
results of each part, with these notes:

- Part 2, step 11: `fd-rust` is the same (`CALLS` 2,943, `REFERENCES`
  2,353). `jsoup-java` `CALLS`: 36,915 (`main`) and 36,897 (merged). A load
  of one SCIP file gives 36,921 and 36,897. The difference is part 4: the
  old loader used one kind for each `local N` of the whole index, the kind
  of the last document. In `jsoup-java`, it classified 380 uses of local
  symbols as calls, and 375 of these locals are not callable in their own
  document. In `jsonpath-java`, 24 of 24.
- Part 4, step 9, `jsonpath-java`: `CALLS` 10,998 → 10,984 for the same
  reason. A load of one SCIP file with the binary of part 2 only gives
  10,995.
- With parts 2 and 4, five loads of one `jsoup-java` SCIP file give the
  same `CALLS` (36,897). The `main` binary gave 36,909 to 36,912. The local
  symbols caused that difference. `hono-typescript` still gives 13,042 to
  13,044: anchors with the same line and specificity come from a `HashMap`
  in a different sequence in each process.
- `click-python` shows approximately 830 more `pruned` entities. 857 local
  symbols are used in a document that does not declare them. Their
  placeholder rows now have the scheme of their document, so the load
  removes them when no relation refers to them. Before, these rows stayed.

### Part 5: Bazel Go message (bug 5)

1. Write a test for the display of a new status `SkippedNoGoModules { targets }`.
   The text must include the target count and the name of the variable
   `ENGRAPH_BAZEL_SCIP_GO_CMD`. An example:
   `skipped (3 go targets, no go.mod; set ENGRAPH_BAZEL_SCIP_GO_CMD to index them)`.
2. Add the status. In `run_go_modules`, use it when `targets > 0` and there is
   no `go.mod`.

Validation of part 5:

1. Make sure that `bazel version` operates. Use bazelisk with the name
   `bazel`.
2. Do the preparation for `go-tutorial-go-bazel`.
3. Do `engraph index --bazel-symbols .`. Result: the output has the line
   `symbol[go]: skipped (3 go targets, no go.mod; set ENGRAPH_BAZEL_SCIP_GO_CMD to index them)`.
4. Do the preparation for `java-tutorial-java-bazel`. Do
   `engraph index --bazel-symbols .`. Result: the output has the line
   `symbol[go]: skipped (no targets in this workspace)`. This workspace has no
   Go targets, so the message did not change.

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

### End of each part

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
tests. Part 3 is merged. Only its validation is necessary. Part 1 is the largest: it adds
a schema version and changes `subgraph` and the grep redirect. Parts 4 and 7
are medium: part 4 changes the loader and two lists, and part 7 changes the
hook and `engraph run`.
