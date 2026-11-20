//! Decode a SCIP protobuf blob and load its symbols + occurrences into the
//! existing `entities` / `relations` tables.
//!
//! `entities.id` is the SCIP moniker after [`normalize_moniker`] (e.g.
//! `rust-analyzer cargo engraph-core 0.0.0 schema/run_migrations().`): every
//! id passes through the plan 05 rules before it reaches a row, so ids are
//! the same on each computer and after each commit (`normalize_index`).
//! Re-indexing is idempotent: we replace the
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
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
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

/// The version of the rules in [`normalize_moniker`]: `index_state` records
/// it with each run (`index::record_indexed_commit`), and `engraph index`
/// recommends a full index run of a project whose rows carry another version
/// (`index::stale_normalization`), since the ids of the two runs do not
/// connect. Bump it whenever a rule changes the ids it produces: the rules
/// here, and the scip-python project name (`driver::python_project_name`).
///
/// 1: R2 (project tag for `semanticdb` ids with the empty package name) and
///    R3 (the pinned version for `rust-analyzer`, `scip-typescript`, and
///    `semanticdb` ids), 2026-11-17.
/// 2: R1 gives the scip-python fallback name (no name in `pyproject.toml`) the
///    basename of the main worktree, not the basename of the checkout
///    directory, so every worktree of a repo writes one id (plan 10, part 1,
///    step 4), 2026-11-19.
pub const NORMALIZATION_VERSION: i64 = 2;

/// The id normalization rules of plan 05, part 2 (step 5), one `match` arm
/// per scheme, so a symbol id is the same on each computer and after each
/// commit. `project` is the project key of the load, for R2.
///
/// - R2: a `semanticdb` id with the empty package name `.` (scip-java writes
///   package symbols so: `semanticdb maven . . fixture/models/`) gets the
///   project tag as its version, so the package rows of two repos with the
///   same package names are two rows.
/// - R3: every other `semanticdb` id, and every `rust-analyzer` and
///   `scip-typescript` id, gets `PINNED_VERSION`: the manifest version a
///   release bumps, the JDK major of the computer, and the standard-library
///   URL all go, dependencies included, so a reference in one repo and the
///   definition in another stay one id.
/// - A `scip-go gomod` id with the version `.` gets `driver::SCIP_GO_VERSION`.
///   scip-go writes `.` for a module of the same repo that it cannot tie to
///   the indexed one (no `replace`, and the run does not start at the go.work
///   root), but the module's own run defines its symbols with the pinned
///   version; without the pin, the reference and the definition are two ids
///   and the relation ends at a placeholder. This rule predates the
///   versioned ones, so rows from before them already have it; a change to
///   it bumps `NORMALIZATION_VERSION` like any other rule.
/// - Other scip-go ids and all scip-python ids are already pinned by their
///   drivers and keep their dependency versions (a published Go module keeps
///   its real version); local ids and ids with fewer than four fields pass
///   through.
///
/// Call this everywhere a moniker enters the database so the rules cover
/// every entry point (steps 9 and 10).
pub fn normalize_moniker<'a>(s: &'a str, project: &str) -> Cow<'a, str> {
    let Some([scheme, manager, name, version, descriptors]) = id_fields(s) else {
        return Cow::Borrowed(s);
    };
    let pinned = match scheme {
        "semanticdb" if name == "." => project_tag(project),
        "semanticdb" | "rust-analyzer" | "scip-typescript" => {
            crate::driver::PINNED_VERSION.to_string()
        }
        "scip-go" if manager == "gomod" && version == "." => {
            crate::driver::SCIP_GO_VERSION.to_string()
        }
        _ => return Cow::Borrowed(s),
    };
    if version == pinned {
        return Cow::Borrowed(s);
    }
    Cow::Owned(format!("{scheme} {manager} {name} {pinned} {descriptors}"))
}

/// The five parts of a global SCIP id — scheme, package manager, package
/// name, version, and the descriptors — as raw slices: two spaces inside one
/// of the four fields are one escaped space and stay as they are, so the
/// parts join back into a valid id. `None` for an id with fewer than four
/// fields before the descriptors (`local 5`).
fn id_fields(s: &str) -> Option<[&str; 5]> {
    let bytes = s.as_bytes();
    let mut fields: Vec<&str> = Vec::with_capacity(4);
    let (mut start, mut i) = (0usize, 0usize);
    while i < bytes.len() && fields.len() < 4 {
        if bytes[i] == b' ' {
            if bytes.get(i + 1) == Some(&b' ') {
                i += 2;
                continue;
            }
            fields.push(&s[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    if fields.len() < 4 {
        return None;
    }
    Some([fields[0], fields[1], fields[2], fields[3], &s[start..]])
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

/// Every id the loader writes passes through [`normalize_moniker`] first
/// (plan 05, part 2, steps 9 and 10): one pass over the index, so the entity
/// upsert, the placeholder insert, and both ends of every relation all see
/// the normalized id, and no per-row write can miss the rules.
pub(crate) fn normalize_index(index: &mut Index, project: &str) {
    visit_symbols(index, |symbol| {
        if let Cow::Owned(normalized) = normalize_moniker(symbol, project) {
            *symbol = normalized;
        }
    });
}

/// The packages `index` defines symbols in that another project already
/// defines symbols in, each as `(package, those projects)`, the package being
/// the four fields before the descriptors. Two unrelated projects with one
/// package name write the same ids (the version is pinned), so the last load
/// takes over the rows they share — their `project` and `file_path`. Read it
/// after `normalize_index` and before the load writes anything.
fn shared_packages(
    conn: &PooledConn,
    index: &Index,
    project: &str,
) -> Result<Vec<(String, Vec<String>)>> {
    let mut packages = BTreeSet::new();
    for doc in &index.documents {
        for occ in &doc.occurrences {
            if occ.symbol.starts_with("local ")
                || !role_set(occ.symbol_roles, SymbolRole::Definition)
            {
                continue;
            }
            if let Some([scheme, manager, name, version, _]) = id_fields(&occ.symbol) {
                packages.insert(format!("{scheme} {manager} {name} {version} "));
            }
        }
    }
    // A placeholder (no file_path) is a reference, not a definition.
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT project FROM entities
         WHERE id >= ?1 AND id < ?2 AND project != ?3 AND file_path IS NOT NULL
         ORDER BY project",
    )?;
    let mut shared = Vec::new();
    for prefix in packages {
        // Every id that starts with `prefix`, as a range on the primary key:
        // `!` is the byte after the space that ends the prefix.
        let end = format!("{}!", &prefix[..prefix.len() - 1]);
        let others = stmt
            .query_map(rusqlite::params![prefix, end, project], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        if !others.is_empty() {
            shared.push((prefix.trim_end().to_string(), others));
        }
    }
    Ok(shared)
}

/// Apply `f` to every symbol id in the index: each document's symbols and
/// their relationships, each occurrence, and the external symbols and their
/// relationships.
fn visit_symbols(index: &mut Index, mut f: impl FnMut(&mut String)) {
    let info = |info: &mut scip::types::SymbolInformation, f: &mut dyn FnMut(&mut String)| {
        f(&mut info.symbol);
        for rel in &mut info.relationships {
            f(&mut rel.symbol);
        }
    };
    for doc in &mut index.documents {
        for i in &mut doc.symbols {
            info(i, &mut f);
        }
        for occ in &mut doc.occurrences {
            f(&mut occ.symbol);
        }
    }
    for i in &mut index.external_symbols {
        info(i, &mut f);
    }
}

/// The first 16 hex digits of the SHA-256 of a project key: a short, fixed
/// part of a local symbol id that differs between projects.
fn project_tag(project: &str) -> String {
    let mut hex = engraph_core::utils::sha256_hex(project.as_bytes());
    hex.truncate(16);
    hex
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

/// A file of a checkout whose content differs from the base commit, thus a
/// file of that checkout's overlay (plan 10, part 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayFile {
    /// Repo-relative, `/`-separated, as `entities.file_path` stores it.
    pub path: String,
    /// The checkout does not have the file, but the base does. It gets no
    /// overlay rows; the row in `overlay_files` hides the base rows.
    pub deleted: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OverlayStats {
    pub entities_inserted: usize,
    pub relations_inserted: usize,
    /// Documents of the SCIP blob that the overlay kept — the changed files.
    pub documents_loaded: usize,
    /// Files of the overlay, deleted ones included.
    pub files: usize,
}

/// Remove the overlay of `worktree`: its checkout matches the base, so every
/// lookup from it reads base rows. Its state row goes too — a later overlay
/// run records a fresh one.
pub fn clear_overlay(conn: &PooledConn, worktree: &str) -> Result<OverlayStats> {
    for sql in [
        "DELETE FROM overlay_entities WHERE worktree = ?1",
        "DELETE FROM overlay_relations WHERE worktree = ?1",
        "DELETE FROM overlay_files WHERE worktree = ?1",
        "DELETE FROM overlay_state WHERE worktree = ?1",
    ] {
        conn.execute(sql, [worktree])?;
    }
    Ok(OverlayStats::default())
}

/// What the overlay of a worktree was computed from (plan 10, part 3, step
/// 11): the checkout state, and a digest of the base commits it diffed
/// against. The staleness check of step 12 compares both with the state now.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayState {
    /// The tree hash of the checkout at overlay time — of the `git stash
    /// create` commit when the tree was dirty, else of HEAD. The tree, not
    /// the commit: a stash commit's id embeds the time it was made. `None`
    /// when git gave none, which counts as stale.
    pub checkout_tree: Option<String>,
    /// A digest of the `(subtree, commit)` rows of `index_state` the overlay
    /// diffed against; a base refresh changes it.
    pub base_digest: String,
    /// When the overlay was computed, as fractional epoch seconds, for the
    /// untracked-file mtime comparison — the same rule `index_state` follows.
    pub indexed_at: f64,
}

/// Record what the overlay of `worktree` was computed from. One row per
/// worktree: a later run replaces it.
pub fn record_overlay_state(
    conn: &PooledConn,
    worktree: &str,
    project: &str,
    checkout_tree: Option<&str>,
    base_digest: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO overlay_state (worktree, project, checkout_tree, base_digest, indexed_at)
         VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%d %H:%M:%f','now'))
         ON CONFLICT(worktree) DO UPDATE SET
             project = excluded.project,
             checkout_tree = excluded.checkout_tree,
             base_digest = excluded.base_digest,
             indexed_at = excluded.indexed_at",
        rusqlite::params![worktree, project, checkout_tree, base_digest],
    )?;
    Ok(())
}

/// The recorded state of `worktree`'s overlay, or `None` when no overlay run
/// recorded one.
pub fn overlay_state(conn: &PooledConn, worktree: &str) -> Result<Option<OverlayState>> {
    let mut stmt = conn.prepare(
        "SELECT checkout_tree, base_digest,
                (julianday(indexed_at) - 2440587.5) * 86400.0
         FROM overlay_state WHERE worktree = ?1",
    )?;
    let mut rows = stmt.query_map([worktree], |r| {
        Ok(OverlayState {
            checkout_tree: r.get(0)?,
            base_digest: r.get(1)?,
            indexed_at: r.get(2)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

/// Replace the overlay of `worktree` with the rows of `files` (plan 10, part
/// 3, step 10).
///
/// `scip_bytes` is the output of an index run in that checkout, with
/// repo-relative document paths. Only the documents of `files` are loaded:
/// the run analyses whole packages, because SCIP indexers do, but every
/// unchanged file keeps its base rows, which the view of the worktree reads.
/// A deleted file has no document and gets a row in `overlay_files` alone,
/// which is what hides its base rows.
///
/// The overlay of a checkout is replaced whole, not merged: `files` is what
/// differs from the base now, so a file that stopped differing must lose its
/// rows. One transaction, as the base load.
pub fn load_overlay(
    conn: &PooledConn,
    project: &str,
    worktree: &str,
    files: &[OverlayFile],
    scip_bytes: &[u8],
) -> Result<OverlayStats> {
    let mut index = Index::parse_from_bytes(scip_bytes).context("decoding SCIP protobuf")?;
    scope_local_symbols(&mut index, project);
    normalize_index(&mut index, project);

    let wanted: HashSet<&str> = files
        .iter()
        .filter(|f| !f.deleted)
        .map(|f| f.path.as_str())
        .collect();

    // The kind of every symbol, as the base load gathers it: the CALLS vs
    // REFERENCES choice reads the *target* symbol's kind, which often lives
    // in a document this overlay does not keep.
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
    let multi_document = MultiDocument::of(&index);
    let schemes = LoadSchemes::of(&index, &current_symbols);

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let mut guard = TxGuard { conn, done: false };
    for sql in [
        "DELETE FROM overlay_entities WHERE worktree = ?1",
        "DELETE FROM overlay_relations WHERE worktree = ?1",
        "DELETE FROM overlay_files WHERE worktree = ?1",
    ] {
        conn.execute(sql, [worktree])?;
    }
    {
        let mut ins = conn.prepare_cached(
            "INSERT INTO overlay_files (worktree, project, file_path, deleted)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for f in files {
            ins.execute(rusqlite::params![worktree, project, f.path, f.deleted])?;
        }
    }

    let sink = RowSink::Overlay { worktree };
    let mut stats = LoadStats::default();
    let mut documents_loaded = 0usize;
    for doc in &index.documents {
        if !wanted.contains(doc.relative_path.as_str()) {
            continue;
        }
        documents_loaded += 1;
        stats.documents_seen += 1;
        load_document(
            conn,
            project,
            doc,
            &sym_kind,
            &multi_document,
            &schemes,
            &sink,
            &mut stats,
        )?;
    }
    guard.commit()?;
    Ok(OverlayStats {
        entities_inserted: stats.entities_inserted,
        relations_inserted: stats.relations_inserted,
        documents_loaded,
        files: files.len(),
    })
}

pub fn load_with_options(
    conn: &PooledConn,
    project: &str,
    scip_bytes: &[u8],
    options: LoadOptions,
) -> Result<LoadStats> {
    let mut index = Index::parse_from_bytes(scip_bytes).context("decoding SCIP protobuf")?;
    scope_local_symbols(&mut index, project);
    normalize_index(&mut index, project);
    for (package, others) in shared_packages(conn, &index, project)? {
        tracing::warn!(
            project,
            package = %package,
            others = %others.join(", "),
            "another project defines symbols in this package too; their symbol ids are \
             the same, so this load takes over the rows they share",
        );
    }

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
    let multi_document = MultiDocument::of(&index);
    let schemes = LoadSchemes::of(&index, &current_symbols);

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
        load_document(
            conn,
            project,
            doc,
            &sym_kind,
            &multi_document,
            &schemes,
            &RowSink::Base,
            &mut stats,
        )?;
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

#[allow(clippy::too_many_arguments)]
fn load_document(
    conn: &PooledConn,
    project: &str,
    doc: &Document,
    sym_kind: &HashMap<String, SymKind>,
    multi_document: &MultiDocument,
    schemes: &LoadSchemes,
    sink: &RowSink<'_>,
    stats: &mut LoadStats,
) -> Result<()> {
    // Insert one entity per SymbolInformation in this doc. We capture
    // file_path + line_range from the first Definition occurrence we see for
    // the symbol below. Until then file_path = doc.relative_path,
    // line_range = NULL.
    let mut def_loc: HashMap<String, (i32, i32)> = HashMap::new(); // sym -> (start_line, end_line)
    // sym -> lines of the definition's `enclosing_range` (the whole function
    // or class, decorators included), when the indexer writes one.
    let mut def_scope: HashMap<String, (i32, i32)> = HashMap::new();
    let doc_scheme = document_scheme(doc).or_else(|| schemes.fallback(doc));
    let mut def_generated: HashMap<String, bool> = HashMap::new();

    for occ in &doc.occurrences {
        let roles = occ.symbol_roles;
        let is_def = role_set(roles, SymbolRole::Definition);
        if !is_def || occ.symbol.is_empty() {
            continue;
        }
        let (start_line, end_line) = decode_range(&occ.range);
        def_loc.insert(occ.symbol.clone(), (start_line, end_line));
        if !occ.enclosing_range.is_empty() {
            def_scope.insert(occ.symbol.clone(), decode_range(&occ.enclosing_range));
        }
        if role_set(roles, SymbolRole::Generated) {
            def_generated.insert(occ.symbol.clone(), true);
        }
    }

    // Insert/upsert one entity row per SymbolInformation in this document.
    // In the base, a conflict claims ownership: a real definition takes the
    // `project` away from any earlier placeholder. Phase 2.2 relies on that so
    // `entities.project` always names the *defining* repo, never the first
    // referrer. The name is replaced too: the existing row may be a
    // placeholder a reference wrote (`ensure_placeholder_entity`, which only
    // guesses from the id), or a row an older loader named differently. The
    // sink holds the statement for its table (`RowSink::entity`).
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
        let (file_path, line_range) = match multi_document.location(&s.symbol) {
            // A multi-document symbol gets its one canonical location,
            // whichever of its documents this is.
            Some((path, (start, end))) => (
                Some(path.clone()),
                Some(format!("{}:{}", start + 1, end + 1)),
            ),
            None => match def_loc.get(&s.symbol) {
                Some((start, end)) => (
                    Some(doc.relative_path.clone()),
                    Some(format!("{}:{}", start + 1, end + 1)),
                ),
                None => (Some(doc.relative_path.clone()), None),
            },
        };
        let signature = signature_text(s);
        sink.entity(
            conn,
            &EntityRow {
                id: &s.symbol,
                name: &display_name,
                qualified: names.qualified.as_deref(),
                project,
                file_path: file_path.as_deref(),
                line_range: line_range.as_deref(),
                signature: signature.as_deref(),
                scheme: symbol_scheme(&s.symbol, doc_scheme),
            },
        )?;
        stats.entities_inserted += 1;

        // Relationship-derived edges. is_implementation → IMPLEMENTS,
        // is_reference → EXTENDS only when not also is_implementation (avoids
        // double-counting; SCIP marks parent-class relationships as is_reference).
        for rel in &s.relationships {
            if rel.symbol.is_empty() {
                continue;
            }
            sink.placeholder(conn, &rel.symbol, project, doc_scheme)?;
            if rel.is_implementation {
                sink.relation(
                    conn,
                    &s.symbol,
                    &rel.symbol,
                    RelationKind::Implements,
                    "extracted",
                    None,
                )?;
                stats.relations_inserted += 1;
            } else if rel.is_reference {
                sink.relation(
                    conn,
                    &s.symbol,
                    &rel.symbol,
                    RelationKind::Extends,
                    "extracted",
                    None,
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
    // Anchors are tuples of (start_line, specificity, symbol, end_line). An
    // anchor with an `enclosing_range` starts at the start of that range and
    // ends at its end; one without has no end. When several
    // definitions start on the same line — common at the top of a file where
    // the `crate/` module def shares line 0 with the first function def —
    // ties are broken by specificity so a Function anchor wins over the
    // enclosing Module. Without this tiebreak the `nearest_enclosing` search
    // can attribute a call to `crate/` instead of `app_caller`. The symbol
    // breaks the remaining ties, so the choice does not depend on HashMap
    // order.
    let mut def_anchors: Vec<(i32, u8, String, Option<i32>)> = def_loc
        .iter()
        .filter_map(|(sym, (start, _))| {
            let kind = sym_kind.get(sym).copied();
            let spec = anchor_specificity(kind, sym);
            if spec == 0 {
                None
            } else {
                let scope = def_scope.get(sym);
                let start = scope.map_or(*start, |&(s, _)| s);
                Some((start, spec, sym.clone(), scope.map(|&(_, e)| e)))
            }
        })
        .collect();
    def_anchors.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    // An anchor comes from a definition occurrence, but the rows above come
    // from `doc.symbols`. A definition this document does not list (no
    // SymbolInformation anywhere, or, for a Go package, in a document loaded
    // later) needs a row before it can be a relation's source.
    let listed: HashSet<&str> = doc.symbols.iter().map(|s| s.symbol.as_str()).collect();
    let mut unlisted_with_row: HashSet<String> = HashSet::new();

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
        sink.placeholder(conn, &occ.symbol, project, doc_scheme)?;

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
        // The line is in this document, but `subgraph` shows it with the
        // anchor's one `file_path`. A Go package is defined in each of its
        // files, so its line would point into the wrong file.
        let line = (!multi_document.contains(&src_sym)).then_some(start_line + 1);
        if !listed.contains(src_sym.as_str()) && !unlisted_with_row.contains(&src_sym) {
            sink.placeholder(conn, &src_sym, project, doc_scheme)?;
            unlisted_with_row.insert(src_sym.clone());
        }
        sink.relation(conn, &src_sym, &occ.symbol, rel_kind, provenance, line)?;
        stats.relations_inserted += 1;
    }

    Ok(())
}

/// The symbols with a definition in more than one document (a Go package,
/// which each of its files defines), each with one canonical location: the
/// lexicographically smallest defining document path, and the smallest
/// definition range there. The entity row gets this location, and a relation
/// from such a source gets no line — otherwise both would follow the order of
/// the documents in the SCIP blob, which is the indexer's to change, so the
/// same repo could index differently on two machines.
struct MultiDocument {
    location: HashMap<String, (String, (i32, i32))>,
}

impl MultiDocument {
    fn of(index: &Index) -> Self {
        // What one pass over the definitions knows of a symbol, borrowed from
        // the index; only the multi-document symbols are copied out.
        struct Def<'a> {
            first_doc: usize,
            several: bool,
            best: (&'a str, (i32, i32)),
        }
        let mut defs: HashMap<&str, Def> = HashMap::new();
        for (i, doc) in index.documents.iter().enumerate() {
            for occ in &doc.occurrences {
                if occ.symbol.is_empty() || !role_set(occ.symbol_roles, SymbolRole::Definition) {
                    continue;
                }
                let here = (doc.relative_path.as_str(), decode_range(&occ.range));
                defs.entry(occ.symbol.as_str())
                    .and_modify(|d| {
                        d.several |= d.first_doc != i;
                        d.best = d.best.min(here);
                    })
                    .or_insert(Def {
                        first_doc: i,
                        several: false,
                        best: here,
                    });
            }
        }
        let location = defs
            .into_iter()
            .filter(|(_, d)| d.several)
            .map(
                |(
                    symbol,
                    Def {
                        best: (path, range),
                        ..
                    },
                )| { (symbol.to_string(), (path.to_string(), range)) },
            )
            .collect();
        Self { location }
    }

    fn contains(&self, symbol: &str) -> bool {
        self.location.contains_key(symbol)
    }

    fn location(&self, symbol: &str) -> Option<&(String, (i32, i32))> {
        self.location.get(symbol)
    }
}

fn nearest_enclosing(
    def_anchors: &[(i32, u8, String, Option<i32>)],
    occ_line: i32,
) -> Option<String> {
    // Find the anchor with maximum start_line ≤ occ_line, breaking line ties
    // by higher specificity. `def_anchors` is sorted (line ASC, spec ASC), so
    // a linear scan from the end picks the right one in O(n) — fine for
    // per-document anchor sets. An anchor whose range ended before the
    // occurrence does not contain it: module-level code after a Python
    // function, or a nested function's caller after the nested function,
    // goes to the next enclosing anchor, or to none.
    let mut best: Option<(i32, u8, &str)> = None;
    for (line, spec, sym, end) in def_anchors {
        if *line > occ_line {
            break;
        }
        if end.is_some_and(|e| e < occ_line) {
            continue;
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

/// One symbol row as `load_document` computes it, before a sink files it in
/// the base tables or in the overlay of a worktree.
pub(crate) struct EntityRow<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) qualified: Option<&'a str>,
    pub(crate) project: &'a str,
    pub(crate) file_path: Option<&'a str>,
    pub(crate) line_range: Option<&'a str>,
    pub(crate) signature: Option<&'a str>,
    pub(crate) scheme: Option<&'a str>,
}

/// Where a load files its rows. `load_document` computes the same rows for
/// both — the anchor heuristics, the relation kinds and the locations are one
/// implementation — and the sink decides the table (plan 10, part 3, step 10).
pub(crate) enum RowSink<'a> {
    /// `entities` and `relations`: the base graph of the project.
    Base,
    /// `overlay_entities` and `overlay_relations` of one checkout.
    Overlay { worktree: &'a str },
}

impl RowSink<'_> {
    /// Upsert the symbol row. The base claims ownership of a placeholder a
    /// reference wrote (`project`, and the name an older loader derived); the
    /// overlay row belongs to its worktree alone, so it replaces in place.
    fn entity(&self, conn: &PooledConn, row: &EntityRow<'_>) -> Result<()> {
        match self {
            RowSink::Base => {
                let mut stmt = conn.prepare_cached(
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
                stmt.execute(rusqlite::params![
                    row.id,
                    row.name,
                    row.project,
                    row.file_path,
                    row.line_range,
                    row.signature,
                    row.qualified,
                    row.scheme,
                ])?;
            }
            RowSink::Overlay { worktree } => {
                let mut stmt = conn.prepare_cached(
                    "INSERT INTO overlay_entities (worktree, id, kind, name, qualified_name, project, file_path, line_range, signature, scheme)
                     VALUES (?9, ?1, 'symbol', ?2, ?7, ?3, ?4, ?5, ?6, ?8)
                     ON CONFLICT(worktree, id) DO UPDATE SET
                        name = excluded.name,
                        qualified_name = excluded.qualified_name,
                        project = excluded.project,
                        scheme = COALESCE(excluded.scheme, overlay_entities.scheme),
                        file_path = COALESCE(excluded.file_path, overlay_entities.file_path),
                        line_range = COALESCE(excluded.line_range, overlay_entities.line_range),
                        signature = COALESCE(excluded.signature, overlay_entities.signature)",
                )?;
                stmt.execute(rusqlite::params![
                    row.id,
                    row.name,
                    row.project,
                    row.file_path,
                    row.line_range,
                    row.signature,
                    row.qualified,
                    row.scheme,
                    worktree,
                ])?;
            }
        }
        Ok(())
    }

    /// `line`: the 1-based line of the occurrence the edge comes from, in the
    /// document of `src`; `None` for an edge from a `SymbolInformation`
    /// relationship, which has no occurrence, and for a `src` defined in more
    /// than one document.
    fn relation(
        &self,
        conn: &PooledConn,
        src: &str,
        dst: &str,
        kind: RelationKind,
        provenance: &str,
        line: Option<i32>,
    ) -> Result<()> {
        let id = Uuid::now_v7().to_string();
        match self {
            RowSink::Base => {
                conn.execute(
                    "INSERT INTO relations (id, src_entity, dst_entity, kind, provenance, confidence, line)
                     VALUES (?1, ?2, ?3, ?4, ?5, 1.0, ?6)",
                    rusqlite::params![id, src, dst, kind.as_str(), provenance, line],
                )?;
            }
            RowSink::Overlay { worktree } => {
                conn.execute(
                    "INSERT INTO overlay_relations (worktree, id, src_entity, dst_entity, kind, provenance, confidence, line)
                     VALUES (?7, ?1, ?2, ?3, ?4, ?5, 1.0, ?6)",
                    rusqlite::params![id, src, dst, kind.as_str(), provenance, line, worktree],
                )?;
            }
        }
        Ok(())
    }

    /// A stub row for a symbol this load references but no document defines.
    ///
    /// The base needs one for every such symbol: `relations` has a foreign key
    /// to `entities`. The overlay has no foreign key (schema v20), and the
    /// view of a worktree falls back to the base rows, so it writes a stub
    /// only for a symbol the base does not have — a symbol this worktree
    /// introduced. Without that rule every external symbol an unchanged
    /// dependency names would be copied into every overlay.
    fn placeholder(
        &self,
        conn: &PooledConn,
        sym: &str,
        project: &str,
        doc_scheme: Option<&str>,
    ) -> Result<()> {
        match self {
            RowSink::Base => ensure_placeholder_entity(conn, sym, project, doc_scheme),
            RowSink::Overlay { worktree } => {
                let in_base: bool = conn
                    .prepare_cached("SELECT 1 FROM entities WHERE id = ?1")?
                    .exists([sym])?;
                if in_base {
                    return Ok(());
                }
                let names = symbol_names(sym);
                conn.execute(
                    "INSERT INTO overlay_entities (worktree, id, kind, name, qualified_name, project, scheme)
                     VALUES (?1, ?2, 'symbol', ?3, ?4, ?5, ?6)
                     ON CONFLICT(worktree, id) DO UPDATE SET
                        name = excluded.name,
                        qualified_name = excluded.qualified_name
                      WHERE overlay_entities.file_path IS NULL
                        AND (overlay_entities.name IS NOT excluded.name
                             OR overlay_entities.qualified_name IS NOT excluded.qualified_name)",
                    rusqlite::params![
                        worktree,
                        sym,
                        names.name,
                        names.qualified,
                        project,
                        symbol_scheme(sym, doc_scheme)
                    ],
                )?;
                Ok(())
            }
        }
    }
}

/// Insert a stub entity row for a referenced symbol that we haven't seen a
/// SymbolInformation for in this Index. Keeps FK satisfied; later runs that
/// do see the SymbolInformation will overwrite via ON CONFLICT.
///
/// Refreshes the name of an existing *placeholder* row (bug 9). For a symbol
/// no document here defines — every other crate's symbols, for which
/// rust-analyzer emits no SymbolInformation — this is the only statement that
/// ever touches the row, so a name an older engraph derived (`Result#`, before
/// part 1) would otherwise be permanent: the definition upsert never sees the
/// symbol, and the prune statements skip it because a relation references it.
///
/// `file_path IS NULL` keeps the part 1 step 4 invariant intact: a reference
/// never renames a symbol some document defines, and never takes its
/// `project`/`scheme`. The name comparison keeps the common case a no-op —
/// this runs once per reference *occurrence*, so every `.unwrap()` in the
/// codebase reaches the row of the same symbol.
fn ensure_placeholder_entity(
    conn: &PooledConn,
    sym: &str,
    project: &str,
    doc_scheme: Option<&str>,
) -> Result<()> {
    let names = symbol_names(sym);
    conn.execute(
        "INSERT INTO entities (id, kind, name, qualified_name, project, scheme)
         VALUES (?1, 'symbol', ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            qualified_name = excluded.qualified_name
          WHERE entities.file_path IS NULL
            AND (entities.name IS NOT excluded.name
                 OR entities.qualified_name IS NOT excluded.qualified_name)",
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

/// The schemes of one load, for the locals of a document that has no global
/// symbol of its own (`document_scheme` is `None`). Such locals would get
/// `scheme` NULL, which `IN_SCOPE` never matches, so no later load could
/// remove them — not even after the file is deleted. The fallback is the
/// load's only scheme when it has one; in a merged load (one indexer per
/// language), the scheme the other documents with the same file extension
/// carry. `None` when neither applies, which leaves the locals as before.
struct LoadSchemes {
    all: BTreeSet<String>,
    by_extension: HashMap<String, String>,
}

impl LoadSchemes {
    fn of(index: &Index, current_symbols: &HashSet<String>) -> Self {
        let all = current_symbols
            .iter()
            .filter_map(|s| scheme(s))
            .map(str::to_string)
            .collect();
        let mut by_extension = HashMap::new();
        for doc in &index.documents {
            if let (Some(s), Some(ext)) = (document_scheme(doc), extension(&doc.relative_path)) {
                by_extension
                    .entry(ext.to_string())
                    .or_insert_with(|| s.to_string());
            }
        }
        Self { all, by_extension }
    }

    fn fallback(&self, doc: &Document) -> Option<&str> {
        if self.all.len() == 1 {
            return self.all.iter().next().map(String::as_str);
        }
        self.by_extension
            .get(extension(&doc.relative_path)?)
            .map(String::as_str)
    }
}

fn extension(path: &str) -> Option<&str> {
    std::path::Path::new(path).extension()?.to_str()
}

/// The scheme (indexer) of a document: that of its first global symbol, in
/// its symbols, then in its occurrences (references to other packages carry
/// the indexer's scheme too). `None` when the document has only locals; the
/// loader then falls back to `LoadSchemes`.
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
        // scip-typescript gives a module (`…/`a.ts`/`) the kind File.
        Some(SymKind::Module)
        | Some(SymKind::Namespace)
        | Some(SymKind::Package)
        | Some(SymKind::File) => 10,
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

/// Last-resort name for an id the SCIP grammar rejects: the name of its last
/// descriptor, found without the grammar. The descriptors start after the
/// fourth space-separated field (scheme, package manager, package name,
/// package version), not at the last space: only those four fields escape a
/// literal space as two spaces (`accept_space_escaped_identifier`), and a
/// descriptor can hold a single one (``impl#[`Result<T, E>`]unwrap().``).
/// Then the trailing suffix punctuation goes (`().`, `#`, `!`, `:`), and the
/// text after the last descriptor separator is the name; an escaped name
/// (`` `weird.name` ``) is taken whole.
fn fallback_display_name_textual(moniker: &str) -> String {
    let trimmed = after_package_fields(moniker).trim_end_matches(['.', ')', '(', '#', '!', ':']);
    if let Some((_, name)) = trimmed
        .strip_suffix('`')
        .and_then(|inner| inner.rsplit_once('`'))
    {
        return name.to_string();
    }
    trimmed
        .rsplit(['/', '#', ']', '(', ')', '.', '!', ':'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

/// The text after the four space-separated fields of a SCIP id, where two
/// spaces together inside a field are one literal space. An id with fewer
/// fields (`local 5`) gives its last field.
fn after_package_fields(moniker: &str) -> &str {
    match id_fields(moniker) {
        Some([_, _, _, _, descriptors]) => descriptors,
        None => moniker.rsplit(' ').next().unwrap_or(moniker),
    }
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
    fn shared_packages_names_other_projects_defining_the_package() {
        // One document defining `defs` and referencing `refs`.
        let index_of = |defs: &[&str], refs: &[&str]| {
            let mut doc = Document::new();
            doc.relative_path = "src/lib.rs".into();
            for id in defs {
                let mut info = scip::types::SymbolInformation::new();
                info.symbol = id.to_string();
                info.kind = SymKind::Function.into();
                doc.symbols.push(info);
            }
            for (line, (id, role)) in defs
                .iter()
                .map(|d| (d, SymbolRole::Definition as i32))
                .chain(refs.iter().map(|r| (r, 0)))
                .enumerate()
            {
                let mut occ = scip::types::Occurrence::new();
                occ.symbol = id.to_string();
                occ.range = vec![line as i32, 0, 4];
                occ.symbol_roles = role;
                doc.occurrences.push(occ);
            }
            let mut index = Index::new();
            index.documents.push(doc);
            index
        };
        let dir = tempfile::tempdir().unwrap();
        let pool = engraph_core::db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let app = "rust-analyzer cargo app 0.1.0 main().";
        let load_index = |project: &str, index: Index| {
            load(&conn, project, &index.write_to_bytes().unwrap()).unwrap();
        };
        load_index("/a", index_of(&[app], &[]));
        // A package whose name starts with `app` is another package; a
        // reference to `app` (a placeholder row) is not a definition.
        load_index(
            "/d",
            index_of(&["rust-analyzer cargo app2 0.1.0 f()."], &[]),
        );
        load_index(
            "/c",
            index_of(&["rust-analyzer cargo c 0.1.0 g()."], &[app]),
        );

        let mut b = index_of(&[app, "rust-analyzer cargo b 0.2.0 h()."], &[]);
        normalize_index(&mut b, "/b");
        assert_eq!(
            shared_packages(&conn, &b, "/b").unwrap(),
            vec![(
                "rust-analyzer cargo app 0.0.0".to_string(),
                vec!["/a".to_string()]
            )]
        );
        // A project's own earlier rows are not another project's.
        let mut a = index_of(&[app], &[]);
        normalize_index(&mut a, "/a");
        assert!(shared_packages(&conn, &a, "/a").unwrap().is_empty());
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
    fn rule_r2_gives_empty_java_packages_the_project_tag() {
        let before = "semanticdb maven . . fixture/models/";
        let tag = project_tag("/home/me/repo");
        assert_eq!(
            normalize_moniker(before, "/home/me/repo"),
            format!("semanticdb maven . {tag} fixture/models/")
        );
        // Two repos with the same package names: two ids (the third test).
        assert_ne!(
            normalize_moniker(before, "/home/me/repo"),
            normalize_moniker(before, "/home/me/other")
        );
        // A JDK package placeholder gets the tag too; harmless.
        assert_eq!(
            normalize_moniker("semanticdb maven . . java/util/", "/home/me/repo"),
            format!("semanticdb maven . {tag} java/util/")
        );
    }

    #[test]
    fn rule_r3_pins_the_version_of_cargo_npm_and_maven_ids() {
        let p = "/proj";
        for (before, after) in [
            (
                "rust-analyzer cargo engraph-core 0.3.0 schema/run_migrations().",
                "rust-analyzer cargo engraph-core 0.0.0 schema/run_migrations().",
            ),
            (
                "rust-analyzer cargo core https://github.com/rust-lang/rust/library/core option/Option#",
                "rust-analyzer cargo core 0.0.0 option/Option#",
            ),
            (
                "scip-typescript npm engraph-fixture 0.1.0 src/`models.ts`/Book#",
                "scip-typescript npm engraph-fixture 0.0.0 src/`models.ts`/Book#",
            ),
            (
                "semanticdb maven maven/engraph/engraph-fixture 0.1.0 fixture/models/Book#",
                "semanticdb maven maven/engraph/engraph-fixture 0.0.0 fixture/models/Book#",
            ),
            (
                "semanticdb maven jdk 21 java/lang/String#",
                "semanticdb maven jdk 0.0.0 java/lang/String#",
            ),
            // An escaped space in a field, and a space in a descriptor, stay.
            (
                "rust-analyzer cargo my  pkg 1.0 result/impl#[`Result<T, E>`]unwrap().",
                "rust-analyzer cargo my  pkg 0.0.0 result/impl#[`Result<T, E>`]unwrap().",
            ),
        ] {
            assert_eq!(normalize_moniker(before, p), after, "{before}");
            // The rule is idempotent and does not allocate the second time.
            assert!(matches!(normalize_moniker(after, p), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn normalize_moniker_leaves_other_schemes_and_locals_alone() {
        for id in [
            "scip-go gomod github.com/x/y v1.2.3 `github.com/x/y`/F().",
            "scip-go gomod example.com/b 0.0.0 `example.com/b`/Helper().",
            "scip-python python requests 2.31.0 `requests.api`/get().",
            "scip-python python python 0.0.0 `engraph_fixture.models`/Book#",
            "local 5",
            "local 5@0123456789abcdef:src/a.rs",
            "not a scip id",
        ] {
            assert!(
                matches!(normalize_moniker(id, "/proj"), Cow::Borrowed(s) if s == id),
                "{id}"
            );
        }
    }

    #[test]
    fn id_fields_splits_the_four_fields_and_keeps_escapes() {
        assert_eq!(
            id_fields("rust-analyzer cargo my  pkg 1.0 mod/foo()."),
            Some(["rust-analyzer", "cargo", "my  pkg", "1.0", "mod/foo()."])
        );
        assert_eq!(id_fields("local 5"), None);
        // Four fields and no descriptors is not an id: the fourth field never
        // ends, so there are three.
        assert_eq!(id_fields("a b c d"), None);
        assert_eq!(id_fields("a b c d e"), Some(["a", "b", "c", "d", "e"]));
    }

    #[test]
    fn go_module_versions_pin_only_the_unknown_version() {
        let sibling = "scip-go gomod example.com/b . `example.com/b`/Helper().";
        let pinned = "scip-go gomod example.com/b 0.0.0 `example.com/b`/Helper().";
        let published = "scip-go gomod github.com/x/y v1.2.3 `github.com/x/y`/F().";
        // Another scheme's `.` version is not the Go rule's to pin.
        let other = "scip-python python a . a/f().";
        let mut info = scip::types::SymbolInformation::new();
        info.symbol = sibling.into();
        let mut rel = scip::types::Relationship::new();
        rel.symbol = sibling.into();
        info.relationships.push(rel);
        let mut doc = Document::new();
        doc.symbols.push(info);
        for symbol in [sibling, published, other] {
            let mut occ = scip::types::Occurrence::new();
            occ.symbol = symbol.into();
            doc.occurrences.push(occ);
        }
        let mut ext = scip::types::SymbolInformation::new();
        ext.symbol = sibling.into();
        let mut index = Index::new();
        index.documents.push(doc);
        index.external_symbols.push(ext);

        normalize_index(&mut index, "/proj");
        let doc = &index.documents[0];
        assert_eq!(doc.symbols[0].symbol, pinned);
        assert_eq!(doc.symbols[0].relationships[0].symbol, pinned);
        let occs: Vec<_> = doc.occurrences.iter().map(|o| o.symbol.as_str()).collect();
        assert_eq!(occs, [pinned, published, other]);
        assert_eq!(index.external_symbols[0].symbol, pinned);
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
    fn textual_fallback_name_is_the_last_descriptor_name() {
        // The descriptors follow four space-separated fields; a descriptor can
        // hold a space (a generic type), so the split must count fields, not
        // take the last space.
        assert_eq!(
            fallback_display_name_textual("x cargo core 1.0 result/impl#[`Result<T, E>`]unwrap()."),
            "unwrap"
        );
        assert_eq!(
            fallback_display_name_textual("x cargo alloc 1.0 vec/impl#[`Vec<T, A>`]push()."),
            "push"
        );
        assert_eq!(
            fallback_display_name_textual("x cargo fd 0.1.0 config/Config#full_path_base()."),
            "full_path_base"
        );
        // Two spaces inside one of the four fields are one literal space.
        assert_eq!(
            fallback_display_name_textual("x cargo my  pkg 1.0 mod/foo()."),
            "foo"
        );
        // An escaped last name keeps its dots; a type keeps nothing of `#`.
        assert_eq!(
            fallback_display_name_textual("x cargo lib 1.0 `weird.name`()."),
            "weird.name"
        );
        assert_eq!(
            fallback_display_name_textual("x cargo fd 0.1.0 config/Config#"),
            "Config"
        );
        assert_eq!(fallback_display_name_textual("local 5"), "5");
    }

    #[test]
    fn a_rejected_id_with_a_generic_type_gets_the_method_name() {
        // No trailing `.` after `()`: the SCIP grammar rejects the id, so the
        // name comes from the textual fallback.
        assert_eq!(
            fallback_display_name(
                "rust-analyzer cargo core 1.0 result/impl#[`Result<T, E>`]unwrap()"
            ),
            "unwrap"
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
