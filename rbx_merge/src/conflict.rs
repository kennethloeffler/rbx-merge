//! Conflict and result types surfaced to callers.

use crate::diagnostics::Diagnostic;

/// The full outcome of a merge. Unlike [`MergeResult`], a report always carries
/// diagnostics and conflicts together, and exposes the merged bytes (when the
/// merge was clean) through the same value — callers don't have to match on an
/// enum to read diagnostics.
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

    /// Lower this report into the legacy [`MergeResult`] enum.
    pub(crate) fn into_merge_result(self) -> MergeResult {
        match self.merged {
            Some(merged) if self.conflicts.is_empty() => MergeResult::Clean {
                merged,
                diagnostics: self.diagnostics,
            },
            _ => MergeResult::Conflicted {
                conflicts: self.conflicts,
                diagnostics: self.diagnostics,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    Clean {
        merged: Vec<u8>,
        diagnostics: Vec<Diagnostic>,
    },
    Conflicted {
        conflicts: Vec<Conflict>,
        diagnostics: Vec<Diagnostic>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    InstanceIdentity,
    UniqueIdCollision,
    DeleteModify,
    PropertyValue,
    ParentMove,
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
