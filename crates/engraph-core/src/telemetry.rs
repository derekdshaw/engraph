use crate::{Result, db::PooledConn, models::EventKind};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct EventInput<'a> {
    pub session_id: Option<&'a str>,
    pub kind: EventKind,
    pub feature: &'a str,
    pub filter_id: Option<&'a str>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub latency_ms: i64,
}

pub fn record_event(conn: &PooledConn, ev: EventInput<'_>) -> Result<String> {
    insert_event(conn, &ev, None, None)
}

/// Record one file read (a `hook` / `read` event). `input_tokens` is the token
/// count of the read; `filter_id` is the read type (`text`, `image`, `pdf`,
/// `notebook`); `range` is the part of the file the read returned (`None`
/// when the read reports none: a notebook, an image, a whole PDF). The path is stored only as its SHA-256, which is enough to
/// find reads of a file, or of one range of it, that the session read before.
pub fn record_read(
    conn: &PooledConn,
    session_id: Option<&str>,
    read_type: &str,
    file_path: &str,
    range: Option<&str>,
    tokens: i64,
    latency_ms: i64,
) -> Result<String> {
    let ev = EventInput {
        session_id,
        kind: EventKind::Hook,
        feature: "read",
        filter_id: Some(read_type),
        input_tokens: tokens,
        output_tokens: 0,
        latency_ms,
    };
    let path_hash = crate::utils::sha256_hex(file_path.as_bytes());
    insert_event(conn, &ev, Some(&path_hash), range)
}

fn insert_event(
    conn: &PooledConn,
    ev: &EventInput<'_>,
    path_hash: Option<&str>,
    read_range: Option<&str>,
) -> Result<String> {
    let id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO events (id, session_id, kind, feature, filter_id, input_tokens, output_tokens, latency_ms, path_hash, read_range) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            id,
            ev.session_id,
            ev.kind.as_str(),
            ev.feature,
            ev.filter_id,
            ev.input_tokens,
            ev.output_tokens,
            ev.latency_ms,
            path_hash,
            read_range,
        ],
    )?;
    emit_metrics(ev);
    Ok(id)
}

/// Mirror the event into OpenTelemetry counters/histograms. No-op unless built
/// with `--features otel` AND `ENGRAPH_OTEL` enabled the global meter provider
/// (otherwise this records against the default no-op meter, which is cheap).
/// Attributes are deliberately low-cardinality (`kind`, `feature`); `session_id`
/// and `filter_id` stay only in the SQLite row to avoid metric series explosion.
#[cfg(feature = "otel")]
fn emit_metrics(ev: &EventInput<'_>) {
    use opentelemetry::{KeyValue, global};

    let meter = global::meter("engraph");
    let attrs = [
        KeyValue::new("kind", ev.kind.as_str()),
        KeyValue::new("feature", ev.feature.to_string()),
    ];

    meter.u64_counter("engraph.events").build().add(1, &attrs);
    meter
        .u64_counter("engraph.tokens.input")
        .build()
        .add(ev.input_tokens.max(0) as u64, &attrs);
    meter
        .u64_counter("engraph.tokens.output")
        .build()
        .add(ev.output_tokens.max(0) as u64, &attrs);
    if let Some(saved) = saved_for(
        ev.kind.as_str(),
        ev.feature,
        ev.input_tokens,
        ev.output_tokens,
    ) && saved > 0
    {
        meter
            .u64_counter("engraph.tokens.saved")
            .build()
            .add(saved as u64, &attrs);
    }
    meter
        .u64_histogram("engraph.latency.ms")
        .build()
        .record(ev.latency_ms.max(0) as u64, &attrs);
}

#[cfg(not(feature = "otel"))]
#[inline]
fn emit_metrics(_ev: &EventInput<'_>) {}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GainRow {
    pub kind: String,
    pub feature: String,
    pub count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Token savings. Defined where `input` is the pre-compression / avoided-read
    /// baseline and `output` is the produced size: `compress` and `wrapped_cmd`
    /// (compression), plus the `subgraph` feature (the codegraph neighborhood
    /// stands in for reading the symbol's definition file). `None` for everything
    /// else (`recall`, other `hook` / `index` rows), where the diff has no savings
    /// semantic.
    pub saved_tokens: Option<i64>,
}

pub(crate) fn saved_for(kind: &str, feature: &str, input: i64, output: i64) -> Option<i64> {
    match (kind, feature) {
        ("compress" | "wrapped_cmd", _) => Some(input - output),
        // The codegraph subgraph replaces reading a symbol's definition file:
        // `input` is that avoided-read baseline, `output` the subgraph body.
        (_, "subgraph") => Some((input - output).max(0)),
        _ => None,
    }
}

/// SQL mirror of `saved_for`: the rows where input/output carries a savings
/// semantic. Every aggregate report (summary, temporal, scope, graph) filters
/// through this so non-savings rows — `recall`, `hook`, and especially `index`
/// (millions of input tokens, 0 output) — never inflate the numbers. Keep in
/// lockstep with `saved_for`.
pub(crate) const SAVINGS_WHERE: &str =
    "(kind IN ('compress','wrapped_cmd') OR feature = 'subgraph')";

/// Per-group saved expression, matching `saved_for` (subgraph clamps at 0).
/// Used inside aggregate queries already scoped by `SAVINGS_WHERE`.
pub(crate) const SAVED_EXPR: &str = "COALESCE(SUM(CASE WHEN feature='subgraph' \
          THEN MAX(input_tokens - output_tokens, 0) \
          ELSE input_tokens - output_tokens END),0)";

fn save_pct(saved: i64, input: i64) -> f64 {
    if input > 0 {
        saved as f64 / input as f64 * 100.0
    } else {
        0.0
    }
}

pub fn gain_report(conn: &PooledConn) -> Result<Vec<GainRow>> {
    let mut stmt = conn.prepare(
        "SELECT kind, feature, COUNT(*) AS cnt, \
                COALESCE(SUM(input_tokens),0) AS itk, \
                COALESCE(SUM(output_tokens),0) AS otk \
         FROM events GROUP BY kind, feature ORDER BY kind, feature",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let kind: String = r.get(0)?;
            let feature: String = r.get(1)?;
            let count: i64 = r.get(2)?;
            let input_tokens: i64 = r.get(3)?;
            let output_tokens: i64 = r.get(4)?;
            let saved_tokens = saved_for(&kind, &feature, input_tokens, output_tokens);
            Ok(GainRow {
                kind,
                feature,
                count,
                input_tokens,
                output_tokens,
                saved_tokens,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One row of the itemized savings breakdown across **all** credited sources,
/// not just command output. The `item` is the command name for `output_filter`
/// events (`rg`, `git_log`, …) and the feature name otherwise (`subgraph`,
/// `compress_ingest`, …), so subgraph and message-compression get their own rows
/// and the table's total matches the summary. Field kept as `filter_id` for
/// serialization stability.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FilterGainRow {
    pub filter_id: String,
    pub count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub saved_tokens: i64,
}

pub fn gain_report_by_filter(conn: &PooledConn) -> Result<Vec<FilterGainRow>> {
    // Group command output by filter_id; everything else by feature, so a weak
    // command (`rg`) and a strong source (`subgraph`) are both visible as rows.
    let sql = format!(
        "SELECT (CASE WHEN feature='output_filter' THEN COALESCE(filter_id,'?') \
                      ELSE feature END) AS item, \
                COUNT(*) AS cnt, \
                COALESCE(SUM(input_tokens),0) AS itk, \
                COALESCE(SUM(output_tokens),0) AS otk, \
                {SAVED_EXPR} AS saved \
         FROM events WHERE {SAVINGS_WHERE} \
         GROUP BY item ORDER BY saved DESC, item"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(FilterGainRow {
                filter_id: r.get(0)?,
                count: r.get(1)?,
                input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                saved_tokens: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One row of the three-bucket savings breakdown: `command` (wrapped commands),
/// `codegraph` (subgraph neighborhoods replacing reads), `memory` (message
/// compression). Together these partition every credited (`SAVINGS_WHERE`) row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceRow {
    pub source: String,
    pub count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub saved_tokens: i64,
    pub save_pct: f64,
}

pub fn gain_by_source(conn: &PooledConn) -> Result<Vec<SourceRow>> {
    let sql = format!(
        "SELECT CASE WHEN feature='subgraph' THEN 'codegraph' \
                     WHEN kind='wrapped_cmd' THEN 'command' \
                     WHEN kind='compress' THEN 'memory' \
                     ELSE 'other' END AS source, \
                COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), \
                {SAVED_EXPR} AS saved \
         FROM events WHERE {SAVINGS_WHERE} GROUP BY source ORDER BY saved DESC, source"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            let source: String = r.get(0)?;
            let count: i64 = r.get(1)?;
            let input_tokens: i64 = r.get(2)?;
            let output_tokens: i64 = r.get(3)?;
            let saved_tokens: i64 = r.get(4)?;
            Ok(SourceRow {
                source,
                count,
                input_tokens,
                output_tokens,
                saved_tokens,
                save_pct: save_pct(saved_tokens, input_tokens),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Top-line totals over savings-bearing rows only (the rtk-style header).
#[derive(Debug, Clone, serde::Serialize)]
pub struct GainSummary {
    pub commands: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub saved_tokens: i64,
    pub save_pct: f64,
}

pub fn gain_summary(conn: &PooledConn) -> Result<GainSummary> {
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), {SAVED_EXPR} \
         FROM events WHERE {SAVINGS_WHERE}"
    );
    let s = conn.query_row(&sql, [], |r| {
        let commands: i64 = r.get(0)?;
        let input_tokens: i64 = r.get(1)?;
        let output_tokens: i64 = r.get(2)?;
        let saved_tokens: i64 = r.get(3)?;
        Ok(GainSummary {
            commands,
            input_tokens,
            output_tokens,
            saved_tokens,
            save_pct: save_pct(saved_tokens, input_tokens),
        })
    })?;
    Ok(s)
}

/// File reads (`read` events, §5.6 of DETAILS.md): all of them; the repeats,
/// reads of a range of a file that the same session read before; and the new
/// ranges, reads of another range of a file that the session read before
/// (such as the next page of a long file). A read without a session id is
/// neither. Reads have no savings semantic, so they are outside
/// `SAVINGS_WHERE`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadsReport {
    pub reads: i64,
    pub read_tokens: i64,
    pub repeat_reads: i64,
    pub repeat_tokens: i64,
    pub new_range_reads: i64,
    pub new_range_tokens: i64,
}

// Only read events have a path_hash, so the filter is that column alone:
// it lets the scan use the partial `idx_events_reads`, whose order serves
// the `range_nth` window (SQLite evaluates the last window first).
const READS_SQL: &str = "SELECT COUNT(*), COALESCE(SUM(input_tokens),0), \
        COALESCE(SUM(range_nth > 1),0), \
        COALESCE(SUM(CASE WHEN range_nth > 1 THEN input_tokens ELSE 0 END),0), \
        COALESCE(SUM(range_nth = 1 AND path_nth > 1),0), \
        COALESCE(SUM(CASE WHEN range_nth = 1 AND path_nth > 1 \
                          THEN input_tokens ELSE 0 END),0) \
 FROM (SELECT input_tokens, \
              CASE WHEN session_id IS NULL THEN 1 \
                   ELSE ROW_NUMBER() OVER (PARTITION BY session_id, path_hash \
                                           ORDER BY seq) END AS path_nth, \
              CASE WHEN session_id IS NULL THEN 1 \
                   ELSE ROW_NUMBER() OVER (PARTITION BY session_id, path_hash, \
                                           read_range ORDER BY seq) END AS range_nth \
       FROM events WHERE path_hash IS NOT NULL)";

pub fn gain_reads(conn: &PooledConn) -> Result<ReadsReport> {
    let r = conn.query_row(READS_SQL, [], |r| {
        Ok(ReadsReport {
            reads: r.get(0)?,
            read_tokens: r.get(1)?,
            repeat_reads: r.get(2)?,
            repeat_tokens: r.get(3)?,
            new_range_reads: r.get(4)?,
            new_range_tokens: r.get(5)?,
        })
    })?;
    Ok(r)
}

/// Index runs (`index` events) of one `filter_id`: `auto` (the SessionStart
/// refresh), `if_changed` (`--if-changed` by hand), `workspace`, or the
/// driver name. `runs`, `total_ms`, and `mean_ms` are of the runs that
/// produced SCIP; `no_op` counts the others, which `auto` and `if_changed`
/// record when they find nothing to do, are skipped, or fail (0 bytes), so
/// the refresh rate is visible without the change checks diluting the time.
/// Index runs save no tokens, so they are outside `SAVINGS_WHERE`;
/// `scip_bytes` is the sum of `input_tokens`, which holds the SCIP bytes of a
/// run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexRow {
    pub filter_id: String,
    pub runs: i64,
    pub total_ms: i64,
    pub mean_ms: f64,
    pub scip_bytes: i64,
    pub no_op: i64,
}

pub fn gain_index(conn: &PooledConn) -> Result<Vec<IndexRow>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(filter_id,'?') AS f, COALESCE(SUM(input_tokens > 0),0), \
                COALESCE(SUM(CASE WHEN input_tokens > 0 THEN latency_ms ELSE 0 END),0), \
                COALESCE(AVG(CASE WHEN input_tokens > 0 THEN latency_ms END),0), \
                COALESCE(SUM(input_tokens),0), COALESCE(SUM(input_tokens = 0),0) \
         FROM events WHERE kind = 'index' GROUP BY f ORDER BY COUNT(*) DESC, f",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(IndexRow {
                filter_id: r.get(0)?,
                runs: r.get(1)?,
                total_ms: r.get(2)?,
                mean_ms: r.get(3)?,
                scip_bytes: r.get(4)?,
                no_op: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Calendar granularity for `gain_by_time`.
#[derive(Debug, Clone, Copy)]
pub enum TimeBucket {
    Daily,
    Weekly,
    Monthly,
}

impl TimeBucket {
    /// SQLite expression that maps `ts` to a bucket label. Weekly is
    /// Sunday-aligned (the start-of-week date) to match rtk's Sun–Sat week.
    fn expr(self) -> &'static str {
        match self {
            TimeBucket::Daily => "date(ts)",
            TimeBucket::Weekly => "date(ts, '-' || strftime('%w', ts) || ' days')",
            TimeBucket::Monthly => "strftime('%Y-%m', ts)",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TimeBucket::Daily => "Daily",
            TimeBucket::Weekly => "Weekly",
            TimeBucket::Monthly => "Monthly",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimeRow {
    pub bucket: String,
    pub count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub saved_tokens: i64,
    pub save_pct: f64,
}

pub fn gain_by_time(conn: &PooledConn, bucket: TimeBucket) -> Result<Vec<TimeRow>> {
    let b = bucket.expr();
    let sql = format!(
        "SELECT {b} AS bkt, COUNT(*), COALESCE(SUM(input_tokens),0), \
                COALESCE(SUM(output_tokens),0), {SAVED_EXPR} \
         FROM events WHERE {SAVINGS_WHERE} GROUP BY bkt ORDER BY bkt"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            let bucket: String = r.get(0)?;
            let count: i64 = r.get(1)?;
            let input_tokens: i64 = r.get(2)?;
            let output_tokens: i64 = r.get(3)?;
            let saved_tokens: i64 = r.get(4)?;
            Ok(TimeRow {
                bucket,
                count,
                input_tokens,
                output_tokens,
                saved_tokens,
                save_pct: save_pct(saved_tokens, input_tokens),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Aggregation axis for `gain_by_scope`: by project (joined from `sessions`) or
/// by raw session id.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Project,
    Session,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScopeRow {
    pub scope: String,
    pub count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub saved_tokens: i64,
    pub save_pct: f64,
}

pub fn gain_by_scope(conn: &PooledConn, scope: Scope) -> Result<Vec<ScopeRow>> {
    // Project needs the sessions join; session keys directly off the event.
    let key = match scope {
        Scope::Project => "COALESCE(s.project, '?')",
        Scope::Session => "COALESCE(e.session_id, '?')",
    };
    let sql = format!(
        "SELECT {key} AS scope, COUNT(*), COALESCE(SUM(e.input_tokens),0), \
                COALESCE(SUM(e.output_tokens),0), \
                COALESCE(SUM(CASE WHEN e.feature='subgraph' \
                    THEN MAX(e.input_tokens - e.output_tokens, 0) \
                    ELSE e.input_tokens - e.output_tokens END),0) AS saved \
         FROM events e LEFT JOIN sessions s ON e.session_id = s.id \
         WHERE (e.kind IN ('compress','wrapped_cmd') OR e.feature = 'subgraph') \
         GROUP BY scope ORDER BY saved DESC, scope"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            let scope: String = r.get(0)?;
            let count: i64 = r.get(1)?;
            let input_tokens: i64 = r.get(2)?;
            let output_tokens: i64 = r.get(3)?;
            let saved_tokens: i64 = r.get(4)?;
            Ok(ScopeRow {
                scope,
                count,
                input_tokens,
                output_tokens,
                saved_tokens,
                save_pct: save_pct(saved_tokens, input_tokens),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HistoryRow {
    pub ts: String,
    pub kind: String,
    pub feature: String,
    pub filter_id: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub saved_tokens: i64,
}

/// The `n` most recent savings-bearing events, newest first.
pub fn gain_history(conn: &PooledConn, n: usize) -> Result<Vec<HistoryRow>> {
    let sql = format!(
        "SELECT ts, kind, feature, COALESCE(filter_id,'-'), input_tokens, output_tokens \
         FROM events WHERE {SAVINGS_WHERE} ORDER BY seq DESC LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([n as i64], |r| {
            let ts: String = r.get(0)?;
            let kind: String = r.get(1)?;
            let feature: String = r.get(2)?;
            let filter_id: String = r.get(3)?;
            let input_tokens: i64 = r.get(4)?;
            let output_tokens: i64 = r.get(5)?;
            let saved_tokens = saved_for(&kind, &feature, input_tokens, output_tokens).unwrap_or(0);
            Ok(HistoryRow {
                ts,
                kind,
                feature,
                filter_id,
                input_tokens,
                output_tokens,
                saved_tokens,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Saved tokens per day over the last `days`, oldest first. Days with no events
/// are absent here; the renderer fills the gaps so the graph spans a full window.
pub fn gain_daily_series(conn: &PooledConn, days: i64) -> Result<Vec<(String, i64)>> {
    let sql = format!(
        "SELECT date(ts) AS d, {SAVED_EXPR} \
         FROM events WHERE {SAVINGS_WHERE} AND date(ts) >= date('now', ?1) \
         GROUP BY d ORDER BY d"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([format!("-{} days", days - 1)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_pool;
    use tempfile::tempdir;

    #[test]
    fn record_and_report() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        record_event(
            &conn,
            EventInput {
                session_id: None,
                kind: EventKind::Compress,
                feature: "compress",
                filter_id: None,
                input_tokens: 1000,
                output_tokens: 400,
                latency_ms: 12,
            },
        )
        .unwrap();
        let rows = gain_report(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].saved_tokens, Some(600));
    }

    #[test]
    fn retrieve_kind_has_no_savings() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        record_event(
            &conn,
            EventInput {
                session_id: None,
                kind: EventKind::Retrieve,
                feature: "recall",
                filter_id: None,
                input_tokens: 0,
                output_tokens: 200,
                latency_ms: 5,
            },
        )
        .unwrap();
        let rows = gain_report(&conn).unwrap();
        assert_eq!(rows[0].saved_tokens, None);
    }

    #[test]
    fn index_kind_has_no_savings() {
        // `codegraph_index` records index bytes in `input` and 0 in `output`;
        // under the `index` kind that must NOT read as input-output saved.
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        record_event(
            &conn,
            EventInput {
                session_id: None,
                kind: EventKind::Index,
                feature: "codegraph_index",
                filter_id: None,
                input_tokens: 5_000_000,
                output_tokens: 0,
                latency_ms: 42,
            },
        )
        .unwrap();
        let rows = gain_report(&conn).unwrap();
        assert_eq!(rows[0].saved_tokens, None);
    }

    #[test]
    fn subgraph_feature_is_credited_and_clamped() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        // subgraph stands in for a 1000-token def file, emits a 150-token body.
        record_event(
            &conn,
            EventInput {
                session_id: None,
                kind: EventKind::Retrieve,
                feature: "subgraph",
                filter_id: Some("subgraph"),
                input_tokens: 1000,
                output_tokens: 150,
                latency_ms: 3,
            },
        )
        .unwrap();
        let rows = gain_report(&conn).unwrap();
        assert_eq!(rows[0].saved_tokens, Some(850));
    }

    fn ev(kind: EventKind, feature: &str, input: i64, output: i64) -> EventInput<'_> {
        EventInput {
            session_id: None,
            kind,
            feature,
            filter_id: None,
            input_tokens: input,
            output_tokens: output,
            latency_ms: 0,
        }
    }

    #[test]
    fn summary_excludes_non_savings_rows() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        record_event(&conn, ev(EventKind::Compress, "compress", 1000, 400)).unwrap();
        record_event(&conn, ev(EventKind::Retrieve, "subgraph", 800, 150)).unwrap();
        // These two must NOT count toward the summary.
        record_event(&conn, ev(EventKind::Index, "codegraph_index", 5_000_000, 0)).unwrap();
        record_event(&conn, ev(EventKind::Retrieve, "recall", 0, 200)).unwrap();

        let s = gain_summary(&conn).unwrap();
        assert_eq!(s.commands, 2);
        assert_eq!(s.input_tokens, 1800);
        assert_eq!(s.output_tokens, 550);
        assert_eq!(s.saved_tokens, 1250);
    }

    #[test]
    fn reads_report_separates_repeats_and_new_ranges() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let read = |session, path, range, tokens| {
            record_read(&conn, session, "text", path, range, tokens, 0).unwrap();
        };
        read(Some("s1"), "/r/a.rs", Some("1+10"), 100);
        // Same session, file, and range: a repeat.
        read(Some("s1"), "/r/a.rs", Some("1+10"), 40);
        // The next page of a file the session read: a new range, not a repeat.
        read(Some("s1"), "/r/a.rs", Some("11+10"), 60);
        read(Some("s1"), "/r/a.rs", Some("11+10"), 20);
        read(Some("s1"), "/r/b.rs", None, 30);
        // Another session reading a.rs for the first time: neither.
        read(Some("s2"), "/r/a.rs", Some("1+10"), 100);
        // Without a session id, a read is neither.
        read(None, "/r/c.rs", None, 10);
        read(None, "/r/c.rs", None, 10);
        // Other events are not reads.
        record_event(&conn, ev(EventKind::Hook, "read_augment", 0, 50)).unwrap();

        let r = gain_reads(&conn).unwrap();
        assert_eq!((r.reads, r.read_tokens), (8, 370));
        assert_eq!((r.repeat_reads, r.repeat_tokens), (2, 60));
        assert_eq!((r.new_range_reads, r.new_range_tokens), (1, 60));
        // Reads have no savings semantic.
        assert_eq!(gain_summary(&conn).unwrap().commands, 0);
    }

    #[test]
    fn reads_report_scans_the_reads_index() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {READS_SQL}"))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map([], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|d| d.contains("idx_events_reads")),
            "{plan:?}"
        );
    }

    #[test]
    fn index_report_has_one_row_per_filter_id() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let index = |filter_id, bytes, ms| EventInput {
            session_id: None,
            kind: EventKind::Index,
            feature: "codegraph_index",
            filter_id: Some(filter_id),
            input_tokens: bytes,
            output_tokens: 0,
            latency_ms: ms,
        };
        record_event(&conn, index("auto", 1000, 100)).unwrap();
        record_event(&conn, index("auto", 3000, 300)).unwrap();
        record_event(&conn, index("if_changed", 500, 50)).unwrap();
        record_event(&conn, index("rust-analyzer", 9000, 2000)).unwrap();
        // Runs that produced no SCIP (nothing changed, skipped) are no-ops:
        // counted apart, and not in the time.
        record_event(&conn, index("auto", 0, 5)).unwrap();
        record_event(&conn, index("if_changed", 0, 5)).unwrap();
        record_event(&conn, index("workspace", 0, 5)).unwrap();
        record_event(&conn, ev(EventKind::Compress, "compress", 1000, 400)).unwrap();
        let before = gain_summary(&conn).unwrap();

        let rows = gain_index(&conn).unwrap();
        let row = |f: &str| rows.iter().find(|r| r.filter_id == f).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            (
                row("auto").runs,
                row("auto").total_ms,
                row("auto").scip_bytes
            ),
            (2, 400, 4000)
        );
        assert_eq!(row("auto").mean_ms, 200.0);
        assert_eq!(row("auto").no_op, 1);
        assert_eq!(row("if_changed").no_op, 1);
        assert_eq!(row("rust-analyzer").no_op, 0);
        let ws = row("workspace");
        assert_eq!((ws.runs, ws.total_ms, ws.mean_ms, ws.no_op), (0, 0, 0.0, 1));
        assert_eq!(
            (row("if_changed").runs, row("if_changed").total_ms),
            (1, 50)
        );
        assert_eq!(
            (row("rust-analyzer").runs, row("rust-analyzer").scip_bytes),
            (1, 9000)
        );
        // Index runs save no tokens: the summary counts only the compress row.
        assert_eq!(before.commands, 1);
        assert_eq!(before.input_tokens, 1000);
    }

    #[test]
    fn by_source_buckets_partition_savings() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        record_event(&conn, ev(EventKind::WrappedCmd, "output_filter", 1000, 300)).unwrap();
        record_event(&conn, ev(EventKind::Retrieve, "subgraph", 800, 150)).unwrap();
        record_event(&conn, ev(EventKind::Compress, "compress_sweep", 500, 100)).unwrap();
        // Non-savings rows must not appear in any bucket.
        record_event(&conn, ev(EventKind::Index, "codegraph_index", 9_000, 0)).unwrap();

        let rows = gain_by_source(&conn).unwrap();
        let by = |s: &str| rows.iter().find(|r| r.source == s).map(|r| r.saved_tokens);
        assert_eq!(by("command"), Some(700));
        assert_eq!(by("codegraph"), Some(650));
        assert_eq!(by("memory"), Some(400));
        assert!(rows.iter().all(|r| r.source != "other"));
        let total: i64 = rows.iter().map(|r| r.saved_tokens).sum();
        assert_eq!(total, 1750);
    }

    #[test]
    fn time_bucketing_groups_by_day() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        // Insert with explicit ts so the buckets are deterministic.
        for (ts, input, output) in [
            ("2026-06-01 10:00:00", 1000, 300),
            ("2026-06-01 12:00:00", 500, 200),
            ("2026-06-02 09:00:00", 800, 100),
        ] {
            conn.execute(
                "INSERT INTO events (id, kind, feature, input_tokens, output_tokens, ts) \
                 VALUES (?1, 'compress', 'compress', ?2, ?3, ?4)",
                rusqlite::params![Uuid::now_v7().to_string(), input, output, ts],
            )
            .unwrap();
        }
        let rows = gain_by_time(&conn, TimeBucket::Daily).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].bucket, "2026-06-01");
        assert_eq!(rows[0].saved_tokens, 1000); // (1000-300)+(500-200)
        assert_eq!(rows[1].bucket, "2026-06-02");
        assert_eq!(rows[1].saved_tokens, 700);
    }

    #[test]
    fn by_project_joins_sessions() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO sessions (id, project, started_at) VALUES ('s1', '/repo/a', '2026-06-01')",
            [],
        )
        .unwrap();
        record_event(
            &conn,
            EventInput {
                session_id: Some("s1"),
                ..ev(EventKind::WrappedCmd, "output_filter", 1000, 250)
            },
        )
        .unwrap();
        // An event with no session falls into the '?' bucket, not '/repo/a'.
        record_event(&conn, ev(EventKind::Compress, "compress", 400, 100)).unwrap();

        let rows = gain_by_scope(&conn, Scope::Project).unwrap();
        let a = rows.iter().find(|r| r.scope == "/repo/a").unwrap();
        assert_eq!(a.saved_tokens, 750);
        assert!(rows.iter().any(|r| r.scope == "?"));
    }

    #[test]
    fn subgraph_savings_clamp_at_zero_when_baseline_unmeasurable() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        // input=0 (file unreadable) must not read as a negative saving.
        record_event(
            &conn,
            EventInput {
                session_id: None,
                kind: EventKind::Retrieve,
                feature: "subgraph",
                filter_id: Some("subgraph"),
                input_tokens: 0,
                output_tokens: 150,
                latency_ms: 3,
            },
        )
        .unwrap();
        let rows = gain_report(&conn).unwrap();
        assert_eq!(rows[0].saved_tokens, Some(0));
    }
}
