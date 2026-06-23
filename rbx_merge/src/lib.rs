//! VCS-neutral semantic three-way merge backend for Roblox model and place
//! files. The pipeline is: decode bytes into a `semantic` model, match instance
//! identities across the three sides (`identity`), merge the matched graph
//! (`merge_graph`), and re-encode (`format`). Conflicts and `diagnostics` are
//! reported to the caller rather than written inline.
//!
//! Internally the crate is split into focused modules:
//!
//! - `format`: file-format detection and binary/XML decode/encode.
//! - `semantic`: the format-independent instance model and value-equality
//!   logic.
//! - `identity`: cross-side instance matching (which base/ours/theirs nodes are
//!   "the same").
//! - `merge_graph`: the three-way merge, child-order resolution,
//!   reference-target and unique-id checks, and lowering back to a `WeakDom`.
//! - `render`: per-value display strings and the deterministic [`textconv`]
//!   tree.
//! - `diagnostics` / `conflict`: the reported diagnostic and conflict/report
//!   types.
//!
//! The entry point is [`merge_files`], taking a side-specific [`FileInput`] per
//! side and returning a [`MergeReport`]. Conflict resolution is data-driven: a
//! caller supplies a [`Resolutions`] value telling the merge which side to take,
//! so any frontend — a CLI flag, an edited conflict report, a Studio plugin —
//! just builds one and hands it to [`merge_files`].

mod conflict;
mod diagnostics;
mod format;
mod identity;
mod merge_graph;
mod render;
mod resolve;
mod semantic;

use std::path::Path;

use thiserror::Error;

use crate::format::{decode_file, encode_file};
use crate::identity::build_identities;
use crate::merge_graph::{MergeOutcome, merge};
use crate::render::render_textconv;
use crate::semantic::{SemanticDom, SemanticInputs};

pub use crate::conflict::{
    Conflict, ConflictKind, ConflictKindParseError, DisplayValue, MergeReport,
};
pub use crate::diagnostics::{Diagnostic, DiagnosticSeverity};
pub use crate::format::{FileFormat, detect_format};
pub use crate::resolve::{Resolutions, Side};

use crate::diagnostics::metadata_diagnostic;

/// A single side of a merge. Each side carries its own bytes, an optional path
/// hint used for format detection and diagnostics, and an optional explicit
/// format that overrides detection.
#[derive(Debug, Clone, Copy)]
pub struct FileInput<'a> {
    pub bytes: &'a [u8],
    pub path_hint: Option<&'a Path>,
    pub format: Option<FileFormat>,
}

impl<'a> FileInput<'a> {
    /// Construct an input from raw bytes, leaving format detection to the path
    /// hint and content sniffing.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            path_hint: None,
            format: None,
        }
    }

    pub fn with_path_hint(mut self, path_hint: &'a Path) -> Self {
        self.path_hint = Some(path_hint);
        self
    }

    pub fn with_format(mut self, format: FileFormat) -> Self {
        self.format = Some(format);
        self
    }
}

/// Cross-cutting merge settings. Per-side formats live on each [`FileInput`];
/// this struct holds only options that apply to the merge as a whole.
#[derive(Debug, Clone, Default)]
pub struct MergeSettings {
    pub output_format: Option<FileFormat>,
    /// How to resolve conflicts the merge cannot settle automatically. Defaults
    /// to reporting every conflict ([`Resolutions::none`]).
    pub resolutions: Resolutions,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not detect Roblox file format{path}")]
    UnknownFormat { path: String },

    #[error("failed to decode {format}: {message}")]
    Decode { format: FileFormat, message: String },

    #[error("failed to encode {format}: {message}")]
    Encode { format: FileFormat, message: String },

    #[error("{0}")]
    Internal(String),
}

pub fn textconv(bytes: &[u8], path_hint: Option<&Path>) -> Result<String, Error> {
    let decoded = decode_file(bytes, path_hint, None)?;
    let semantic = SemanticDom::from_weak_dom(&decoded.dom)?;
    Ok(render_textconv(&semantic, decoded.format))
}

/// Three-way merge over side-specific [`FileInput`]s, returning a full
/// [`MergeReport`]. This is the crate's entry point.
pub fn merge_files(
    base: FileInput<'_>,
    ours: FileInput<'_>,
    theirs: FileInput<'_>,
    settings: MergeSettings,
) -> Result<MergeReport, Error> {
    let mut diagnostics = vec![metadata_diagnostic()];

    let base_file = decode_file(base.bytes, base.path_hint, base.format)?;
    let ours_file = decode_file(ours.bytes, ours.path_hint, ours.format)?;
    let theirs_file = decode_file(theirs.bytes, theirs.path_hint, theirs.format)?;
    let output_format = choose_output_format(
        ours.path_hint,
        settings.output_format,
        ours.format,
        ours_file.format,
    );

    let base_dom = SemanticDom::from_weak_dom(&base_file.dom)?;
    let ours_dom = SemanticDom::from_weak_dom(&ours_file.dom)?;
    let theirs_dom = SemanticDom::from_weak_dom(&theirs_file.dom)?;

    let (identities, identity_diagnostics) = build_identities(&base_dom, &ours_dom, &theirs_dom);
    diagnostics.extend(identity_diagnostics);

    let doms = SemanticInputs {
        base: &base_dom,
        ours: &ours_dom,
        theirs: &theirs_dom,
    };
    let dom = match merge(doms, &identities, &settings.resolutions, &mut diagnostics)? {
        MergeOutcome::Conflicts(conflicts) => {
            return Ok(MergeReport {
                merged: None,
                conflicts,
                diagnostics,
            });
        }
        MergeOutcome::Merged(dom) => dom,
    };

    let root_refs = dom.root().children().to_vec();
    let merged = encode_file(&dom, &root_refs, output_format)?;

    diagnostics.push(Diagnostic {
        severity: DiagnosticSeverity::Info,
        code: "output_format".to_owned(),
        message: format!("merged output encoded as {output_format}"),
        path: ours.path_hint.map(|path| path.display().to_string().into()),
    });

    Ok(MergeReport {
        merged: Some(merged),
        conflicts: Vec::new(),
        diagnostics,
    })
}

fn choose_output_format(
    path_hint: Option<&Path>,
    output_format: Option<FileFormat>,
    explicit_ours_format: Option<FileFormat>,
    detected_ours_format: FileFormat,
) -> FileFormat {
    output_format
        .or(explicit_ours_format)
        .or_else(|| path_hint.and_then(FileFormat::from_extension))
        .unwrap_or(detected_ours_format)
}

#[cfg(test)]
mod tests;
