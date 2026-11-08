//! Decode a SCIP protobuf blob and load its symbols + occurrences into the
//! existing `entities` / `relations` tables.
//!
//! `entities.id` is the raw SCIP moniker (e.g.
//! `rust-analyzer cargo engraph-core 0.1.0 schema/run_migrations().`).
//! Cross-machine normalization of monikers is a Phase 2.2 problem; here we
//! trust whatever the indexer emits. Re-indexing is idempotent: we replace the
//! project's SCIP-derived relations, upsert current symbols, and prune stale
//! unreferenced symbol rows absent from the new SCIP blob, all in one
//! transaction. Below ~10M edges this is sufficient (the staging-tables-then-swap
//! pattern from Mnemosyne is overkill at that scale).
//!
//! Relation kinds are validated against `RelationKind` enum values; there is
//! no DB-level CHECK constraint (SQLite cannot add one in-place after the
//! v2 migration created the table).

use crate::relation_kind::RelationKind;
use anyhow::{Context, Result};
use engraph_core::db::PooledConn;
use protobuf::{EnumOrUnknown, Message};
use scip::types::{Document, Index, SymbolRole, symbol_information::Kind as SymKind};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Default, Clone, Copy)]
pub struct LoadStats {
    pub entities_inserted: usize,
    pub relations_inserted: usize,
    pub documents_seen: usize,
    pub entities_pruned: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct LoadOptions<'a> {
    /// Garbage-collect in-scope entities no relation references before the
    /// load, and delete in-scope symbol rows absent from the new SCIP blob
    /// after it replaces relations. Safe current edgeless symbols are kept
    /// because the load re-creates them.
    pub prune_stale: bool,
    /// Repo-relative directory this load refreshes; empty for the whole
    /// project. Document paths must already carry it as a prefix.
    pub subtree: &'a str,
}

impl Default for LoadOptions<'_> {
    fn default() -> Self {
        Self {
            prune_stale: true,
            subtree: "",
        }
    }
}

/// The rows of `?1`'s project that a load may replace: symbols from the SCIP
/// schemes (indexers) in this load — so another indexer's symbols survive —
/// plus scheme-less legacy/placeholder rows, under the `?2` path prefix (empty
/// for the whole project). A local symbol has no scheme in its id; its
/// `scheme` column (the scheme of its document) decides. Unqualified columns,
/// for `entities` statements.
const IN_SCOPE: &str = "project = ?1
    AND (instr(id, ' ') = 0
         OR substr(id, 1, instr(id, ' ') - 1) IN (SELECT s FROM current_scip_schemes)
         OR (id LIKE 'local %' AND scheme IN (SELECT s FROM current_scip_schemes)))
    AND (?2 = '' OR substr(file_path, 1, length(?2)) = ?2)";

/// The SCIP scheme (indexer name) a symbol starts with; `None` for
/// document-local symbols.
fn scheme(symbol: &str) -> Option<&str> {
    symbol
        .split_once(' ')
        .map(|(s, _)| s)
        .filter(|s| *s != "local")
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

/// A SCIP local symbol (`local 0`) names a symbol only inside its document,
/// and `entities.id` is one key for the whole database. Give each local the
/// id `local <n>@<project tag>:<relative_path>`, so the `local 0` of two
/// documents, or of two projects with the same file path, are different rows.
/// The tag is [`project_tag`]. Changes the id in the document's symbols, their
/// relationships, and its occurrences; the name still comes from the original
/// id (`symbol_names` ignores the scope). The `local ` prefix stays, so
/// `suffix_kind` still skips locals.
fn scope_local_symbols(index: &mut Index, project: &str) {
    let tag = project_tag(project);
    for doc in &mut index.documents {
        let scope = |symbol: &mut String| {
            if symbol.starts_with("local ") && !symbol.contains('@') {
                symbol.push_str(&format!("@{tag}:{}", doc.relative_path));
            }
        };
        for info in &mut doc.symbols {
            scope(&mut info.symbol);
            for rel in &mut info.relationships {
                scope(&mut rel.symbol);
            }
        }
        for occ in &mut doc.occurrences {
            scope(&mut occ.symbol);
        }
    }
}

/// The first 16 hex digits of the SHA-256 of a project key: a short, fixed
/// part of a local symbol id that differs between projects.
fn project_tag(project: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(project.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `local 7@<tag>:src/a.rs` → `local 7`: a local symbol id as the indexer
/// wrote it. Other ids are unchanged.
fn unscoped_local(symbol: &str) -> &str {
    if symbol.starts_with("local ") {
        symbol.split_once('@').map_or(symbol, |(id, _)| id)
    } else {
        symbol
    }
}

pub fn load(conn: &PooledConn, project: &str, scip_bytes: &[u8]) -> Result<LoadStats> {
    load_with_options(conn, project, scip_bytes, LoadOptions::default())
}

pub fn load_with_options(
    conn: &PooledConn,
    project: &str,
    scip_bytes: &[u8],
    options: LoadOptions,
) -> Result<LoadStats> {
    let mut index = Index::parse_from_bytes(scip_bytes).context("decoding SCIP protobuf")?;
    scope_local_symbols(&mut index, project);

    // Pass 1: gather every symbol's Kind across all documents (and the
    // external_symbols list). The CALLS vs REFERENCES distinction is based on
    // the *target* symbol's kind, which is only known after we've seen its
    // SymbolInformation — which may live in a different document than the
    // occurrence that references it.
    let mut sym_kind: HashMap<String, SymKind> = HashMap::new();
    let mut current_symbols: HashSet<String> = HashSet::new();
    for doc in &index.documents {
        for s in &doc.symbols {
            if !s.symbol.is_empty() {
                current_symbols.insert(s.symbol.clone());
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

    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS current_scip_symbols (
            id TEXT PRIMARY KEY
        ) WITHOUT ROWID;
        CREATE TEMP TABLE IF NOT EXISTS current_scip_schemes (
            s TEXT PRIMARY KEY
        ) WITHOUT ROWID;
        DELETE FROM current_scip_symbols;
        DELETE FROM current_scip_schemes;",
    )?;
    {
        let mut ins_sym =
            conn.prepare_cached("INSERT OR IGNORE INTO current_scip_symbols (id) VALUES (?1)")?;
        let mut ins_scheme =
            conn.prepare_cached("INSERT OR IGNORE INTO current_scip_schemes (s) VALUES (?1)")?;
        for sym in &current_symbols {
            ins_sym.execute([sym.as_str()])?;
            if let Some(s) = scheme(sym) {
                ins_scheme.execute([s])?;
            }
        }
    }
    let prefix = if options.subtree.is_empty() {
        String::new()
    } else {
        format!("{}/", options.subtree.trim_end_matches('/'))
    };

    let mut stats = LoadStats::default();
    // Pre-load GC: in-scope rows nothing references. The load re-creates any
    // that are still current, so this only removes genuinely deleted ones.
    if options.prune_stale {
        stats.entities_pruned = conn.execute(
            &format!(
                "DELETE FROM entities
                 WHERE {IN_SCOPE}
                   AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.src_entity = entities.id)
                   AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.dst_entity = entities.id)"
            ),
            rusqlite::params![project, prefix],
        )?;
    }

    // Phase 2.2: scope the relations DELETE to outgoing edges only (src in
    // this project). Deleting inbound edges from other projects would break
    // cross-repo CALLS like app_b → lib_a::foo when lib_a re-indexes.
    //
    // Phase 2.3 #2: exclude BAZEL_DEPENDS_ON. The target-level Bazel pass
    // writes those edges under the same project; a SCIP load running after
    // it (symbol-level pass) must not wipe them. `!= 'BAZEL_DEPENDS_ON'` is
    // chosen over an explicit kind-IN-list so a future symbol-level kind
    // (e.g. OVERRIDES) auto-cycles correctly on re-index.
    //
    // We intentionally do NOT bulk-delete entities here. With cross-repo,
    // another project's relation may point to a symbol we're about to recompute;
    // dropping that entity would FK-fail the holding project's edge. The upsert
    // path below refreshes current symbols in place, and the stale-prune pass
    // below deletes only symbols absent from this SCIP blob and unreferenced by
    // any remaining relation.
    //
    // Only in-scope sources (IN_SCOPE: this load's indexers and subtree), so a
    // partial refresh keeps the rest of the project's edges — plus every
    // source this blob defines, wherever it's filed now, so a symbol another
    // run filed under another key or path doesn't keep its old edges too.
    conn.execute(
        &format!(
            "DELETE FROM relations
             WHERE kind != 'BAZEL_DEPENDS_ON'
               AND (src_entity IN (SELECT id FROM current_scip_symbols)
                    OR src_entity IN (SELECT id FROM entities WHERE {IN_SCOPE}))"
        ),
        rusqlite::params![project, prefix],
    )?;

    for doc in &index.documents {
        stats.documents_seen += 1;
        load_document(conn, project, doc, &sym_kind, &mut stats)?;
    }
    if options.prune_stale {
        stats.entities_pruned += prune_stale_symbols(conn, project, &prefix)?;
    }
    conn.execute_batch("DELETE FROM current_scip_symbols; DELETE FROM current_scip_schemes;")?;

    guard.commit()?;
    Ok(stats)
}

/// Delete in-scope symbols absent from the load's `current_scip_symbols` that
/// no relation references.
fn prune_stale_symbols(conn: &PooledConn, project: &str, prefix: &str) -> Result<usize> {
    let pruned = conn.execute(
        &format!(
            "DELETE FROM entities
             WHERE {IN_SCOPE}
               AND kind = 'symbol'
               AND NOT EXISTS (
                 SELECT 1 FROM current_scip_symbols c WHERE c.id = entities.id
               )
               AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.src_entity = entities.id)
               AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.dst_entity = entities.id)"
        ),
        rusqlite::params![project, prefix],
    )?;
    Ok(pruned)
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
    let doc_scheme = document_scheme(doc);
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
    // referrer. The name is replaced too: the existing row may be a
    // placeholder a reference wrote (`ensure_placeholder_entity`, which only
    // guesses from the id), or a row an older loader named differently.
    let mut entity_insert = conn.prepare_cached(
        "INSERT INTO entities (id, kind, name, qualified_name, project, file_path, line_range, signature, scheme)
         VALUES (?1, 'symbol', ?2, ?7, ?3, ?4, ?5, ?6, ?8)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            qualified_name = excluded.qualified_name,
            project = excluded.project,
            scheme = COALESCE(excluded.scheme, entities.scheme),
            file_path = COALESCE(excluded.file_path, entities.file_path),
            line_range = COALESCE(excluded.line_range, entities.line_range),
            signature = COALESCE(excluded.signature, entities.signature)",
    )?;
    for s in &doc.symbols {
        if s.symbol.is_empty() {
            continue;
        }
        let names = symbol_names(&s.symbol);
        let display_name = if !s.display_name.is_empty() {
            s.display_name.clone()
        } else {
            names.name
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
            names.qualified,
            symbol_scheme(&s.symbol, doc_scheme),
        ])?;
        stats.entities_inserted += 1;

        // Relationship-derived edges. is_implementation → IMPLEMENTS,
        // is_reference → EXTENDS only when not also is_implementation (avoids
        // double-counting; SCIP marks parent-class relationships as is_reference).
        for rel in &s.relationships {
            if rel.symbol.is_empty() {
                continue;
            }
            ensure_placeholder_entity(conn, &rel.symbol, project, doc_scheme)?;
            if rel.is_implementation {
                insert_relation(
                    conn,
                    &s.symbol,
                    &rel.symbol,
                    RelationKind::Implements,
                    "extracted",
                )?;
                stats.relations_inserted += 1;
            } else if rel.is_reference {
                insert_relation(
                    conn,
                    &s.symbol,
                    &rel.symbol,
                    RelationKind::Extends,
                    "extracted",
                )?;
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
            let spec = anchor_specificity(kind, sym);
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
        ensure_placeholder_entity(conn, &occ.symbol, project, doc_scheme)?;

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
fn ensure_placeholder_entity(
    conn: &PooledConn,
    sym: &str,
    project: &str,
    doc_scheme: Option<&str>,
) -> Result<()> {
    let names = symbol_names(sym);
    conn.execute(
        "INSERT OR IGNORE INTO entities (id, kind, name, qualified_name, project, scheme)
         VALUES (?1, 'symbol', ?2, ?3, ?4, ?5)",
        rusqlite::params![
            sym,
            names.name,
            names.qualified,
            project,
            symbol_scheme(sym, doc_scheme)
        ],
    )?;
    Ok(())
}

/// The scheme (indexer) of a document: that of its first global symbol, in
/// its symbols, then in its occurrences (references to other packages carry
/// the indexer's scheme too). `None` when the document has only locals.
fn document_scheme(doc: &Document) -> Option<&str> {
    doc.symbols
        .iter()
        .map(|s| s.symbol.as_str())
        .chain(doc.occurrences.iter().map(|o| o.symbol.as_str()))
        .find_map(scheme)
}

/// The `entities.scheme` of a symbol: the first word of a global id, and the
/// scheme of the document for a local.
fn symbol_scheme<'a>(symbol: &'a str, doc_scheme: Option<&'a str>) -> Option<&'a str> {
    if symbol.starts_with("local ") {
        doc_scheme
    } else {
        scheme(symbol)
    }
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
///
/// When the indexer sets no kind (scip-python sets none), the suffix of the
/// symbol's last descriptor gives it: a method (`…().`) is 100, a type (`…#`)
/// 50, a namespace or package (`…/`) 10. Parameters, terms, locals, and ids
/// the SCIP grammar rejects are not anchors.
fn anchor_specificity(k: Option<SymKind>, symbol: &str) -> u8 {
    use scip::types::descriptor::Suffix;
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
        None | Some(SymKind::UnspecifiedKind) => match last_suffix(symbol) {
            Some(Suffix::Method) => 100,
            Some(Suffix::Type) => 50,
            // Namespace and Package share one enum value in SCIP.
            Some(Suffix::Namespace) => 10,
            _ => 0,
        },
        _ => 0,
    }
}

/// The suffix of a symbol's last descriptor (`Method` for `…/C#m().`,
/// `Parameter` for `…/C#m().(self)`); `None` when the id does not follow the
/// SCIP grammar.
fn last_suffix(symbol: &str) -> Option<scip::types::descriptor::Suffix> {
    parsed_descriptors(symbol)?.last()?.suffix.enum_value().ok()
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
/// descriptors encode kind in their trailing punctuation; the suffix of the
/// last descriptor (`parse_symbol`) gives:
///   - method (`().`)  → CALLS (function / method)
///   - type (`#`)      → REFERENCES (class / struct / interface / trait / enum)
///   - everything else → no relation: terms (`.`), parameters (`(x)`), type
///     parameters (`[T]`), meta (`:`), macros (`!`), and locals
///
/// A parameter id also ends with `)` (`…/C#m().(self)`), so the grammar, not
/// the last character, has to tell it from a method. Ids the grammar rejects
/// fall back to that textual check.
///
/// This is used when `SymbolInformation.kind` isn't available: scip-python
/// sets no kind at all, and cross-crate references often have none when the
/// indexer was pointed at only the consumer crate. Without this fallback the
/// loader drops cross-crate CALLS edges entirely (the Phase 2.2 cross-repo
/// regression we hit).
fn suffix_kind(symbol: &str) -> Option<RelationKind> {
    use scip::types::descriptor::Suffix;
    // Skip locals — they're per-document and never navigable cross-repo.
    if symbol.starts_with("local ") {
        return None;
    }
    if let Some(descriptors) = parsed_descriptors(symbol) {
        return match descriptors.last().and_then(|d| d.suffix.enum_value().ok()) {
            Some(Suffix::Method) => Some(RelationKind::Calls),
            Some(Suffix::Type) => Some(RelationKind::References),
            _ => None,
        };
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

/// The descriptors of a SCIP symbol, parsed by the `scip` crate's grammar
/// (`scheme manager package version descriptors…`). `None` when the id does
/// not follow the grammar, so callers can fall back to a textual guess.
fn parsed_descriptors(moniker: &str) -> Option<Vec<scip::types::Descriptor>> {
    scip::symbol::parse_symbol(moniker)
        .ok()
        .map(|s| s.descriptors)
        .filter(|d| !d.is_empty())
}

/// The names engraph derives from a SCIP symbol id.
struct SymbolNames {
    /// The name when `SymbolInformation.display_name` is empty (rust-analyzer
    /// and scip-python never set it): the name of the last descriptor,
    /// without its suffix character — `Config` for `…/Config#`,
    /// `full_path_base` for `…/Config#full_path_base().`. Ids the SCIP
    /// grammar rejects fall back to trimming the punctuation by hand.
    name: String,
    /// The descriptors after the namespace, joined with `.`:
    /// `Config.full_path_base` for `…/Config#full_path_base().`, `Config` for
    /// `…/Config#`, `load` for `…/load().`. `None` when the id does not follow
    /// the SCIP grammar, and for a local symbol (`local 129`): its number is
    /// not a name, and a local is not addressable by a path.
    qualified: Option<String>,
}

/// Both names from one parse of the id.
fn symbol_names(moniker: &str) -> SymbolNames {
    use scip::types::descriptor::Suffix;
    let moniker = unscoped_local(moniker);
    let Some(descriptors) = parsed_descriptors(moniker) else {
        return SymbolNames {
            name: fallback_display_name_textual(moniker),
            qualified: None,
        };
    };
    let name = match descriptors.last() {
        Some(last) if !last.name.is_empty() => last.name.clone(),
        _ => fallback_display_name_textual(moniker),
    };
    if scip::symbol::is_local_symbol(moniker) {
        return SymbolNames {
            name,
            qualified: None,
        };
    }
    // rust-analyzer files the members of an impl block under a type
    // descriptor named `impl` and its type parameters: the self type, then
    // the trait (`walk/impl#[Batch]new().`,
    // `regex_helper/impl#[UppercaseVisitor][Visitor]finish().`). The path
    // uses the self type (`Batch.new`, `UppercaseVisitor.finish`). The impl
    // block itself (`impl#[Batch]`) is not the type: it is named `impl Batch`
    // (`impl Visitor for UppercaseVisitor`) so it does not answer a lookup
    // of `Batch`, and it has no path.
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < descriptors.len() {
        let d = &descriptors[i];
        let suffix = d.suffix.enum_value().ok();
        if matches!(suffix, Some(Suffix::Type)) && d.name == "impl" {
            let params: Vec<&str> = descriptors[i + 1..]
                .iter()
                .take_while(|p| matches!(p.suffix.enum_value().ok(), Some(Suffix::TypeParameter)))
                .map(|p| p.name.as_str())
                .collect();
            if let Some(self_type) = params.first() {
                if i + 1 + params.len() == descriptors.len() {
                    let block = match params.get(1) {
                        Some(trait_name) => {
                            format!(
                                "impl {} for {}",
                                without_generics(trait_name),
                                without_generics(self_type)
                            )
                        }
                        None => format!("impl {}", without_generics(self_type)),
                    };
                    return SymbolNames {
                        name: block,
                        qualified: None,
                    };
                }
                parts.push(without_generics(self_type).to_string());
                i += 1 + params.len();
                continue;
            }
        }
        // Namespace and Package share one enum value in SCIP.
        if !matches!(suffix, Some(Suffix::Namespace) | Some(Suffix::Package)) && !d.name.is_empty()
        {
            parts.push(d.name.clone());
        }
        i += 1;
    }
    let qualified = if parts.is_empty() {
        Some(name.clone())
    } else {
        Some(parts.join("."))
    };
    SymbolNames { name, qualified }
}

/// `ReceiverBuffer<'a, W>` → `ReceiverBuffer`: a type's generic arguments are
/// not part of its name.
fn without_generics(type_name: &str) -> &str {
    type_name
        .split_once('<')
        .map(|(head, _)| head)
        .unwrap_or(type_name)
        .trim()
}

/// Last-resort name: the last descriptor token with its trailing `().`
/// punctuation removed. Keeps `#` and `!`, since it cannot tell a suffix from
/// a name character without the grammar.
fn fallback_display_name_textual(moniker: &str) -> String {
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

    fn fallback_display_name(moniker: &str) -> String {
        symbol_names(moniker).name
    }

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
    fn anchor_specificity_uses_the_descriptor_suffix_without_a_kind() {
        let py = |d: &str| format!("scip-python python demo 0 `pkg.mod`/{d}");
        for kind in [None, Some(SymKind::UnspecifiedKind)] {
            assert_eq!(anchor_specificity(kind, &py("C#m().")), 100);
            assert_eq!(anchor_specificity(kind, &py("f().")), 100);
            assert_eq!(anchor_specificity(kind, &py("C#")), 50);
            assert_eq!(
                anchor_specificity(kind, "rust-analyzer cargo demo 0.1.0 config/"),
                10
            );
            assert_eq!(anchor_specificity(kind, &py("C#m().(self)")), 0);
            assert_eq!(anchor_specificity(kind, &py("C#attr.")), 0);
            assert_eq!(anchor_specificity(kind, &py("__init__:")), 0);
            assert_eq!(anchor_specificity(kind, "local 3"), 0);
            assert_eq!(
                anchor_specificity(kind, "local 3@0123456789abcdef:src/a.py"),
                0
            );
            assert_eq!(anchor_specificity(kind, "not a scip symbol"), 0);
        }
        // A kind the indexer set wins over the suffix.
        assert_eq!(
            anchor_specificity(Some(SymKind::Variable), &py("C#m().")),
            0
        );
        assert_eq!(anchor_specificity(Some(SymKind::Function), &py("x.")), 100);
    }

    #[test]
    fn scope_local_symbols_adds_the_project_and_the_document() {
        use scip::types::{Occurrence, Relationship, SymbolInformation};
        let global = "scip-java maven . . pkg/C#run().";
        let mut info = SymbolInformation::new();
        info.symbol = "local 3".into();
        let mut rel = Relationship::new();
        rel.symbol = "local 4".into();
        info.relationships.push(rel);
        let mut rel = Relationship::new();
        rel.symbol = global.into();
        info.relationships.push(rel);
        let mut doc = Document::new();
        doc.relative_path = "src/A.java".into();
        doc.symbols.push(info);
        for symbol in ["local 3", global] {
            let mut occ = Occurrence::new();
            occ.symbol = symbol.into();
            doc.occurrences.push(occ);
        }
        let mut index = Index::new();
        index.documents.push(doc);

        scope_local_symbols(&mut index, "/home/me/repo");
        let tag = project_tag("/home/me/repo");
        assert_eq!(tag.len(), 16);
        assert_ne!(tag, project_tag("/home/me/other"));
        let doc = &index.documents[0];
        let local3 = format!("local 3@{tag}:src/A.java");
        assert_eq!(doc.symbols[0].symbol, local3);
        assert_eq!(
            doc.symbols[0].relationships[0].symbol,
            format!("local 4@{tag}:src/A.java")
        );
        assert_eq!(doc.symbols[0].relationships[1].symbol, global);
        assert_eq!(doc.occurrences[0].symbol, local3);
        assert_eq!(doc.occurrences[1].symbol, global);

        // A second pass does not scope an id again.
        scope_local_symbols(&mut index, "/home/me/repo");
        assert_eq!(index.documents[0].symbols[0].symbol, local3);
    }

    #[test]
    fn a_scoped_local_keeps_the_name_of_its_original_id() {
        let n = symbol_names("local 129@0123456789abcdef:src/a.ts");
        assert_eq!((n.name.as_str(), n.qualified), ("129", None));
        assert_eq!(unscoped_local("local 1@0123456789abcdef:a.rs"), "local 1");
        assert_eq!(
            unscoped_local("scip-x a b c x@y/f()."),
            "scip-x a b c x@y/f()."
        );
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
        assert_eq!(suffix_kind("local 5@0123456789abcdef:src/a.ts"), None);
        // A parameter id also ends with `)`, but it is not a call.
        assert_eq!(
            suffix_kind("scip-python python demo 0 `pkg.mod`/C#m().(self)"),
            None
        );
        assert_eq!(
            suffix_kind("scip-typescript npm demo 1.0.0 src/`a.ts`/f().(x)"),
            None
        );
        // Python methods, functions, and types.
        assert_eq!(
            suffix_kind("scip-python python demo 0 `pkg.mod`/C#m()."),
            Some(RelationKind::Calls)
        );
        assert_eq!(
            suffix_kind("scip-python python demo 0 `pkg.mod`/C#"),
            Some(RelationKind::References)
        );
        // A module (meta), a type parameter, and a macro give no relation.
        assert_eq!(
            suffix_kind("scip-python python demo 0 `pkg.mod`/__init__:"),
            None
        );
        assert_eq!(
            suffix_kind("rust-analyzer cargo lib 0.1.0 mod/f().[T]"),
            None
        );
        assert_eq!(suffix_kind("rust-analyzer cargo lib 0.1.0 mod/m!"), None);
    }

    #[test]
    fn fallback_display_name_strips_descriptor_punct() {
        let n = fallback_display_name(
            "rust-analyzer cargo engraph-core 0.1.0 schema/run_migrations().",
        );
        assert_eq!(n, "run_migrations");
    }

    #[test]
    fn fallback_display_name_is_the_last_descriptor_name() {
        // A type (`#` suffix) and a method on a type (`Type#method().`):
        // rust-analyzer and scip-python set no display_name, so these ids
        // are the only source of the name.
        assert_eq!(
            fallback_display_name("rust-analyzer cargo fd 0.1.0 config/Config#"),
            "Config"
        );
        assert_eq!(
            fallback_display_name("rust-analyzer cargo fd 0.1.0 config/Config#full_path_base()."),
            "full_path_base"
        );
        assert_eq!(
            fallback_display_name(
                "scip-python python click 0 `examples.aliases.aliases`/Config#add_alias()."
            ),
            "add_alias"
        );
        // A macro (`!` suffix), a parameter, and a local symbol.
        assert_eq!(
            fallback_display_name("rust-analyzer cargo grep 0.1.0 macros/assert_eq_printed!"),
            "assert_eq_printed"
        );
        assert_eq!(
            fallback_display_name("scip-python python click 0 `x`/Config#add_alias().(self)"),
            "self"
        );
        assert_eq!(fallback_display_name("local 5"), "5");
        // An id the SCIP grammar does not accept keeps the old behaviour.
        assert_eq!(
            fallback_display_name("rust-analyzer cargo lib 0.1.0 mod/foo()"),
            "foo"
        );
    }

    #[test]
    fn symbol_names_follow_rust_analyzer_impl_blocks() {
        // rust-analyzer files impl members under a type descriptor named
        // `impl` plus type parameters: the self type, then the trait.
        let n = symbol_names("rust-analyzer cargo fd-find 10.5.0 walk/impl#[Batch]new().");
        assert_eq!(
            (n.name.as_str(), n.qualified.as_deref()),
            ("new", Some("Batch.new"))
        );
        let n = symbol_names(
            "rust-analyzer cargo fd-find 10.5.0 regex_helper/impl#[UppercaseVisitor][Visitor]finish().",
        );
        assert_eq!(
            (n.name.as_str(), n.qualified.as_deref()),
            ("finish", Some("UppercaseVisitor.finish"))
        );
        // Generic arguments of the self type are not part of the name.
        let n = symbol_names(
            "rust-analyzer cargo fd-find 10.5.0 walk/impl#[`ReceiverBuffer<'a, W>`]new().",
        );
        assert_eq!(n.qualified.as_deref(), Some("ReceiverBuffer.new"));
        // The impl block itself is not the type: it must not answer a lookup
        // of the type's name, and has no path of its own.
        let n = symbol_names("rust-analyzer cargo fd-find 10.5.0 walk/impl#[Batch]");
        assert_eq!((n.name.as_str(), n.qualified), ("impl Batch", None));
        let n = symbol_names(
            "rust-analyzer cargo fd-find 10.5.0 regex_helper/impl#[UppercaseVisitor][Visitor]",
        );
        assert_eq!(
            (n.name.as_str(), n.qualified),
            ("impl Visitor for UppercaseVisitor", None)
        );
    }

    #[test]
    fn symbol_names_qualified_drops_the_namespace() {
        let q = |id: &str| symbol_names(id).qualified;
        assert_eq!(
            q("rust-analyzer cargo fd 0.1.0 config/Config#full_path_base().").as_deref(),
            Some("Config.full_path_base")
        );
        assert_eq!(
            q("rust-analyzer cargo fd 0.1.0 config/Config#").as_deref(),
            Some("Config")
        );
        assert_eq!(
            q("scip-python python click 0 `examples.aliases.aliases`/Config#add_alias().")
                .as_deref(),
            Some("Config.add_alias")
        );
        assert_eq!(
            q("scip-java maven . . com/jayway/jsonpath/internal/JsonContext#pathFromCache().")
                .as_deref(),
            Some("JsonContext.pathFromCache")
        );
        // Only namespaces: the qualified name is the name.
        assert_eq!(
            q("rust-analyzer cargo fd 0.1.0 config/").as_deref(),
            Some("config")
        );
        // A local symbol has no qualified name: it is not addressable by a
        // path, and its id's number is not a name.
        let n = symbol_names("local 129");
        assert_eq!(n.name, "129");
        assert_eq!(n.qualified, None);
        // Not parseable: no qualified name, textual name.
        let n = symbol_names("rust-analyzer cargo lib 0.1.0 mod/foo()");
        assert_eq!(n.name, "foo");
        assert_eq!(n.qualified, None);
    }
}
