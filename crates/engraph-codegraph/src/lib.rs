pub mod bazel;
pub mod bazel_symbols;
pub mod driver;
pub mod index;
pub mod relation_kind;
pub mod scip_loader;
pub mod subgraph;

pub use bazel::{BazelStats, detect_bazel, index_bazel_workspace};
pub use bazel_symbols::{BazelSymbolStats, LangIndexResult, LangStatus, index_bazel_symbols};
pub use driver::{Driver, registry};
pub use index::{
    IndexStats, WorkspaceRepoResult, WorkspaceStats, discover_workspace_repos, index_repo,
    index_workspace,
};
pub use relation_kind::RelationKind;
pub use scip_loader::{LoadStats, load};
pub use subgraph::{Neighborhood, format_markdown, subgraph_for};
