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
    let id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO events (id, session_id, kind, feature, filter_id, input_tokens, output_tokens, latency_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            id,
            ev.session_id,
            ev.kind.as_str(),
            ev.feature,
            ev.filter_id,
            ev.input_tokens,
            ev.output_tokens,
            ev.latency_ms,
        ],
    )?;
    Ok(id)
}

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

fn saved_for(kind: &str, feature: &str, input: i64, output: i64) -> Option<i64> {
    match (kind, feature) {
        ("compress" | "wrapped_cmd", _) => Some(input - output),
        // The codegraph subgraph replaces reading a symbol's definition file:
        // `input` is that avoided-read baseline, `output` the subgraph body.
        (_, "subgraph") => Some((input - output).max(0)),
        _ => None,
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
