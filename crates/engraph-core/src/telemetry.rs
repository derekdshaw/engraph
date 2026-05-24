use crate::{db::PooledConn, models::EventKind, Result};
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
    pub saved_tokens: i64,
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
            Ok(GainRow {
                kind,
                feature,
                count,
                input_tokens,
                output_tokens,
                saved_tokens: input_tokens - output_tokens,
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
                feature: "F6",
                filter_id: None,
                input_tokens: 1000,
                output_tokens: 400,
                latency_ms: 12,
            },
        )
        .unwrap();
        let rows = gain_report(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].saved_tokens, 600);
    }
}
