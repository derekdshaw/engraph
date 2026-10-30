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
