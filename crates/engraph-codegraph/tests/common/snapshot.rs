//! The snapshot of a fixture repo's symbol ids (plan 05, part 1): what an
//! index run of the fixture put in the database, in a form that is the same
//! on every machine.
//!
//! A snapshot records every first-party symbol the fixture defines — a
//! non-local id with a definition location — with its name, repo-relative
//! file path, line range, and the ids of its callers and callees (`CALLS`
//! relations), plus the indexer and its version. Local symbols are left out:
//! their ids carry a tag of the project key, which is a path on the machine.

use engraph_core::db::PooledConn;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub indexer: String,
    pub indexer_version: String,
    pub symbols: Vec<SymbolRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRecord {
    pub id: String,
    pub name: String,
    pub file_path: String,
    pub line_range: String,
    pub callers: Vec<String>,
    pub callees: Vec<String>,
}

impl Snapshot {
    /// The defined, non-local symbols of `project`, sorted by id, with their
    /// callers and callees sorted and deduplicated.
    pub fn from_db(
        conn: &PooledConn,
        project: &str,
        indexer: &str,
        indexer_version: &str,
    ) -> Snapshot {
        let mut defs = conn
            .prepare(
                "SELECT id, name, file_path, line_range FROM entities
                 WHERE project = ?1 AND kind = 'symbol'
                   AND file_path IS NOT NULL AND line_range IS NOT NULL
                   AND id NOT LIKE 'local %'
                 ORDER BY id",
            )
            .unwrap();
        let mut callers = conn
            .prepare(
                "SELECT DISTINCT src_entity FROM relations
                 WHERE dst_entity = ?1 AND kind = 'CALLS' ORDER BY src_entity",
            )
            .unwrap();
        let mut callees = conn
            .prepare(
                "SELECT DISTINCT dst_entity FROM relations
                 WHERE src_entity = ?1 AND kind = 'CALLS' ORDER BY dst_entity",
            )
            .unwrap();
        let ids = |stmt: &mut rusqlite::Statement, id: &str| -> Vec<String> {
            stmt.query_map([id], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let symbols = defs
            .query_map([project], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .map(|(id, name, file_path, line_range)| SymbolRecord {
                callers: ids(&mut callers, &id),
                callees: ids(&mut callees, &id),
                id,
                name,
                file_path,
                line_range,
            })
            .collect();
        Snapshot {
            indexer: indexer.to_string(),
            indexer_version: indexer_version.to_string(),
            symbols,
        }
    }

    /// The differences between `self`, the snapshot, and `actual`, an index
    /// run, one line each; empty when they are equal. A changed indexer or
    /// version comes first, then a count line, then the symbols: `-` only in
    /// the snapshot, `+` only in the index run, `~` changed, with the changed
    /// fields indented below it (`callers`/`callees` as the ids that went or
    /// came).
    pub fn diff(&self, actual: &Snapshot) -> Vec<String> {
        let mut lines = Vec::new();
        for (name, want, got) in [
            ("indexer", &self.indexer, &actual.indexer),
            (
                "indexer_version",
                &self.indexer_version,
                &actual.indexer_version,
            ),
        ] {
            if want != got {
                lines.push(format!("{name}: {want} (snapshot) -> {got} (index run)"));
            }
        }
        let want: BTreeMap<&str, &SymbolRecord> =
            self.symbols.iter().map(|s| (s.id.as_str(), s)).collect();
        let got: BTreeMap<&str, &SymbolRecord> =
            actual.symbols.iter().map(|s| (s.id.as_str(), s)).collect();
        let removed: Vec<String> = want
            .keys()
            .filter(|id| !got.contains_key(*id))
            .map(|id| format!("- {id}"))
            .collect();
        let added: Vec<String> = got
            .keys()
            .filter(|id| !want.contains_key(*id))
            .map(|id| format!("+ {id}"))
            .collect();
        let mut changed = Vec::new();
        let mut n_changed = 0;
        for (id, w) in &want {
            let Some(g) = got.get(id).filter(|g| *g != w) else {
                continue;
            };
            n_changed += 1;
            changed.push(format!("~ {id}"));
            for (name, a, b) in [
                ("name", &w.name, &g.name),
                ("file_path", &w.file_path, &g.file_path),
                ("line_range", &w.line_range, &g.line_range),
            ] {
                if a != b {
                    changed.push(format!("    {name}: {a} -> {b}"));
                }
            }
            for (name, a, b) in [
                ("callers", &w.callers, &g.callers),
                ("callees", &w.callees, &g.callees),
            ] {
                let went: Vec<&String> = a.iter().filter(|x| !b.contains(x)).collect();
                let came: Vec<&String> = b.iter().filter(|x| !a.contains(x)).collect();
                if went.is_empty() && came.is_empty() {
                    // The same ids in a different order still differ as
                    // records; without this line the symbol would show as
                    // changed with nothing below it.
                    if a != b {
                        changed.push(format!("    {name}: order changed"));
                    }
                    continue;
                }
                for x in went {
                    changed.push(format!("    {name}: - {x}"));
                }
                for x in came {
                    changed.push(format!("    {name}: + {x}"));
                }
            }
        }
        if !(removed.is_empty() && added.is_empty() && changed.is_empty()) {
            lines.push(format!(
                "only in the snapshot: {}, only in the index run: {}, changed: {n_changed}",
                removed.len(),
                added.len()
            ));
        }
        lines.extend(removed);
        lines.extend(added);
        lines.extend(changed);
        lines
    }

    pub fn read(path: &Path) -> Snapshot {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading snapshot {}: {e}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing snapshot {}: {e}", path.display()))
    }

    /// Pretty-printed JSON with a trailing newline, parent directories made.
    pub fn write(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut text = serde_json::to_string_pretty(self).unwrap();
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }
}
