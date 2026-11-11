# Plan: General performance improvements

Read [the general information for all plans](00-general-info-plan.md)
before you use this plan. It gives the rules for the work.

This plan is a list of small performance improvements. Each item is
independent. Add new items to the end of the list when you find them. When the
work of an item is complete, remove the item from this plan.

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
