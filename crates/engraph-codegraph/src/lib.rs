pub mod bazel;
pub mod bazel_symbols;
pub mod driver;
pub mod gc;
pub mod index;
pub mod lock;
pub mod relation_kind;
pub mod scip_loader;
pub mod subgraph;

pub use bazel::{BazelStats, detect_bazel, index_bazel_workspace};
pub use bazel_symbols::{
    BazelSymbolStats, LangIndexResult, LangStatus, SymbolLangPlan, index_bazel_symbols,
    plan_symbol_langs,
};
pub use driver::{Driver, registry};
pub use gc::collect_orphans;
pub use index::{
    IndexPlan, IndexScope, IndexStats, SymbolLangSummary, WorkspaceRepoResult, WorkspaceStats,
    discover_workspace_repos, index_repo, index_repo_in, index_scip_manifest, index_workspace,
    indexed_commits, plan_repo, plan_workspace, record_indexed_commit,
};
pub use lock::{Acquire, IndexLock, STALE_AFTER, acquire as acquire_index_lock, held_message};
pub use relation_kind::RelationKind;
pub use scip_loader::{LoadStats, load};
pub use subgraph::{Neighborhood, format_markdown, subgraph_for};
