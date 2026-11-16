# Plan: General performance improvements

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work.

This plan is a list of small performance improvements. Each item is
independent. Add new items to the end of the list when you find them.

## Plan management

This plan does not complete as a whole, because new items are added when they
are found. These tasks replace rule 5 of the general information:

1. When the work of an item is complete, remove the item from this plan. The
   numbers of the other items do not change.
2. This plan does not move to `docs/plans/completed/`.

---

## 1. Open one database connection, not four

### Result

Each `engraph` process opens one SQLite connection when it starts. It does not
open connections that it does not use.

### Condition at this time (2026-10-30)

Each `engraph` process opens the pool one time and uses one connection
(`crates/engraph-cli/src/main.rs:62-63`):

```rust
let pool = db::open_default_pool()?;
let conn = pool.get()?;
```

`open_pool` builds an r2d2 pool with `.max_size(4)`
(`crates/engraph-core/src/db.rs:74`). With the r2d2 0.8.10 defaults, this pool
does these things:

- It opens 4 connections before it returns. `min_idle` is `None`, and r2d2
  then uses `max_size` (`r2d2/src/lib.rs:212`). `build()` waits for all 4
  connections (`r2d2/src/config.rs:228`). Each connection does the 5 pragmas in
  `Pragmas::on_acquire`.
- It starts 3 worker threads (`r2d2/src/config.rs:250`) and a task that removes
  old connections, because `idle_timeout` and `max_lifetime` have values.
- It does `SELECT 1` each time it gives a connection (`test_on_check_out` is
  `true`). This check is for network connections. A local SQLite file does not
  need it.

Measurements (2026-10-30, WSL2, a copy of the live database):

| Measurement | Result |
|---|---|
| `engraph budget status`, release build, 30 runs | 7.2 ms median |
| Open 1 connection with the same pragmas (Python `sqlite3`, 200 runs) | 0.44 ms |
| Open 4 connections with the same pragmas | 1.30 ms |

Thus, the 3 connections that engraph does not use cost approximately 1 ms for
each process. This value is an estimate from Python, not a measurement of
engraph. The hooks start a process for each Bash, Grep, and Read tool call.

### Steps

1. Measure `engraph budget status` with the release build: 30 runs, median and
   minimum. Use a copy of a real database. Record the result in the pull
   request.
2. In `open_pool`, add these settings to the r2d2 builder:
   - `.min_idle(Some(1))`: open one connection at the start. Open more only if
     a caller asks for them.
   - `.test_on_check_out(false)`: do not do `SELECT 1` for each connection.
   - `.idle_timeout(None)` and `.max_lifetime(None)`: do not start the task
     that removes old connections.
3. Keep `.max_size(4)`. The test `add_used_is_atomic_under_threads`
   (`crates/engraph-core/src/budget.rs`) uses more than one connection at the
   same time.
4. Do the measurement of step 1 again. Record the two results in the pull
   request.

### Tests

- A new pool has one open connection after `open_pool` returns (use
  `Pool::state().connections`).
- A second `pool.get()` while the first connection is in use gives a second
  connection.
- `add_used_is_atomic_under_threads` passes.

### Risks

A caller that asks for more than one connection now waits while r2d2 opens
it. At this time, only tests do this.

### Alternative that this plan does not use

Remove r2d2 and use one `rusqlite::Connection`. This also removes the worker
threads. But each crate uses `&PooledConn` in its function signatures, and
`DETAILS.md` §6.3 gives the reason. This change touches all crates, and it
saves little time more than the steps above.

### Effort

Less than half a day.

---

## 2. Prepare the placeholder insert one time

### Result

`ensure_placeholder_entity` uses a prepared statement from the cache. The
loader does not prepare the same statement again for each reference.

### Condition at this time (2026-11-11)

The loader has two statements that it writes one time for each symbol. It
prepares one of them with the cache, and the other without it.

The entity upsert, one time for each symbol of a document
(`crates/engraph-codegraph/src/scip_loader.rs:325`):

```rust
let mut entity_insert = conn.prepare_cached(
```

`ensure_placeholder_entity`, one time for each reference
(`crates/engraph-codegraph/src/scip_loader.rs:578`):

```rust
    conn.execute(
```

`conn.execute` prepares the statement, operates it, and then discards it.
Thus SQLite parses this SQL again for each call.

`ensure_placeholder_entity` is the hotter of the two statements. The loader
calls it one time for each reference occurrence
(`scip_loader.rs:452`) and one time for each relationship (`scip_loader.rs:374`).
An index of the engraph repo on 2026-11-11 wrote 6,864 entities and 18,594
relations in 11,227 ms. The reference occurrences are more than the symbols,
because each symbol has many references.

`insert_relation` (`scip_loader.rs:560`) has the same condition. It writes one
row for each relation with `conn.execute`.

### Steps

1. Measure `engraph index .` on this repo with the release build: 5 runs,
   median. Record the result in the pull request.
2. Give `ensure_placeholder_entity` a prepared statement, or change it to use
   `conn.prepare_cached`. The function gets `&PooledConn`, and
   `prepare_cached` keeps the statement in the connection's cache. Thus the
   signature does not have to change.
3. Do the same for `insert_relation`.
4. Do the measurement of step 1 again. Record the two results in the pull
   request.

### Tests

The tests of the loader (`crates/engraph-codegraph/tests/loader_unit.rs`) pass
without a change. This item changes no behavior.

### Risks

The statement cache of rusqlite has a size limit (16 statements by default).
The loader prepares few statements, so this limit is not a problem. If the
number of prepared statements increases, use `set_prepared_statement_cache_capacity`.

### Effort

Less than one hour.

---

## 3. Detect the manifests of a directory from its listing

### Result

Workspace discovery finds the manifests of a directory from the directory
listing that it already reads. It does not do one `stat` for each manifest
name.

### Condition at this time (2026-11-15)

`walk_recursive` (`crates/engraph-codegraph/src/index.rs:773`) calls
`detected_langs` for each directory that it examines:

```rust
let detected = detected_langs(dir);
```

`detected_langs` (`index.rs:687`) calls `bazel::detect_bazel` and the
`detect` of each driver. Each probe is an `is_file` call, thus one `stat`
(`crates/engraph-codegraph/src/bazel.rs:38`):

```rust
repo.join("WORKSPACE").is_file()
    || repo.join("WORKSPACE.bazel").is_file()
    || repo.join("MODULE.bazel").is_file()
```

A directory with no manifest gets 13 `stat` calls that fail: 3 for Bazel, 1
for Cargo, 2 for Python, 1 for Go, 1 for TypeScript, and 5 for Java. Also,
`driver::registry()` (`crates/engraph-codegraph/src/driver.rs:21`) allocates
5 boxed drivers for each directory.

Then `walk_recursive` reads the same directory with `std::fs::read_dir` to
find its children. The listing has the names of the manifests, but the walk
does not use them.

Measurements (2026-11-15, WSL2, release build, `plan_workspace` with
`recursive`, median of 5 warm runs):

| Tree | Directories | Time |
|---|---|---|
| Generated git tree, no manifest in most directories | 56,000 | 5,630 ms |
| `~/.cargo/registry/src` | 11,700 | 745 ms |

This is approximately 90 µs for each directory. The ignore check of plan 02
adds 8 to 15 percent to this time.

### Steps

1. Measure `plan_workspace` with `recursive` on a large tree: 5 runs, median.
   Record the result in the pull request.
2. In `walk_recursive`, read the directory one time. Find the manifests in
   the file names of the listing, and the children in the directory names.
3. Use the same manifest names as `detect_bazel` and each `Driver::detect`.
   Keep one list of the names, so that detection and discovery do not differ.
4. For an entry that is a symbolic link, use `Path::is_file`. `is_file`
   follows the link, as the probes do at this time.
5. Do the measurement of step 1 again. Record the two results in the pull
   request.

### Tests

- The tests of `walk_recursive` and `plan_workspace` pass without a change.
- A manifest that is a symbolic link to a file is found.

### Risks

A second list of manifest names can become different from the `detect`
functions. Step 3 prevents this.

### Effort

Half a day.

---

## 4. Examine the subtrees of a workspace in parallel

### Result

`engraph index --recursive` examines the subtrees of a workspace on more than
one thread.

### Condition at this time (2026-11-15)

`walk_recursive` examines one directory at a time. The subtrees are
independent: each call gets the languages of its ancestors as a value
(`claimed`) and adds roots to `out`. Only the ignore matcher is shared,
because `IncrementalIgnore::matched` takes `&mut self` to fill its cache.

Discovery is a small part of an index run: each indexer takes seconds to
minutes. Thus this item is useful only for very large trees, after item 3.

### Steps

1. Do item 3 first, and measure again.
2. Find out if the discovery time is still a problem on a real large
   workspace. If it is not, remove this item.
3. Give each thread its own `IncrementalIgnore`, or examine the top-level
   children on a pool of threads, with one matcher for each child.
4. Collect the roots through a channel, and sort them at the end, as
   `discover_repos_recursive` does at this time.

### Tests

The discovery tests give the same sorted result.

### Risks

Discovery is blocking file-system work, not network work. Async tasks do not
make it faster: an async file-system call operates on a blocking thread. Use
threads.

### Effort

One day.

---

## 5. Read the member rule of a parent project one time

`is_member` (`crates/engraph-codegraph/src/index.rs`) reads and parses the parent's `Cargo.toml`, `package.json` or `pnpm-workspace.yaml`, and compiles its globs, again for each nested crate or package below it; keep the rule with the parent in `claimed` if a large workspace shows the cost.
