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
    conn.execute(
        "INSERT OR IGNORE INTO session_budget (session_id, soft_limit, hard_limit) VALUES (?1, ?2, ?3)",
        rusqlite::params![session_id, DEFAULT_SOFT_LIMIT, DEFAULT_HARD_LIMIT],
    )?;
    conn.execute(
        "UPDATE session_budget
         SET used_tokens = used_tokens + ?2,
             escalation_level = CASE
                WHEN used_tokens + ?2 >= hard_limit THEN 3
                WHEN used_tokens + ?2 >= soft_limit THEN 2
                WHEN used_tokens + ?2 >= soft_limit / 2 THEN 1
                ELSE 0
             END,
             updated_at = datetime('now')
         WHERE session_id = ?1",
        rusqlite::params![session_id, delta],
    )?;
    let (soft, hard, used): (i64, i64, i64) = conn.query_row(
        "SELECT soft_limit, hard_limit, used_tokens FROM session_budget WHERE session_id = ?1",
        [session_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(BudgetGate { soft, hard, used })
}

pub fn set_limits(conn: &PooledConn, session_id: &str, soft: i64, hard: i64) -> Result<()> {
    if soft <= 0 || hard <= 0 {
        return Err(crate::Error::Config(format!(
            "budget limits must be positive (soft={soft}, hard={hard})"
        )));
    }
    if soft > hard {
        return Err(crate::Error::Config(format!(
            "soft limit ({soft}) must not exceed hard limit ({hard})"
        )));
    }
    conn.execute(
        "INSERT INTO session_budget (session_id, soft_limit, hard_limit) VALUES (?1, ?2, ?3) \
         ON CONFLICT(session_id) DO UPDATE SET soft_limit = ?2, hard_limit = ?3, updated_at = datetime('now')",
        rusqlite::params![session_id, soft, hard],
    )?;
    // Recompute escalation against the (possibly changed) limits and unchanged used_tokens.
    conn.execute(
        "UPDATE session_budget
         SET escalation_level = CASE
                WHEN used_tokens >= hard_limit THEN 3
                WHEN used_tokens >= soft_limit THEN 2
                WHEN used_tokens >= soft_limit / 2 THEN 1
                ELSE 0
             END,
             updated_at = datetime('now')
         WHERE session_id = ?1",
        rusqlite::params![session_id],
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
        assert_eq!(BudgetGate { used: 60, ..g }.escalation_level(), 1);
        assert_eq!(BudgetGate { used: 100, ..g }.escalation_level(), 2);
        assert_eq!(BudgetGate { used: 150, ..g }.escalation_level(), 3);
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

    #[test]
    fn set_limits_validates() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        assert!(set_limits(&conn, "s", 0, 100).is_err());
        assert!(set_limits(&conn, "s", 100, 0).is_err());
        assert!(set_limits(&conn, "s", 200, 100).is_err());
        assert!(set_limits(&conn, "s", 100, 200).is_ok());
    }

    #[test]
    fn add_used_is_atomic_under_threads() {
        use std::sync::Arc;
        use std::thread;
        let dir = tempdir().unwrap();
        let pool = Arc::new(open_pool(&dir.path().join("t.db")).unwrap());
        let session = "concurrent";
        // Seed the row so all threads UPDATE rather than racing the INSERT OR IGNORE.
        get_or_init(&pool.get().unwrap(), session).unwrap();
        const N: usize = 16;
        const PER: i64 = 25;
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let pool = pool.clone();
                thread::spawn(move || {
                    let conn = pool.get().unwrap();
                    for _ in 0..PER {
                        add_used(&conn, session, 1).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let final_gate = get_or_init(&pool.get().unwrap(), session).unwrap();
        assert_eq!(final_gate.used, (N as i64) * PER);
    }
}
