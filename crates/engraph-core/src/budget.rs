use crate::{db::PooledConn, Result};

pub const DEFAULT_SOFT_LIMIT: i64 = 100_000;
pub const DEFAULT_HARD_LIMIT: i64 = 150_000;

#[derive(Debug, Clone, Copy)]
pub struct BudgetGate {
    pub soft: i64,
    pub hard: i64,
    pub used: i64,
}

impl BudgetGate {
    pub fn escalation_level(&self) -> i64 {
        if self.used >= self.hard {
            3
        } else if self.used >= self.soft {
            2
        } else if self.used >= self.soft / 2 {
            1
        } else {
            0
        }
    }
}

pub fn get_or_init(conn: &PooledConn, session_id: &str) -> Result<BudgetGate> {
    conn.execute(
        "INSERT OR IGNORE INTO session_budget (session_id, soft_limit, hard_limit) VALUES (?1, ?2, ?3)",
        rusqlite::params![session_id, DEFAULT_SOFT_LIMIT, DEFAULT_HARD_LIMIT],
    )?;
    let (soft, hard, used): (i64, i64, i64) = conn.query_row(
        "SELECT soft_limit, hard_limit, used_tokens FROM session_budget WHERE session_id = ?1",
        [session_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(BudgetGate { soft, hard, used })
}

pub fn add_used(conn: &PooledConn, session_id: &str, delta: i64) -> Result<BudgetGate> {
    let g = get_or_init(conn, session_id)?;
    let new_used = g.used + delta;
    let new_level = BudgetGate {
        soft: g.soft,
        hard: g.hard,
        used: new_used,
    }
    .escalation_level();
    conn.execute(
        "UPDATE session_budget SET used_tokens = ?2, escalation_level = ?3, updated_at = datetime('now') WHERE session_id = ?1",
        rusqlite::params![session_id, new_used, new_level],
    )?;
    Ok(BudgetGate {
        soft: g.soft,
        hard: g.hard,
        used: new_used,
    })
}

pub fn set_limits(conn: &PooledConn, session_id: &str, soft: i64, hard: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO session_budget (session_id, soft_limit, hard_limit) VALUES (?1, ?2, ?3) \
         ON CONFLICT(session_id) DO UPDATE SET soft_limit = ?2, hard_limit = ?3, updated_at = datetime('now')",
        rusqlite::params![session_id, soft, hard],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_pool;
    use tempfile::tempdir;

    #[test]
    fn escalation_thresholds() {
        let g = BudgetGate {
            soft: 100,
            hard: 150,
            used: 0,
        };
        assert_eq!(g.escalation_level(), 0);
        assert_eq!(
            BudgetGate { used: 60, ..g }.escalation_level(),
            1
        );
        assert_eq!(
            BudgetGate { used: 100, ..g }.escalation_level(),
            2
        );
        assert_eq!(
            BudgetGate { used: 150, ..g }.escalation_level(),
            3
        );
    }

    #[test]
    fn init_and_increment() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let g0 = get_or_init(&conn, "s1").unwrap();
        assert_eq!(g0.used, 0);
        let g1 = add_used(&conn, "s1", 500).unwrap();
        assert_eq!(g1.used, 500);
        let g2 = add_used(&conn, "s1", 250).unwrap();
        assert_eq!(g2.used, 750);
    }
}
