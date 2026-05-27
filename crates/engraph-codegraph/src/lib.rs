pub mod bazel;
pub mod bazel_symbols;
pub mod driver;
pub mod index;
pub mod relation_kind;
pub mod scip_loader;
pub mod subgraph;

pub use bazel::{detect_bazel, index_bazel_workspace, BazelStats};
pub use bazel_symbols::{index_bazel_symbols, BazelSymbolStats, LangIndexResult, LangStatus};
pub use driver::{registry, Driver};
pub use index::{
    discover_workspace_repos, index_repo, index_workspace, IndexStats, WorkspaceRepoResult,
    WorkspaceStats,
};
pub use relation_kind::RelationKind;
pub use scip_loader::{load, LoadStats};
pub use subgraph::{format_markdown, subgraph_for, Neighborhood};
