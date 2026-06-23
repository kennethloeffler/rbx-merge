//! Conflict and result types surfaced to callers.

use std::fmt;
use std::str::FromStr;

use crate::diagnostics::Diagnostic;

/// The full outcome of a merge: diagnostics and conflicts together, with the
/// merged bytes (when the merge was clean) exposed through the same value, so
/// callers don't have to match on an enum to read diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReport {
    /// Encoded merged file, present only when the merge was clean.
    pub merged: Option<Vec<u8>>,
    pub conflicts: Vec<Conflict>,
    pub diagnostics: Vec<Diagnostic>,
}

impl MergeReport {
    /// True when no conflicts were reported and merged output is available.
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty() && self.merged.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    InstanceIdentity,
    UniqueIdCollision,
    DeleteModify,
    PropertyValue,
    ParentMove,
    /// Two or more instances were independently reparented under one another, so
    /// the merged parent links form a cycle that no tree can satisfy. Neither
    /// side's tree has the cycle; it emerges only from combining their moves.
    ParentCycle,
    ChildOrder,
    RefTarget,
}

impl ConflictKind {
    /// The stable, canonical name of this kind, used in serialized conflict
    /// reports. [`Display`](fmt::Display) and [`FromStr`] are inverses over these
    /// names.
    fn name(&self) -> &'static str {
        match self {
            ConflictKind::InstanceIdentity => "InstanceIdentity",
            ConflictKind::UniqueIdCollision => "UniqueIdCollision",
            ConflictKind::DeleteModify => "DeleteModify",
            ConflictKind::PropertyValue => "PropertyValue",
            ConflictKind::ParentMove => "ParentMove",
            ConflictKind::ParentCycle => "ParentCycle",
            ConflictKind::ChildOrder => "ChildOrder",
            ConflictKind::RefTarget => "RefTarget",
        }
    }
}

impl fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Returned when a string does not name a known [`ConflictKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictKindParseError;

impl fmt::Display for ConflictKindParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown conflict kind")
    }
}

impl std::error::Error for ConflictKindParseError {}

impl FromStr for ConflictKind {
    type Err = ConflictKindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "InstanceIdentity" => ConflictKind::InstanceIdentity,
            "UniqueIdCollision" => ConflictKind::UniqueIdCollision,
            "DeleteModify" => ConflictKind::DeleteModify,
            "PropertyValue" => ConflictKind::PropertyValue,
            "ParentMove" => ConflictKind::ParentMove,
            "ParentCycle" => ConflictKind::ParentCycle,
            "ChildOrder" => ConflictKind::ChildOrder,
            "RefTarget" => ConflictKind::RefTarget,
            _ => return Err(ConflictKindParseError),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayValue {
    pub text: String,
}

impl DisplayValue {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub kind: ConflictKind,
    pub path: String,
    pub class: String,
    pub name: String,
    pub property: Option<String>,
    pub base: Option<DisplayValue>,
    pub ours: Option<DisplayValue>,
    pub theirs: Option<DisplayValue>,
}
