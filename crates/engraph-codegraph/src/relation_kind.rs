use std::str::FromStr;

/// Allowed values for `relations.kind` in the codegraph. Enforced in Rust
/// (the SQLite table does not carry a CHECK constraint — see v6 migration).
///
/// `BazelDependsOn` is the F2 Phase 2.3 edge type. Unlike the symbol-level
/// kinds, it connects entities whose `kind = 'bazel_target'` — coarse-grained
/// `//pkg:target` nodes rather than individual functions/classes. Both
/// granularities live in the same `relations` table; the kind discriminator
/// tells consumers which they're looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    Defines,
    References,
    Calls,
    Implements,
    Extends,
    Imports,
    BazelDependsOn,
}

impl RelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationKind::Defines => "DEFINES",
            RelationKind::References => "REFERENCES",
            RelationKind::Calls => "CALLS",
            RelationKind::Implements => "IMPLEMENTS",
            RelationKind::Extends => "EXTENDS",
            RelationKind::Imports => "IMPORTS",
            RelationKind::BazelDependsOn => "BAZEL_DEPENDS_ON",
        }
    }
}

impl FromStr for RelationKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "DEFINES" => Ok(RelationKind::Defines),
            "REFERENCES" => Ok(RelationKind::References),
            "CALLS" => Ok(RelationKind::Calls),
            "IMPLEMENTS" => Ok(RelationKind::Implements),
            "EXTENDS" => Ok(RelationKind::Extends),
            "IMPORTS" => Ok(RelationKind::Imports),
            "BAZEL_DEPENDS_ON" => Ok(RelationKind::BazelDependsOn),
            other => anyhow::bail!("unknown RelationKind: {other}"),
        }
    }
}
