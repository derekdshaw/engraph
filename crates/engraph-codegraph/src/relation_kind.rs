use std::str::FromStr;

/// Allowed values for `relations.kind` in the codegraph. Enforced in Rust
/// (the SQLite table does not carry a CHECK constraint — see v6 migration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    Defines,
    References,
    Calls,
    Implements,
    Extends,
    Imports,
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
            other => anyhow::bail!("unknown RelationKind: {other}"),
        }
    }
}
