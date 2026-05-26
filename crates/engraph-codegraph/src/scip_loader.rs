//! Decode a SCIP protobuf blob and load its symbols + occurrences into the
//! existing `entities` / `relations` tables.
//!
//! `entities.id` is the raw SCIP moniker (e.g.
//! `rust-analyzer cargo engraph-core 0.1.0 schema/run_migrations().`).
//! Cross-machine normalization of monikers is a Phase 2.2 problem; here we
//! trust whatever the indexer emits. Re-indexing is idempotent: we delete all
//! entities / relations scoped to `project` then re-insert from the new SCIP
//! blob, all in one transaction. Below ~10M edges this is sufficient (the
//! staging-tables-then-swap pattern from Mnemosyne is overkill at that scale).
//!
//! Relation kinds are validated against `RelationKind` enum values; there is
//! no DB-level CHECK constraint (SQLite cannot add one in-place after the
//! v2 migration created the table).

use crate::relation_kind::RelationKind;
use anyhow::{Context, Result};
use engraph_core::db::PooledConn;
use protobuf::{Message, EnumOrUnknown};
use scip::types::{symbol_information::Kind as SymKind, Document, Index, SymbolRole};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Default, Clone, Copy)]
pub struct LoadStats {
    pub entities_inserted: usize,
    pub relations_inserted: usize,
    pub documents_seen: usize,
}

/// Cross-machine moniker normalization. SCIP monikers are stable
/// cross-machine for canonically-published deps (Cargo, npm, PyPI, Go
/// modules with proper module paths), but some indexers leak machine-local
/// noise. Today this is a pass-through; Phase 2.2 work will plug in
/// per-prefix rewrite rules here (e.g. strip absolute paths from pre-0.7
/// scip-go monikers). Call this everywhere a moniker enters the DB so the
/// hook covers all entry points.
#[inline]
pub fn normalize_moniker(s: &str) -> &str {
    s
}

pub fn load(conn: &PooledConn, project: &str, scip_bytes: &[u8]) -> Result<LoadStats> {
    let index = Index::parse_from_bytes(scip_bytes).context("decoding SCIP protobuf")?;

    // Pass 1: gather every symbol's Kind across all documents (and the
    // external_symbols list). The CALLS vs REFERENCES distinction is based on
    // the *target* symbol's kind, which is only known after we've seen its
    // SymbolInformation — which may live in a different document than the
    // occurrence that references it.
    let mut sym_kind: HashMap<String, SymKind> = HashMap::new();
    for doc in &index.documents {
        for s in &doc.symbols {
            if !s.symbol.is_empty() {
                sym_kind.insert(s.symbol.clone(), enum_or_unspecified(&s.kind));
            }
        }
    }
    for s in &index.external_symbols {
        if !s.symbol.is_empty() {
            sym_kind
                .entry(s.symbol.clone())
                .or_insert_with(|| enum_or_unspecified(&s.kind));
        }
    }

    // One transaction for the whole load: atomic swap from the reader's POV.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let mut guard = TxGuard { conn, done: false };

    // Phase 2.2: scope the relations DELETE to outgoing edges only (src in
    // this project). Deleting inbound edges from other projects would break
    // cross-repo CALLS like app_b → lib_a::foo when lib_a re-indexes.
    //
    // We intentionally do NOT delete entities here. With cross-repo, another
    // project's relation may point to a symbol we're about to recompute;
    // dropping that entity would FK-fail the holding project's edge. The
    // upsert path below refreshes file_path/line_range/signature in place.
    // Stale entities from removed source code accumulate; a future GC pass
    // (Phase 2.3 territory) can prune them.
    conn.execute(
        "DELETE FROM relations
         WHERE src_entity IN (SELECT id FROM entities WHERE project = ?1)",
        [project],
    )?;

    let mut stats = LoadStats::default();
    for doc in &index.documents {
        stats.documents_seen += 1;
        load_document(conn, project, doc, &sym_kind, &mut stats)?;
    }

    guard.commit()?;
    Ok(stats)
}

fn load_document(
    conn: &PooledConn,
    project: &str,
    doc: &Document,
    sym_kind: &HashMap<String, SymKind>,
    stats: &mut LoadStats,
) -> Result<()> {
    // Insert one entity per SymbolInformation in this doc. We capture
    // file_path + line_range from the first Definition occurrence we see for
    // the symbol below. Until then file_path = doc.relative_path,
    // line_range = NULL.
    let mut def_loc: HashMap<String, (i32, i32)> = HashMap::new(); // sym -> (start_line, end_line)
    let mut def_generated: HashMap<String, bool> = HashMap::new();

    for occ in &doc.occurrences {
        let roles = occ.symbol_roles;
        let is_def = role_set(roles, SymbolRole::Definition);
        if !is_def || occ.symbol.is_empty() {
            continue;
        }
        let (start_line, end_line) = decode_range(&occ.range);
        def_loc.insert(occ.symbol.clone(), (start_line, end_line));
        if role_set(roles, SymbolRole::Generated) {
            def_generated.insert(occ.symbol.clone(), true);
        }
    }

    // Insert/upsert one entity row per SymbolInformation in this document.
    // On conflict we claim ownership: a real definition takes the `project`
    // away from any earlier placeholder. Phase 2.2 relies on this so that
    // `entities.project` always names the *defining* repo, never the first
    // referrer.
    let mut entity_insert = conn.prepare_cached(
        "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
         VALUES (?1, 'symbol', ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
            project = excluded.project,
            file_path = COALESCE(excluded.file_path, entities.file_path),
            line_range = COALESCE(excluded.line_range, entities.line_range),
            signature = COALESCE(excluded.signature, entities.signature)",
    )?;
    for s in &doc.symbols {
        if s.symbol.is_empty() {
            continue;
        }
        let display_name = if !s.display_name.is_empty() {
            s.display_name.clone()
        } else {
            fallback_display_name(&s.symbol)
        };
        let (file_path, line_range) = match def_loc.get(&s.symbol) {
            Some((start, end)) => (
                Some(doc.relative_path.clone()),
                Some(format!("{}:{}", start + 1, end + 1)),
            ),
            None => (Some(doc.relative_path.clone()), None),
        };
        let signature = signature_text(s);
        entity_insert.execute(rusqlite::params![
            &s.symbol,
            &display_name,
            project,
            file_path,
            line_range,
            signature,
        ])?;
        stats.entities_inserted += 1;

        // Relationship-derived edges. is_implementation → IMPLEMENTS,
        // is_reference → EXTENDS only when not also is_implementation (avoids
        // double-counting; SCIP marks parent-class relationships as is_reference).
        for rel in &s.relationships {
            if rel.symbol.is_empty() {
                continue;
            }
            ensure_placeholder_entity(conn, &rel.symbol, project)?;
            if rel.is_implementation {
                insert_relation(conn, &s.symbol, &rel.symbol, RelationKind::Implements, "extracted")?;
                stats.relations_inserted += 1;
            } else if rel.is_reference {
                insert_relation(conn, &s.symbol, &rel.symbol, RelationKind::Extends, "extracted")?;
                stats.relations_inserted += 1;
            }
        }
    }

    // Occurrence-derived edges (CALLS / REFERENCES / IMPORTS). DEFINES is
    // implicit in the entity row's file_path/line_range; we don't materialize
    // a DEFINES self-loop.
    // We need a defining symbol "anchor" for each occurrence — the enclosing
    // function/class. SCIP doesn't tag occurrences with their enclosing
    // definition directly, so we attribute each non-definition occurrence to
    // the nearest preceding definition in the same document by start_line —
    // but only definitions whose Kind is "anchorable" (function, method,
    // class, etc.). Without this filter the heuristic latches onto local
    // variables and produces nonsensical "Called by `conn` in budget.rs"
    // lines.
    // Anchors are tuples of (start_line, specificity, symbol). When several
    // definitions start on the same line — common at the top of a file where
    // the `crate/` module def shares line 0 with the first function def —
    // ties are broken by specificity so a Function anchor wins over the
    // enclosing Module. Without this tiebreak the `nearest_enclosing` search
    // can attribute a call to `crate/` instead of `app_caller`.
    let mut def_anchors: Vec<(i32, u8, String)> = def_loc
        .iter()
        .filter_map(|(sym, (start, _))| {
            let kind = sym_kind.get(sym).copied();
            let spec = anchor_specificity(kind);
            if spec == 0 {
                None
            } else {
                Some((*start, spec, sym.clone()))
            }
        })
        .collect();
    def_anchors.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    for occ in &doc.occurrences {
        if occ.symbol.is_empty() {
            continue;
        }
        let roles = occ.symbol_roles;
        if role_set(roles, SymbolRole::Definition) {
            continue;
        }
        let (start_line, _) = decode_range(&occ.range);
        let Some(src_sym) = nearest_enclosing(&def_anchors, start_line) else {
            continue;
        };
        // The target symbol may live in another document or be external.
        // Insert a placeholder entity row for it so the FK holds; the real
        // row (if present elsewhere in this Index) will overwrite via
        // ON CONFLICT.
        ensure_placeholder_entity(conn, &occ.symbol, project)?;

        let target_kind = sym_kind.get(&occ.symbol).copied();
        let provenance = if role_set(roles, SymbolRole::Generated) {
            "generated"
        } else {
            "extracted"
        };

        let rel_kind = if role_set(roles, SymbolRole::Import) {
            RelationKind::Imports
        } else if is_callable_kind(target_kind) {
            RelationKind::Calls
        } else if is_type_kind(target_kind) {
            RelationKind::References
        } else {
            // Target kind is missing from SymbolInformation (common for
            // cross-crate refs when only one crate is indexed) or refers to
            // a local. Fall back to the SCIP descriptor suffix:
            //   `()` / `().` → function/method (CALLS)
            //   `#`          → class/struct/interface/trait/enum (REFERENCES)
            //   other        → skip (locals + low-signal term refs)
            match suffix_kind(&occ.symbol) {
                Some(RelationKind::Calls) => RelationKind::Calls,
                Some(RelationKind::References) => RelationKind::References,
                _ => continue,
            }
        };

        if src_sym == occ.symbol {
            // Skip trivial self-loops (e.g. recursive functions reference
            // their own name in their body); they bloat the graph without
            // adding signal.
            continue;
        }
        insert_relation(conn, &src_sym, &occ.symbol, rel_kind, provenance)?;
        stats.relations_inserted += 1;
    }

    Ok(())
}

fn nearest_enclosing(def_anchors: &[(i32, u8, String)], occ_line: i32) -> Option<String> {
    // Find the anchor with maximum start_line ≤ occ_line, breaking line ties
    // by higher specificity. `def_anchors` is sorted (line ASC, spec ASC), so
    // a linear scan from the end picks the right one in O(n) — fine for
    // per-document anchor sets.
    let mut best: Option<(i32, u8, &str)> = None;
    for (line, spec, sym) in def_anchors {
        if *line > occ_line {
            break;
        }
        match best {
            None => best = Some((*line, *spec, sym.as_str())),
            Some((bl, bs, _)) => {
                if *line > bl || (*line == bl && *spec >= bs) {
                    best = Some((*line, *spec, sym.as_str()));
                }
            }
        }
    }
    best.map(|(_, _, s)| s.to_string())
}

fn insert_relation(
    conn: &PooledConn,
    src: &str,
    dst: &str,
    kind: RelationKind,
    provenance: &str,
) -> Result<()> {
    let id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO relations (id, src_entity, dst_entity, kind, provenance, confidence)
         VALUES (?1, ?2, ?3, ?4, ?5, 1.0)",
        rusqlite::params![id, src, dst, kind.as_str(), provenance],
    )?;
    Ok(())
}

/// Insert a stub entity row for a referenced symbol that we haven't seen a
/// SymbolInformation for in this Index. Keeps FK satisfied; later runs that
/// do see the SymbolInformation will overwrite via ON CONFLICT.
fn ensure_placeholder_entity(conn: &PooledConn, sym: &str, project: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO entities (id, kind, name, project)
         VALUES (?1, 'symbol', ?2, ?3)",
        rusqlite::params![sym, fallback_display_name(sym), project],
    )?;
    Ok(())
}

fn role_set(bitset: i32, role: SymbolRole) -> bool {
    (bitset & (role as i32)) != 0
}

fn enum_or_unspecified(e: &EnumOrUnknown<SymKind>) -> SymKind {
    e.enum_value().unwrap_or(SymKind::UnspecifiedKind)
}

fn is_callable_kind(k: Option<SymKind>) -> bool {
    matches!(
        k,
        Some(SymKind::Function)
            | Some(SymKind::Method)
            | Some(SymKind::Constructor)
            | Some(SymKind::StaticMethod)
            | Some(SymKind::AbstractMethod)
            | Some(SymKind::Macro)
    )
}

/// Specificity score for an anchor candidate. Higher = inner / more specific.
/// 0 means "not an anchor" (locals, fields, unknown). The relative ordering
/// matters more than the absolute numbers — Function beats Module so calls
/// inside a function at line 0 anchor to the function, not to the enclosing
/// crate module.
fn anchor_specificity(k: Option<SymKind>) -> u8 {
    match k {
        Some(SymKind::Method)
        | Some(SymKind::StaticMethod)
        | Some(SymKind::Constructor)
        | Some(SymKind::AbstractMethod)
        | Some(SymKind::Function)
        | Some(SymKind::Macro) => 100,
        Some(SymKind::Class)
        | Some(SymKind::Struct)
        | Some(SymKind::Interface)
        | Some(SymKind::Trait)
        | Some(SymKind::Enum) => 50,
        Some(SymKind::Module) | Some(SymKind::Namespace) | Some(SymKind::Package) => 10,
        _ => 0,
    }
}

fn is_type_kind(k: Option<SymKind>) -> bool {
    matches!(
        k,
        Some(SymKind::Class)
            | Some(SymKind::Struct)
            | Some(SymKind::Interface)
            | Some(SymKind::Trait)
            | Some(SymKind::Enum)
            | Some(SymKind::TypeAlias)
            | Some(SymKind::Type)
            | Some(SymKind::Protocol)
    )
}

/// Last-resort kind guess from a SCIP symbol's descriptor suffix. SCIP
/// descriptors encode kind in their trailing punctuation:
///   - `()` or `().` → method / function
///   - `#`           → class / struct / interface / trait / enum / typealias
///   - everything else (`. `, `:`, `[…]`, `local …`) → unknown / low-signal
///
/// This is used when `SymbolInformation.kind` isn't available, which is
/// common for cross-crate references when the indexer was pointed at only
/// the consumer crate. Without this fallback the loader drops cross-crate
/// CALLS edges entirely (the Phase 2.2 cross-repo regression we hit).
fn suffix_kind(symbol: &str) -> Option<RelationKind> {
    // Skip locals — they're per-document and never navigable cross-repo.
    if symbol.starts_with("local ") {
        return None;
    }
    // Trim a trailing `.` (definition disambiguator) so `foo().` matches.
    let trimmed = symbol.trim_end_matches('.');
    if trimmed.ends_with(')') {
        Some(RelationKind::Calls)
    } else if trimmed.ends_with('#') {
        Some(RelationKind::References)
    } else {
        None
    }
}

fn decode_range(range: &[i32]) -> (i32, i32) {
    // SCIP range is one of:
    //   [startLine, startChar, endLine, endChar]  (4 elements)
    //   [startLine, startChar, endChar]           (3 elements, endLine = startLine)
    match range.len() {
        4 => (range[0], range[2]),
        3 => (range[0], range[0]),
        _ => (0, 0),
    }
}

fn signature_text(s: &scip::types::SymbolInformation) -> Option<String> {
    let sig = s.signature_documentation.as_ref()?;
    if sig.text.is_empty() {
        None
    } else {
        Some(sig.text.clone())
    }
}

fn fallback_display_name(moniker: &str) -> String {
    // SCIP moniker structure: "scheme manager package descriptors". The last
    // descriptor token (e.g. `run_migrations().`) is a reasonable display
    // name fallback when SymbolInformation.display_name is empty.
    moniker
        .rsplit_once(' ')
        .map(|(_, last)| {
            last.trim_end_matches('.')
                .trim_end_matches(')')
                .trim_end_matches('(')
                .rsplit('/')
                .next()
                .unwrap_or(last)
                .to_string()
        })
        .unwrap_or_else(|| moniker.to_string())
}

/// RAII transaction guard mirroring engraph-ingest's TxGuard: rolls back on
/// drop unless commit() was called, so an early `?` propagation never leaves
/// a pooled connection with an open txn.
struct TxGuard<'a> {
    conn: &'a PooledConn,
    done: bool,
}

impl TxGuard<'_> {
    fn commit(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        self.done = true;
        Ok(())
    }
}

impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_range_handles_three_and_four_element() {
        assert_eq!(decode_range(&[5, 0, 10, 4]), (5, 10));
        assert_eq!(decode_range(&[5, 0, 4]), (5, 5));
        assert_eq!(decode_range(&[]), (0, 0));
    }

    #[test]
    fn role_set_reads_bits() {
        let bits = SymbolRole::Definition as i32 | SymbolRole::Import as i32;
        assert!(role_set(bits, SymbolRole::Definition));
        assert!(role_set(bits, SymbolRole::Import));
        assert!(!role_set(bits, SymbolRole::Generated));
    }

    #[test]
    fn suffix_kind_classifies_descriptors() {
        assert_eq!(
            suffix_kind("rust-analyzer cargo lib 0.1.0 mod/foo()."),
            Some(RelationKind::Calls)
        );
        assert_eq!(
            suffix_kind("rust-analyzer cargo lib 0.1.0 mod/foo()"),
            Some(RelationKind::Calls)
        );
        assert_eq!(
            suffix_kind("rust-analyzer cargo lib 0.1.0 mod/MyType#"),
            Some(RelationKind::References)
        );
        // Term/constant (`.` suffix only) → unknown, drop.
        assert_eq!(suffix_kind("rust-analyzer cargo lib 0.1.0 mod/MAX."), None);
        // Local → never emit.
        assert_eq!(suffix_kind("local 5"), None);
    }

    #[test]
    fn fallback_display_name_strips_descriptor_punct() {
        let n = fallback_display_name("rust-analyzer cargo engraph-core 0.1.0 schema/run_migrations().");
        assert_eq!(n, "run_migrations");
    }
}
