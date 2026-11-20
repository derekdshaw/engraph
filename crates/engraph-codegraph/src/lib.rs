pub mod bazel;
pub mod bazel_symbols;
pub mod driver;
pub mod gc;
pub mod index;
pub mod lock;
pub mod relation_kind;
pub mod scip_loader;
pub mod subgraph;
#[cfg(test)]
mod test_env;
mod which;

pub use bazel::{BazelStats, detect_bazel, index_bazel_workspace};
pub use bazel_symbols::{
    BazelSymbolStats, LangIndexResult, LangStatus, SymbolLangPlan, index_bazel_symbols,
    plan_symbol_langs,
};
pub use driver::{Driver, NarrowRun, registry};
pub use gc::collect_orphans;
pub use index::{
    BaseState, DirOutcome, DiscoveredDir, Discovery, DiscoveryFilter, GoOptions, IndexMode,
    IndexPlan, IndexScope, IndexSource, IndexStats, MAX_NARROW_RUNS, RECURSIVE_MAX_DEPTH,
    SymbolLangSummary, WorkspacePlan, WorkspaceRepoResult, WorkspaceStats, base_checkout,
    base_state, discover_workspace_repos, index_modes, index_overlay, index_repo, index_repo_in,
    index_scip_manifest, index_workspace, indexed_at_by_subtree, indexed_commits, plan_repo,
    plan_workspace, record_base, record_indexed_commit, scip_output_dir, source_extensions,
    stale_normalization, workspace_rel,
};
pub use lock::{
    Acquire, IndexLock, acquire as acquire_index_lock, acquire_wait as acquire_index_lock_wait,
    held_message,
};
pub use relation_kind::RelationKind;
pub use scip_loader::{
    LoadStats, NORMALIZATION_VERSION, OverlayFile, OverlayState, OverlayStats, clear_overlay, load,
    load_overlay, overlay_state, record_overlay_state,
};
pub use subgraph::{
    MATCH_WHERE_SQL, Neighborhood, format_markdown, normalize_qualified, qualified_candidates,
    subgraph_for, subgraph_for_in,
};
