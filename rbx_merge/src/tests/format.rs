use std::path::Path;

use crate::{FileFormat, detect_format};

#[test]
fn extension_drives_detection() {
    assert_eq!(
        FileFormat::from_extension(Path::new("a/b.rbxm")),
        Some(FileFormat::BinaryModel)
    );
    assert_eq!(
        FileFormat::from_extension(Path::new("a/b.rbxl")),
        Some(FileFormat::BinaryPlace)
    );
    assert_eq!(
        FileFormat::from_extension(Path::new("a/b.rbxmx")),
        Some(FileFormat::XmlModel)
    );
    assert_eq!(
        FileFormat::from_extension(Path::new("a/b.rbxlx")),
        Some(FileFormat::XmlPlace)
    );
    assert_eq!(FileFormat::from_extension(Path::new("a/b.txt")), None);
}

#[test]
fn path_hint_wins_over_content() {
    // Binary magic, but the hint says XML place: the hint takes precedence.
    let format = detect_format(b"<roblox!", Some(Path::new("model.rbxlx")), None).unwrap();
    assert_eq!(format, FileFormat::XmlPlace);
}

#[test]
fn explicit_format_overrides_everything() {
    let format = detect_format(
        b"<roblox!",
        Some(Path::new("model.rbxlx")),
        Some(FileFormat::BinaryModel),
    )
    .unwrap();
    assert_eq!(format, FileFormat::BinaryModel);
}

#[test]
fn binary_magic_is_sniffed() {
    let format = detect_format(b"<roblox!\x89\xff", None, None).unwrap();
    assert_eq!(format, FileFormat::BinaryModel);
}

#[test]
fn xml_prologue_is_sniffed_after_whitespace() {
    let format = detect_format(b"  \n<?xml version=\"1.0\"?>", None, None).unwrap();
    assert_eq!(format, FileFormat::XmlModel);
}

#[test]
fn unknown_content_without_hint_errors() {
    let error = detect_format(b"not a roblox file", None, None).unwrap_err();
    assert!(matches!(error, crate::Error::UnknownFormat { .. }));
}
