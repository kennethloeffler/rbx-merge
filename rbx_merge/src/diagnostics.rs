//! Diagnostics emitted alongside merge and textconv results.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

pub(crate) fn metadata_diagnostic() -> Diagnostic {
    Diagnostic {
        severity: DiagnosticSeverity::Warning,
        code: "weak_dom_metadata".to_owned(),
        message: "WeakDom does not model every Roblox file-level metadata field; this prototype is semantic, not byte-perfect.".to_owned(),
        path: None,
    }
}

/// A property on the merged output that the reflection database does not know
/// about. It is preserved as-is, but flagged so callers can audit lossy or
/// format-specific round-tripping at a concrete location.
pub(crate) fn unknown_property_diagnostic(path: String, class: &str, property: &str) -> Diagnostic {
    Diagnostic {
        severity: DiagnosticSeverity::Info,
        code: "unknown_property".to_owned(),
        message: format!(
            "property {property:?} on class {class:?} is not in the reflection database; preserved as-is"
        ),
        path: Some(path),
    }
}

/// A reference that pointed at an instance in the base but resolves to nothing
/// in the merged output because that target was deleted. The reference is not a
/// conflict — nilling it is a reasonable outcome — but it is reported so the
/// drop is visible rather than silent.
pub(crate) fn dropped_reference_diagnostic(path: String, property: &str, target: &str) -> Diagnostic {
    Diagnostic {
        severity: DiagnosticSeverity::Warning,
        code: "dropped_reference".to_owned(),
        message: format!(
            "reference {property:?} was dropped to nil because its target {target} was deleted in the merge"
        ),
        path: Some(path),
    }
}

/// Several same-name, same-class siblings that lack a `UniqueId` and so have no
/// authoritative identity. They were paired across sides by position, which is
/// correct when they were left in place but may misattribute edits if the
/// siblings were reordered.
pub(crate) fn positional_identity_diagnostic(
    path: String,
    class: &str,
    name: &str,
    count: usize,
) -> Diagnostic {
    Diagnostic {
        severity: DiagnosticSeverity::Warning,
        code: "positional_identity".to_owned(),
        message: format!(
            "{count} same-named {name:?} ({class}) children without UniqueId were matched by position; identity may be wrong if siblings were reordered"
        ),
        path: Some(path),
    }
}

/// An added instance on the `theirs` side that could have matched more than one
/// added instance on `ours`. Matching is deterministic and conservative, so the
/// instance is treated as a distinct addition; this records that a guess was
/// declined so callers can review potential duplicates.
pub(crate) fn ambiguous_identity_diagnostic(path: String) -> Diagnostic {
    Diagnostic {
        severity: DiagnosticSeverity::Warning,
        code: "ambiguous_identity".to_owned(),
        message:
            "added instance matched multiple candidates across sides; treated as a distinct addition"
                .to_owned(),
        path: Some(path),
    }
}
