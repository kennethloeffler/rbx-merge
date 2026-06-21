//! Conflict-resolution inputs: how the caller tells the merge which side to
//! take when it cannot decide automatically.
//!
//! A [`Resolutions`] carries an optional bulk default (take this side for every
//! conflict, e.g. [`Resolutions::take`]) plus per-conflict overrides keyed by
//! conflict kind, instance path, and property ([`Resolutions::resolve`]). Any
//! frontend — a CLI flag, an edited conflict report, a Studio plugin —
//! ultimately just builds one of these and hands it to the merge.
//!
//! Resolution is wired through the property-value, instance-identity,
//! parent-move, child-order, and delete/modify conflicts.

use std::collections::HashMap;

use crate::conflict::ConflictKind;

/// Which side of the merge to take for a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Base,
    Ours,
    Theirs,
}

type OverrideKey = (ConflictKind, String, Option<String>);

#[derive(Debug, Clone, Default)]
pub struct Resolutions {
    default: Option<Side>,
    overrides: HashMap<OverrideKey, Side>,
}

impl Resolutions {
    /// No resolutions: every conflict is reported as usual.
    pub fn none() -> Self {
        Self::default()
    }

    /// Take `side` for every otherwise-unresolved conflict.
    pub fn take(side: Side) -> Self {
        Self {
            default: Some(side),
            overrides: HashMap::new(),
        }
    }

    /// Take `side` for the specific conflict identified by kind, instance path,
    /// and property (use `None` for whole-instance conflicts). Overrides win
    /// over the bulk default.
    pub fn resolve(
        mut self,
        kind: ConflictKind,
        path: impl Into<String>,
        property: Option<String>,
        side: Side,
    ) -> Self {
        self.overrides.insert((kind, path.into(), property), side);
        self
    }

    pub(crate) fn lookup(
        &self,
        kind: &ConflictKind,
        path: &str,
        property: Option<&str>,
    ) -> Option<Side> {
        self.overrides
            .get(&(kind.clone(), path.to_owned(), property.map(str::to_owned)))
            .copied()
            .or(self.default)
    }
}
