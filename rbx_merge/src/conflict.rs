//! Conflict and result types surfaced to callers.

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
